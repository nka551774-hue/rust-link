#!/usr/bin/env bash
# One-click Rustlink node + Discord music bot launcher (Linux/macOS).
# Supports interactive prompts, saved settings in quickstart/data, or non-interactive
# env vars (DISCORD_BOT_TOKEN, RUSTLINK_PASSWORD, RUSTLINK_PORT, RUSTLINK_YOUTUBE_PROXY).
set -euo pipefail

case $0 in */*) _dir=${0%/*} ;; *) _dir=. ;; esac
HERE=$(CDPATH='' cd -P -- "${_dir:-/}" && pwd)
ROOT=$(CDPATH='' cd -P -- "$HERE/.." && pwd)
DATA=$HERE/data
CONFIG=$DATA/config.yaml
ENV_FILE=$DATA/.env

fail() { printf 'error: %s\n' "$*" >&2; exit 1; }
as_root() {
    if [ "$(id -u)" -eq 0 ]; then "$@"; else
        command -v sudo >/dev/null 2>&1 || fail "install build dependencies as root, then run again"
        sudo "$@"
    fi
}
saved() {
    [ -f "$ENV_FILE" ] || return 0
    local v
    v=$(sed -n -e "s/^[[:space:]]*export[[:space:]]\+$1=/$1=/" -e "s/^[[:space:]]*$1=//p" "$ENV_FILE" | head -n 1)
    v=${v%$'\r'}
    case $v in \"*\") v=${v#\"}; v=${v%\"} ;; \'*\') v=${v#\'}; v=${v%\'} ;; esac
    printf '%s' "$v"
}
gen_password() {
    local pw
    pw=$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')
    echo "Generated password: $pw" >&2
    printf '%s' "$pw"
}

ENV_TOKEN=${DISCORD_BOT_TOKEN:-}
ENV_PASSWORD=${RUSTLINK_PASSWORD:-}
ENV_PORT=${RUSTLINK_PORT:-}
ENV_PROXY=${RUSTLINK_YOUTUBE_PROXY-__unset__}
ENV_POT=${RUSTLINK_YOUTUBE_PO_TOKEN-__unset__}
ENV_VD=${RUSTLINK_YOUTUBE_VISITOR_DATA-__unset__}

yaml_single() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/''/g")"; }

reuse=0
if [ -z "$ENV_TOKEN" ] && [ -f "$CONFIG" ] && [ -f "$ENV_FILE" ]; then
    read -r -p 'Saved settings found. Use them? [Y/n] ' answer || answer=
    case $answer in n | N | no | NO) ;; *) reuse=1 ;; esac
fi

if [ "$reuse" = 1 ]; then
    PASSWORD=$(saved RUSTLINK_PASSWORD)
    TOKEN=$(saved DISCORD_BOT_TOKEN)
    PORT=$(saved RUSTLINK_URL | sed -n -e 's|^[a-zA-Z][a-zA-Z0-9+.-]*://||' -e 's|/.*||' -e 's|^.*:\([0-9][0-9]*\)$|\1|p')
    case $PORT in '' | *[!0-9]*) PORT= ;; esac
    [ -n "$PASSWORD" ] && [ -n "$TOKEN" ] && [ -n "$PORT" ] ||
        fail "saved settings in $DATA are incomplete; delete that folder and run again"
else
    if [ -n "$ENV_TOKEN" ]; then
        TOKEN=$ENV_TOKEN
        PASSWORD=${ENV_PASSWORD:-$(saved RUSTLINK_PASSWORD)}
        [ -n "$PASSWORD" ] || PASSWORD=$(gen_password)
        PORT=${ENV_PORT:-2333}
        PROXY=; [ "$ENV_PROXY" = "__unset__" ] || PROXY=$ENV_PROXY
        PO_TOKEN=; [ "$ENV_POT" = "__unset__" ] || PO_TOKEN=$ENV_POT
        VISITOR_DATA=; [ "$ENV_VD" = "__unset__" ] || VISITOR_DATA=$ENV_VD
    else
        if [ -n "$ENV_PASSWORD" ]; then
            PASSWORD=$ENV_PASSWORD
        else
            read -r -s -p 'Node password (empty = generate one): ' PASSWORD; echo
        fi
        [ -n "$PASSWORD" ] || PASSWORD=$(gen_password)
        read -r -s -p 'Discord bot token: ' TOKEN; echo
        if [ -n "$ENV_PORT" ]; then
            PORT=$ENV_PORT
        else
            read -r -p 'Port for the node [2333]: ' PORT
            PORT=${PORT:-2333}
        fi
        if [ "$ENV_PROXY" != "__unset__" ]; then
            PROXY=$ENV_PROXY
        else
            warp=
            if (exec 3<>/dev/tcp/127.0.0.1/40000) 2>/dev/null; then warp=socks5://127.0.0.1:40000; fi
            read -r -p "YouTube proxy, e.g. socks5://127.0.0.1:40000 (empty = none) [$warp]: " PROXY
            PROXY=${PROXY:-$warp}
        fi
        if [ "$ENV_POT" != "__unset__" ]; then
            PO_TOKEN=$ENV_POT
        else
            read -r -p 'YouTube PO token (empty = none): ' PO_TOKEN
        fi
        if [ "$ENV_VD" != "__unset__" ]; then
            VISITOR_DATA=$ENV_VD
        elif [ -n "$PO_TOKEN" ]; then
            case $PO_TOKEN in \{* | *:*) VISITOR_DATA= ;; *) read -r -p 'YouTube visitorData (empty = auto): ' VISITOR_DATA ;; esac
        else
            VISITOR_DATA=
        fi
    fi

    case $PASSWORD in *[\"\\]* | *[[:space:]]*) fail 'password cannot contain quotes, backslashes or spaces' ;; esac
    [ ${#PASSWORD} -ge 8 ] || fail 'password needs at least 8 characters'
    [ -n "$TOKEN" ] || fail 'a Discord bot token is required'
    case $TOKEN in *[[:space:]]*) fail 'token cannot contain spaces' ;; esac
    case $PORT in '' | *[!0-9]*) fail 'port must be a number' ;; esac
    [ "$PORT" -ge 1 ] && [ "$PORT" -le 65535 ] || fail 'port must be between 1 and 65535'
    case $PROXY in *[\"\\]* | *[[:space:]]*) fail 'proxy cannot contain quotes, backslashes or spaces' ;; esac
    case $PROXY in '' | http://* | https://* | socks4://* | socks4a://* | socks5://* | socks5h://*) ;;
        *) fail 'proxy must start with http://, https://, socks4://, socks4a://, socks5:// or socks5h://' ;; esac

    umask 077
    mkdir -p "$DATA"
    chmod 700 "$DATA"
    {
        printf 'address: 127.0.0.1:%s\npassword: "%s"\nlogLevel: info\nlogDirectory: logs\ntoolsDirectory: .tools\nautoDownload: true\n' "$PORT" "$PASSWORD"
        [ -z "$PROXY" ] || printf 'youtubeProxy: "%s"\n' "$PROXY"
        [ -z "$PO_TOKEN" ] || printf 'youtubePoToken: %s\n' "$(yaml_single "$PO_TOKEN")"
        [ -z "$VISITOR_DATA" ] || printf 'youtubeVisitorData: %s\n' "$(yaml_single "$VISITOR_DATA")"
    } > "$CONFIG"
    printf 'DISCORD_BOT_TOKEN=%s\nRUSTLINK_PASSWORD=%s\nRUSTLINK_URL=http://127.0.0.1:%s\n' "$TOKEN" "$PASSWORD" "$PORT" > "$ENV_FILE"
    echo "Settings saved to $DATA"
fi

export RUSTLINK_PASSWORD="$PASSWORD" RUSTLINK_ADDRESS="127.0.0.1:$PORT"
export DISCORD_BOT_TOKEN="$TOKEN" RUSTLINK_URL="http://127.0.0.1:$PORT"
unset RUSTLINK_YOUTUBE_PO_TOKEN RUSTLINK_YOUTUBE_VISITOR_DATA RUSTLINK_YOUTUBE_PROXY

for p in /opt/homebrew/bin /usr/local/bin /usr/bin /bin; do
    [ -d "$p" ] && case :$PATH: in *:"$p":*) ;; *) PATH="$PATH:$p" ;; esac
done
# shellcheck source=/dev/null
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

TARGET_DIR=$ROOT/target
if [ -f "$ROOT/Cargo.toml" ]; then
    if ! command -v cc >/dev/null 2>&1 || ! command -v pkg-config >/dev/null 2>&1 ||
        { [ -z "${OPUS_LIB_DIR:-}" ] && ! pkg-config --exists opus 2>/dev/null; }; then
        [ "${RUSTLINK_SKIP_BOOTSTRAP:-0}" != 1 ] || fail "install a C compiler, pkg-config and libopus"
        echo 'Installing build dependencies...'
        if command -v apt-get >/dev/null 2>&1; then
            as_root apt-get update
            as_root apt-get install -y build-essential pkg-config libopus-dev ca-certificates curl
        elif command -v dnf >/dev/null 2>&1; then
            as_root dnf install -y gcc gcc-c++ make pkgconf-pkg-config opus-devel ca-certificates curl
        elif command -v pacman >/dev/null 2>&1; then
            as_root pacman -Syu --noconfirm --needed base-devel pkgconf opus ca-certificates curl
        elif command -v apk >/dev/null 2>&1; then
            as_root apk add build-base pkgconf opus-dev ca-certificates curl
        elif command -v brew >/dev/null 2>&1; then
            brew install opus pkgconf
        else
            fail "install a C compiler, pkg-config and libopus for your OS"
        fi
    fi
    if ! command -v cargo >/dev/null 2>&1; then
        [ "${RUSTLINK_SKIP_BOOTSTRAP:-0}" != 1 ] || fail "install Rust from https://rustup.rs"
        command -v curl >/dev/null 2>&1 || fail "curl is required to install Rust"
        curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
        # shellcheck source=/dev/null
        . "$HOME/.cargo/env"
    fi
    HOST=$(rustc -vV | sed -n 's/^host: //p')
    [ -n "$HOST" ] || fail "could not determine Rust host target"
    echo 'Building node and music bot (first build takes a few minutes)...'
    (cd "$ROOT" && cargo build --release --locked --target "$HOST" --target-dir "$TARGET_DIR" -p rustlink --bin rustlink -p rustlink-discord-bot --example music)
    NODE=$TARGET_DIR/$HOST/release/rustlink
    BOT=$TARGET_DIR/$HOST/release/examples/music
else
    NODE=$TARGET_DIR/release/rustlink
    BOT=$TARGET_DIR/release/examples/music
    [ -x "$NODE" ] && [ -x "$BOT" ] || fail "Cargo.toml or prebuilt binaries are required"
fi

NODE_LOG=$DATA/node.out.log
NODE_PID=
stop_node() {
    if [ -n "$NODE_PID" ] && kill -0 "$NODE_PID" 2>/dev/null; then
        kill "$NODE_PID" 2>/dev/null || true
        wait "$NODE_PID" 2>/dev/null || true
    fi
}
trap stop_node EXIT
trap 'exit 130' INT TERM

echo "Starting node on 127.0.0.1:$PORT (log: $NODE_LOG)..."
(cd "$DATA" && exec "$NODE" --config config.yaml) > "$NODE_LOG" 2>&1 &
NODE_PID=$!

ready=0
for _ in $(seq 300); do
    kill -0 "$NODE_PID" 2>/dev/null || { tail -n 15 "$NODE_LOG" >&2; fail "node stopped; is port $PORT in use?"; }
    if command -v curl >/dev/null 2>&1; then
        curl --noproxy 127.0.0.1 -fsS --max-time 2 "http://127.0.0.1:$PORT/version" >/dev/null 2>&1 && { ready=1; break; }
    elif (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then
        ready=1; break
    fi
    sleep 1
done
[ "$ready" = 1 ] || fail 'node did not answer within 5 minutes'
echo 'Node ready. Starting music bot (Ctrl+C stops both)...'

quick=0
while true; do
    started=$(date +%s)
    status=0
    (cd "$DATA" && "$BOT" .env) || status=$?
    [ "$status" = 0 ] && break
    kill -0 "$NODE_PID" 2>/dev/null || fail 'the node stopped'
    if [ $(($(date +%s) - started)) -lt 30 ]; then quick=$((quick + 1)); else quick=0; fi
    [ "$quick" -lt 3 ] || fail 'music bot failed repeatedly at startup; check DISCORD_BOT_TOKEN'
    echo 'Music bot exited; restarting in 5s...'
    sleep 5
done
