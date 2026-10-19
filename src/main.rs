use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use inotify::{Inotify, WatchMask};
use rsbinder::{
    DeathRecipient, FLAG_CLEAR_BUF, Parcel, ProcessState, SHELL_COMMAND_TRANSACTION, SIBinder,
    WIBinder, hub,
};
use tokio::runtime::Builder;
use tokio::sync::mpsc;
use tokio::task;

const BLOCKLIST: &str = "/data/adb/sensor-blocker/packages.txt";
const PACKAGES_LIST: &str = "/data/system/packages.list";
const WATCHED_FILES: [&str; 2] = [PACKAGES_LIST, BLOCKLIST];

fn parse_packages(content: &str) -> anyhow::Result<HashMap<String, u32>> {
    let mut packages = HashMap::new();

    for (index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }

        let mut fields = line.split_ascii_whitespace();
        let Some(name) = fields.next() else {
            unreachable!();
        };
        let Some(uid) = fields.next() else {
            anyhow::bail!("packages.list line {} has no UID", index + 1);
        };

        packages.insert(
            name.to_owned(),
            uid.parse()
                .with_context(|| format!("invalid UID on packages.list line {}", index + 1))?,
        );
    }

    Ok(packages)
}

fn resolve_blacklist(content: &str, installed: &HashMap<String, u32>) -> HashMap<String, u32> {
    content
        .lines()
        .map(str::trim)
        .filter_map(|name| {
            installed
                .get(name)
                .filter(|uid| **uid >= 10_000)
                .map(|uid| (name.to_owned(), *uid))
        })
        .collect()
}

async fn write_command(service: &SIBinder, args: &[&str]) -> anyhow::Result<()> {
    let service = service.clone();
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();

    task::spawn_blocking(move || {
        let null = || OpenOptions::new().read(true).write(true).open("/dev/null");
        let mut parcel = Parcel::new();

        for fd in [null()?, null()?, null()?] {
            parcel.write_kernel_fd(fd.into())?;
        }

        parcel.write(&(args.len() as i32))?;

        for arg in args {
            parcel.write(arg.as_str())?;
        }

        parcel.write(&None::<SIBinder>)?;
        parcel.write(&None::<SIBinder>)?;
        service
            .as_proxy()
            .context("sensorservice is not a remote Binder service")?
            .submit_transact(SHELL_COMMAND_TRANSACTION, &parcel, FLAG_CLEAR_BUF)?;

        Ok(())
    })
    .await
    .context("joining sensorservice command task")?
}

#[derive(Debug)]
enum Event {
    FilesChanged,
    ServiceDied,
}

struct ServiceDeath(mpsc::UnboundedSender<Event>);

impl DeathRecipient for ServiceDeath {
    fn binder_died(&self, _who: &WIBinder) {
        let _ = self.0.send(Event::ServiceDied);
    }
}

fn watch_files(sender: mpsc::UnboundedSender<Event>) -> anyhow::Result<()> {
    let mut inotify = Inotify::init()?;
    let mask = WatchMask::CREATE | WatchMask::CLOSE_WRITE | WatchMask::MOVED_TO;
    let watched = WATCHED_FILES
        .iter()
        .map(|path| {
            let path = Path::new(path);
            Ok((
                inotify
                    .watches()
                    .add(path.parent().context("watched file has no parent")?, mask)?,
                path.file_name().context("watched file has no name")?,
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    sender.send(Event::FilesChanged)?;

    let mut buffer = [0; 4096];

    loop {
        let changed = inotify.read_events_blocking(&mut buffer)?.any(|event| {
            watched
                .iter()
                .any(|(descriptor, name)| &event.wd == descriptor && event.name == Some(*name))
        });

        if changed {
            std::thread::sleep(Duration::from_millis(100));

            loop {
                match inotify.read_events(&mut buffer) {
                    Ok(_) => {}
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    Err(error) => return Err(error.into()),
                }
            }

            if sender.send(Event::FilesChanged).is_err() {
                return Ok(());
            }
        }
    }
}

async fn apply_blacklist(
    service: &SIBinder,
    applied: &mut HashMap<String, u32>,
) -> anyhow::Result<()> {
    let installed = tokio::fs::read_to_string(PACKAGES_LIST).await?;
    let selected = tokio::fs::read_to_string(BLOCKLIST).await?;
    let installed = parse_packages(&installed)?;
    let selected = resolve_blacklist(&selected, &installed);

    for (name, uid) in applied.clone() {
        if selected.get(&name) == Some(&uid) {
            continue;
        }

        if installed.get(&name) == Some(&uid)
            && !selected.values().any(|wanted_uid| *wanted_uid == uid)
        {
            write_command(service, &["reset-uid-state", &name]).await?;
            eprintln!("reset {name} ({uid})");
        }

        applied.remove(&name);
    }

    let mut applied_uids: HashSet<u32> = applied.values().copied().collect();

    for (name, uid) in selected {
        if applied.get(&name) == Some(&uid) {
            continue;
        }

        if !applied_uids.contains(&uid) {
            write_command(service, &["set-uid-state", &name, "idle"]).await?;
        }

        eprintln!("idle {name} ({uid})");
        applied.insert(name, uid);
        applied_uids.insert(uid);
    }

    Ok(())
}

fn connect(
    sender: mpsc::UnboundedSender<Event>,
) -> anyhow::Result<(SIBinder, Arc<dyn DeathRecipient>)> {
    let service = hub::wait_for_service("sensorservice").context("waiting for sensorservice")?;
    let recipient: Arc<dyn DeathRecipient> = Arc::new(ServiceDeath(sender));

    service.link_to_death(Arc::downgrade(&recipient))?;
    eprintln!("sensorservice connected");

    Ok((service, recipient))
}

async fn connect_async(
    sender: mpsc::UnboundedSender<Event>,
) -> anyhow::Result<(SIBinder, Arc<dyn DeathRecipient>)> {
    task::spawn_blocking(move || connect(sender))
        .await
        .context("joining sensorservice connection task")?
}

async fn async_main() -> anyhow::Result<()> {
    let (sender, mut events) = mpsc::unbounded_channel();
    let watcher_sender = sender.clone();

    task::spawn_blocking(move || {
        if let Err(error) = watch_files(watcher_sender) {
            eprintln!("watching package files: {error:#}");
        }
    });

    let (mut service, mut recipient) = connect_async(sender.clone()).await?;
    let mut applied = HashMap::<String, u32>::new();

    while let Some(event) = events.recv().await {
        if matches!(event, Event::ServiceDied) {
            eprintln!("sensorservice disconnected");
            applied.clear();
            (service, recipient) = connect_async(sender.clone()).await?;
        }

        if let Err(error) = apply_blacklist(&service, &mut applied).await {
            eprintln!("applying package state: {error:#}");
        }
    }

    drop(recipient);
    Ok(())
}

fn main() -> anyhow::Result<()> {
    fs::create_dir_all("/data/adb/sensor-blocker")?;
    // Create the blocklist if missing without truncating an existing one.
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(BLOCKLIST)?;
    ProcessState::init_default()
        .map_err(|error| anyhow::anyhow!("initializing Binder: {error}"))?;
    ProcessState::start_thread_pool();
    Builder::new_current_thread()
        .build()?
        .block_on(async_main())
}

#[cfg(test)]
mod tests {
    use super::{parse_packages, resolve_blacklist};

    #[test]
    fn selects_only_installed_app_uids() {
        let installed =
            parse_packages("com.example.app 10123 0 /data\nandroid 1000 0 /system\n").unwrap();
        let selected = resolve_blacklist("com.example.app\nandroid\ncom.missing\n", &installed);

        assert_eq!(selected.len(), 1);
        assert_eq!(selected["com.example.app"], 10123);
    }

    #[test]
    fn rejects_an_invalid_packages_list() {
        assert!(parse_packages("com.example.app invalid\n").is_err());
        assert!(parse_packages("com.example.app\n").is_err());
    }
}
