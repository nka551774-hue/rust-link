#!/usr/bin/env bash
# One-click start for a Rustlink node plus the example music bot (Linux/macOS).
#
# Asks for the node password, the Discord bot token and the port, writes them
# to quickstart/data (ignored by git), builds both programs, starts the node and
# then the bot. Ctrl+C stops both.
#
#   ./quickstart/start.sh
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(dirname "$HERE")
DATA=$HERE/data
CONFIG=$DATA/config.yaml
ENV_FILE=$DATA/.env

fail() { printf 'error: %s\n' "$*" >&2; exit 1; }
saved() { sed -n "s/^$1=//p" "$ENV_FILE" | head -1; }

# --- Settings ---------------------------------------------------------------

reuse=0
if [ -f "$CONFIG" ] && [ -f "$ENV_FILE" ]; then
    read -r -p 'Saved settings found. Use them? [Y/n] ' answer
    case $answer in n | N | no | NO) ;; *) reuse=1 ;; esac
fi

if [ "$reuse" = 1 ]; then
    PASSWORD=$(saved RUSTLINK_PASSWORD)
    TOKEN=$(saved DISCORD_BOT_TOKEN)
    PORT=$(saved RUSTLINK_URL | sed 's/.*://')
    [ -n "$PASSWORD" ] && [ -n "$TOKEN" ] && [ -n "$PORT" ] ||
        fail "the saved settings in $DATA are incomplete; delete that folder and start again"
else
    read -r -s -p 'Node password (empty = generate one): ' PASSWORD; echo
    if [ -z "$PASSWORD" ]; then
        PASSWORD=$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')
        echo "Generated password: $PASSWORD"
    fi
    case $PASSWORD in *[\"\\]* | *[[:space:]]*) fail 'the password cannot contain quotes, backslashes or spaces' ;; esac
    [ ${#PASSWORD} -ge 8 ] || fail 'the password needs at least 8 characters'

    read -r -s -p 'Discord bot token: ' TOKEN; echo
    [ -n "$TOKEN" ] || fail 'a Discord bot token is required (Developer Portal -> Bot -> Reset Token)'
    case $TOKEN in *[[:space:]]*) fail 'the token cannot contain spaces' ;; esac

    read -r -p 'Port for the node [2333]: ' PORT
    PORT=${PORT:-2333}
    case $PORT in '' | *[!0-9]*) fail 'the port must be a number' ;; esac
    [ "$PORT" -ge 1 ] && [ "$PORT" -le 65535 ] || fail 'the port must be between 1 and 65535'

    # YouTube refuses many server addresses ("Sign in to confirm you're not a
    # bot"); a proxy such as Cloudflare WARP gives it another one. A WARP proxy
    # already listening on its default port is offered as the default.
    warp=
    if (exec 3<>/dev/tcp/127.0.0.1/40000) 2>/dev/null; then warp=socks5://127.0.0.1:40000; fi
    read -r -p "YouTube proxy, e.g. socks5://127.0.0.1:40000 (empty = none) [$warp]: " PROXY
    PROXY=${PROXY:-$warp}
    case $PROXY in *[\"\\]* | *[[:space:]]*) fail 'the proxy cannot contain quotes, backslashes or spaces' ;; esac
    case $PROXY in '' | http://* | socks4://* | socks5://* | socks5h://*) ;;
        *) fail 'the proxy must start with http://, socks4://, socks5:// or socks5h://' ;; esac

    umask 077
    mkdir -p "$DATA"
    chmod 700 "$DATA"
    {
        printf '# Written by quickstart/start.sh. Run it again to change these.\n'
        printf 'address: 127.0.0.1:%s\n' "$PORT"
        printf 'password: "%s"\n' "$PASSWORD"
        printf 'logLevel: info\n'
        printf 'logDirectory: logs\n'
        printf 'toolsDirectory: .tools\n'
        printf 'autoDownload: true\n'
        if [ -n "$PROXY" ]; then
            printf 'youtubeProxy: "%s"\n' "$PROXY"
        fi
    } > "$CONFIG"
    {
        printf '# Written by quickstart/start.sh. Keep this file private.\n'
        printf 'DISCORD_BOT_TOKEN=%s\n' "$TOKEN"
        printf 'RUSTLINK_PASSWORD=%s\n' "$PASSWORD"
        printf 'RUSTLINK_URL=http://127.0.0.1:%s\n' "$PORT"
    } > "$ENV_FILE"
    echo "Settings saved to $DATA"
fi

# The files are the source of truth: a variable left in this shell would
# otherwise override them.
export RUSTLINK_PASSWORD=$PASSWORD RUSTLINK_ADDRESS=127.0.0.1:$PORT
export DISCORD_BOT_TOKEN=$TOKEN RUSTLINK_URL=http://127.0.0.1:$PORT

# --- Build ------------------------------------------------------------------

NODE=$ROOT/target/release/rustlink
BOT=$ROOT/target/release/examples/music
if command -v cargo >/dev/null 2>&1; then
    echo 'Building (the first build takes a few minutes)...'
    (cd "$ROOT" && cargo build --release --locked -p rustlink --bin rustlink) ||
        fail 'building the node failed (it needs pkg-config and libopus; see install.sh)'
    (cd "$ROOT" && cargo build --release --locked -p rustlink-discord-bot --example music) ||
        fail 'building the music bot failed'
elif [ ! -x "$NODE" ] || [ ! -x "$BOT" ]; then
    fail 'Rust is not installed; get it from https://rustup.rs and run this again'
fi

# --- Run --------------------------------------------------------------------

NODE_LOG=$DATA/node.out.log
NODE_PID=
stop_node() {
    if [ -n "$NODE_PID" ] && kill -0 "$NODE_PID" 2>/dev/null; then
        echo 'Stopping the node...'
        kill "$NODE_PID" 2>/dev/null || true
        wait "$NODE_PID" 2>/dev/null || true
    fi
}
trap stop_node EXIT
trap 'exit 130' INT TERM

echo "Starting the node on 127.0.0.1:$PORT (output: $NODE_LOG)..."
(cd "$DATA" && exec "$NODE" --config config.yaml) > "$NODE_LOG" 2>&1 &
NODE_PID=$!

# The first start downloads FFmpeg before it listens, so allow a while.
ready=0
for _ in $(seq 300); do
    if ! kill -0 "$NODE_PID" 2>/dev/null; then
        tail -n 15 "$NODE_LOG" >&2
        fail "the node stopped; is port $PORT already in use?"
    fi
    if command -v curl >/dev/null 2>&1; then
        if curl -fsS --max-time 2 "http://127.0.0.1:$PORT/version" >/dev/null 2>&1; then
            ready=1
            break
        fi
    elif (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then
        ready=1
        break
    fi
    sleep 1
done
[ "$ready" = 1 ] || fail 'the node did not answer within 5 minutes'
echo 'Node is ready. Starting the music bot (Ctrl+C stops everything)...'

# The bot exits when Discord drops its connection, so it is started again;
# one that keeps failing at once (a wrong token) is not retried forever.
quick=0
while true; do
    started=$(date +%s)
    status=0
    (cd "$DATA" && "$BOT" .env) || status=$?
    [ "$status" = 0 ] && break
    kill -0 "$NODE_PID" 2>/dev/null || fail 'the node stopped'
    if [ $(($(date +%s) - started)) -lt 30 ]; then quick=$((quick + 1)); else quick=0; fi
    [ "$quick" -lt 3 ] || fail 'the music bot keeps failing at start; check the token and the message above'
    echo 'The music bot stopped; starting it again in 5 seconds...'
    sleep 5
done
