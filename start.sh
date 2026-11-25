#!/usr/bin/env bash
set -euo pipefail
case ${BASH_SOURCE[0]} in */*) _dir=${BASH_SOURCE[0]%/*} ;; *) _dir=. ;; esac
RUSTLINK_ROOT=$(CDPATH='' cd -P -- "${_dir:-/}" && pwd)
cd "$RUSTLINK_ROOT"

fail() { printf 'Rustlink: %s\n' "$*" >&2; exit 1; }
system_install() {
    if [ "$(id -u)" -eq 0 ]; then "$@"; else
        command -v sudo >/dev/null 2>&1 || fail "Install build dependencies as root, then run start.sh again."
        sudo "$@"
    fi
}

if [ ! -f Cargo.toml ] && [ -x ./rustlink ]; then
    exec ./rustlink "$@"
fi
[ -f Cargo.toml ] || fail "Cargo.toml or a packaged rustlink executable is required."
for p in /opt/homebrew/bin /usr/local/bin /usr/bin /bin; do
    [ -d "$p" ] && case :$PATH: in *:"$p":*) ;; *) PATH="$PATH:$p" ;; esac
done
# shellcheck source=/dev/null
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

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
    elif command -v brew >/dev/null 2>&1; then
        brew install opus pkgconf
    else
        fail "Install a C compiler, pkg-config and libopus development files for your OS."
    fi
fi

if ! command -v cargo >/dev/null 2>&1; then
    [ "${RUSTLINK_SKIP_BOOTSTRAP:-0}" != 1 ] || fail "Install stable Rust from https://rustup.rs/ first."
    command -v curl >/dev/null 2>&1 || fail "curl is required to install Rust."
    curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
    # shellcheck source=/dev/null
    . "$HOME/.cargo/env"
fi

printf 'Building Rustlink (first build can take several minutes)...\n'
RUSTLINK_HOST=$(rustc -vV | sed -n 's/^host: //p')
[ -n "$RUSTLINK_HOST" ] || fail "Could not determine the Rust host target."
cargo build --release --locked --bin rustlink --target "$RUSTLINK_HOST" --target-dir "$RUSTLINK_ROOT/target"
exec "$RUSTLINK_ROOT/target/$RUSTLINK_HOST/release/rustlink" "$@"
