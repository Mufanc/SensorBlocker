#!/usr/bin/env just --justfile

TARGET := "aarch64-linux-android"
API := "33"
HOST := if os() == "macos" { "darwin-x86_64" } else { "linux-x86_64" }
NDK := env("ANDROID_NDK", env("ANDROID_HOME") / "ndk/29.0.14206865")
CC := NDK / "toolchains/llvm/prebuilt" / HOST / "bin" / (TARGET + API + "-clang")
PATCHED_RSBINDER := justfile_directory() / "target/rsbinder-patched"

check: _setup-rsbinder
    cargo fmt --check
    cargo clippy --target {{TARGET}} --config target.{{TARGET}}.linker='"{{CC}}"' --all-targets -- -D warnings

build: _setup-rsbinder
    cargo build --release --target {{TARGET}} --config target.{{TARGET}}.linker='"{{CC}}"'

package: build
    rm -rf target/module target/sensor-blocker.zip
    cp -R module target/module
    cp target/{{TARGET}}/release/sensor-blocker target/module/bin/
    rm target/module/bin/.keep
    cd target/module && zip -r ../sensor-blocker.zip .
    unzip -t target/sensor-blocker.zip

clean:
    rm -rf target

[private]
_setup-rsbinder:
    #!/bin/sh
    set -eu
    patched="{{PATCHED_RSBINDER}}"
    if [ ! -f "$patched/src/parcel.rs" ]; then
        (cd "${TMPDIR:-/tmp}" && cargo info rsbinder@0.12.0 >/dev/null)

        source=$(find "${CARGO_HOME:-$HOME/.cargo}/registry/src" -type d -name rsbinder-0.12.0 -print -quit)

        [ -n "$source" ] || { echo 'rsbinder 0.12.0 source not found' >&2; exit 1; }
        mkdir -p "$(dirname "$patched")"
        cp -R "$source" "$patched"
        patch -d "$patched" -p1 <patches/rsbinder-0.12.0.patch
    fi
