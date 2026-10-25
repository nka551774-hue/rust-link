#!/usr/bin/env bash
# Run from any working directory. Existing config files are never overwritten.
set -euo pipefail
RUSTLINK_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
cd "$RUSTLINK_ROOT"

fail() { printf 'Rustlink: %s\n' "$*" >&2; exit 1; }
system_install() {
    if [ "$(id -u)" -eq 0 ]; then "$@"; else
        command -v sudo >/dev/null 2>&1 || fail "Install build dependencies as root, then run start.sh again."
        sudo "$@"
    fi
}

# Release archives place the executable beside this script.
if [ ! -f Cargo.toml ] && [ -x ./rustlink ]; then
    exec ./rustlink "$@"
fi
[ -f Cargo.toml ] || fail "Cargo.toml or a packaged rustlink executable is required."
if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck source=/dev/null
    . "$HOME/.cargo/env"
fi

if ! command -v cc >/dev/null 2>&1 || ! command -v pkg-config >/dev/null 2>&1 ||
    { [ -z "${OPUS_LIB_DIR:-}" ] && ! pkg-config --exists opus 2>/dev/null; }; then
    [ "${RUSTLINK_SKIP_BOOTSTRAP:-0}" != 1 ] || fail "Install a C compiler, pkg-config and libopus development files."
    printf 'Installing Rustlink build dependencies...\n'
    if command -v apt-get >/dev/null 2>&1; then
        system_install apt-get update
        system_install apt-get install -y build-essential pkg-config libopus-dev ca-certificates curl
    elif command -v dnf >/dev/null 2>&1; then
        system_install dnf install -y gcc gcc-c++ make pkgconf-pkg-config opus-devel ca-certificates curl
    elif command -v pacman >/dev/null 2>&1; then
        system_install pacman -Syu --noconfirm --needed base-devel pkgconf opus ca-certificates curl
    elif command -v apk >/dev/null 2>&1; then
        system_install apk add build-base pkgconf opus-dev ca-certificates curl
    else
        fail "Install a C compiler, pkg-config and libopus development files for your distribution."
    fi
fi

if ! command -v cargo >/dev/null 2>&1; then
    [ "${RUSTLINK_SKIP_BOOTSTRAP:-0}" != 1 ] || fail "Install stable Rust from https://rustup.rs/ first."
    command -v curl >/dev/null 2>&1 || fail "curl is required to install Rust."
    installer=$(mktemp)
    trap 'rm -f -- "$installer"' EXIT
    curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o "$installer"
    sh "$installer" -y --profile minimal --default-toolchain stable
    # shellcheck source=/dev/null
    . "$HOME/.cargo/env"
    rm -f -- "$installer"
    trap - EXIT
fi

printf 'Building Rustlink (the first build can take several minutes)...\n'
# Explicit target and output directory keep external Cargo settings from hiding the executable.
RUSTLINK_HOST=$(rustc -vV | sed -n 's/^host: //p')
[ -n "$RUSTLINK_HOST" ] || fail "Could not determine the Rust host target."
cargo build --release --locked --bin rustlink --target "$RUSTLINK_HOST" --target-dir "$RUSTLINK_ROOT/target"
exec "$RUSTLINK_ROOT/target/$RUSTLINK_HOST/release/rustlink" "$@"
