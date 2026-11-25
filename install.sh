#!/usr/bin/env bash
# Rustlink VPS installer for the audio node and optional Discord music bot.
#
#   sudo ./install.sh
#   sudo DISCORD_BOT_TOKEN=... ./install.sh --with-bot
#   sudo RUSTLINK_PASSWORD=... ./install.sh --address 127.0.0.1 --native
set -euo pipefail

PROGRAM=${0##*/}
case $0 in */*) _dir=${0%/*} ;; *) _dir=. ;; esac
SOURCE=$(CDPATH='' cd -P -- "${_dir:-/}" && pwd)

DIRECTORY=/opt/rustlink
SERVICE_USER=rustlink
ADDRESS=0.0.0.0
PORT=2333
ADDRESS_SET=0
PORT_SET=0
PASSWORD=${RUSTLINK_PASSWORD:-}
BOT_TOKEN=${DISCORD_BOT_TOKEN:-}
WITH_BOT=0
NATIVE=0
BUILD=1
SERVICE=1
DEPS=1
UNINSTALL=0
GENERATED_PASSWORD=

usage() {
    printf '%s\n' \
        "Usage: sudo ./install.sh [options]" \
        "" \
        "  --dir PATH         install directory (default: /opt/rustlink)" \
        "  --user NAME        system service user (default: rustlink)" \
        "  --address ADDRESS  listen address (default: 0.0.0.0)" \
        "  --port PORT        listen port (default: 2333)" \
        "  --password VALUE   node password (prefer RUSTLINK_PASSWORD env var)" \
        "  --bot-token VALUE  Discord bot token; implies --with-bot (prefer DISCORD_BOT_TOKEN)" \
        "  --with-bot         build and deploy rustlink-bot.service alongside rustlink.service" \
        "  --native           build with -C target-cpu=native" \
        "  --no-build         install existing binaries from target/release" \
        "  --no-service       install files without touching systemd" \
        "  --no-deps          skip package manager and rustup installation" \
        "  --uninstall        stop and remove systemd unit(s), preserving /opt/rustlink data" \
        "  -h, --help         show this help"
}

step() { printf '\n==> %s\n' "$*"; }
note() { printf '    %s\n' "$*"; }
die() { printf '%s: %s\n' "$PROGRAM" "$*" >&2; exit 1; }
sed_replacement() { printf '%s' "$1" | sed 's/[\\&|]/\\&/g'; }

while [ $# -gt 0 ]; do
    case $1 in
        --dir) [ $# -ge 2 ] || die "--dir needs a path"; DIRECTORY=$2; shift 2 ;;
        --user) [ $# -ge 2 ] || die "--user needs a name"; SERVICE_USER=$2; shift 2 ;;
        --address) [ $# -ge 2 ] || die "--address needs a value"; ADDRESS=$2; ADDRESS_SET=1; shift 2 ;;
        --port) [ $# -ge 2 ] || die "--port needs a number"; PORT=$2; PORT_SET=1; shift 2 ;;
        --password) [ $# -ge 2 ] || die "--password needs a value"; PASSWORD=$2; shift 2 ;;
        --bot-token) [ $# -ge 2 ] || die "--bot-token needs a value"; BOT_TOKEN=$2; WITH_BOT=1; shift 2 ;;
        --with-bot) WITH_BOT=1; shift ;;
        --native) NATIVE=1; shift ;;
        --no-build) BUILD=0; shift ;;
        --no-service) SERVICE=0; shift ;;
        --no-deps) DEPS=0; shift ;;
        --uninstall) UNINSTALL=1; shift ;;
        -h | --help) usage; exit 0 ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
done

case $PORT in '' | *[!0-9]*) die "--port needs a number" ;; esac
[ "$PORT" -ge 1 ] && [ "$PORT" -le 65535 ] || die "--port must be between 1 and 65535"
case $ADDRESS in '' | *[[:space:]]*) die "--address cannot be empty or contain spaces" ;; esac
case $DIRECTORY in /*) ;; *) die "--dir needs an absolute path" ;; esac
case $DIRECTORY in *[[:space:]\|]*) die "--dir cannot contain spaces or a pipe" ;; esac
case ${DIRECTORY%/} in "" | /bin | /boot | /dev | /etc | /home | /lib | /lib64 | /opt | /proc | /root | /run | /sbin | /srv | /sys | /tmp | /usr | /usr/* | /var)
    die "--dir $DIRECTORY is a system directory; use a dedicated folder such as /opt/rustlink" ;;
esac
if [ "$SERVICE" = 1 ]; then
    case ${DIRECTORY%/} in /home/* | /root/*) die "--dir $DIRECTORY is hidden by ProtectHome=true; use /opt/rustlink or /srv/rustlink" ;; esac
fi
case $SERVICE_USER in '' | *[!A-Za-z0-9_-]*) die "--user needs an alphanumeric name" ;; esac
if [ -n "$PASSWORD" ]; then
    [ ${#PASSWORD} -ge 8 ] || die "--password needs at least 8 characters"
    case $(LC_ALL=C printf '%s' "$PASSWORD") in
        *[\"\\]* | *[![:graph:]]*) die "--password must contain printable ASCII without quotes, backslashes or spaces" ;;
    esac
fi
if [ -n "$BOT_TOKEN" ]; then
    case $(LC_ALL=C printf '%s' "$BOT_TOKEN") in
        *[![:graph:]]*) die "--bot-token cannot contain whitespace or non-ASCII characters" ;;
    esac
fi

case $ADDRESS in
    \[*\]) LISTEN_ADDR="$ADDRESS:$PORT" ;;
    *:*) LISTEN_ADDR="[$ADDRESS]:$PORT" ;;
    *) LISTEN_ADDR="$ADDRESS:$PORT" ;;
esac
case $ADDRESS in
    0.0.0.0 | :: | \[::\] | 127.0.0.1 | localhost) BOT_URL="http://127.0.0.1:$PORT" ;;
    \[*\]) BOT_URL="http://$ADDRESS:$PORT" ;;
    *:*) BOT_URL="http://[$ADDRESS]:$PORT" ;;
    *) BOT_URL="http://$ADDRESS:$PORT" ;;
esac

[ "$(id -u)" = 0 ] || die "run this installer as root (sudo ./install.sh)"

user_exists() { id -u "$1" >/dev/null 2>&1; }
group_exists() {
    if command -v getent >/dev/null 2>&1; then getent group "$1" >/dev/null 2>&1
    else grep -q "^$1:" /etc/group; fi
}
run_as_user() {
    if command -v runuser >/dev/null 2>&1; then runuser -u "$SERVICE_USER" -- "$@"
    elif command -v sudo >/dev/null 2>&1; then sudo -u "$SERVICE_USER" -- "$@"
    else su -s /bin/sh -c "$(printf '%q ' "$@")" "$SERVICE_USER"; fi
}

install_dependencies() {
    if command -v cargo >/dev/null 2>&1 &&
        { command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1; } &&
        pkg-config --exists opus 2>/dev/null; then
        note "build dependencies already installed"
        return
    fi
    step "Installing build dependencies and FFmpeg"
    if command -v apt-get >/dev/null 2>&1; then
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -qq
        apt-get install -y --no-install-recommends build-essential pkg-config libopus-dev git ca-certificates curl
        apt-get install -y --no-install-recommends ffmpeg || note "FFmpeg unavailable in apt; continuing without it"
    elif command -v dnf >/dev/null 2>&1; then
        dnf install -y gcc gcc-c++ make pkgconf-pkg-config opus-devel git ca-certificates curl
        dnf install -y ffmpeg || note "FFmpeg unavailable in dnf; continuing without it"
    elif command -v yum >/dev/null 2>&1; then
        yum install -y gcc gcc-c++ make pkgconf-pkg-config opus-devel git ca-certificates curl
        yum install -y ffmpeg || note "FFmpeg unavailable in yum; continuing without it"
    elif command -v zypper >/dev/null 2>&1; then
        zypper --non-interactive install --no-recommends gcc gcc-c++ make pkg-config libopus-devel git ca-certificates curl
        zypper --non-interactive install --no-recommends ffmpeg || note "FFmpeg unavailable in zypper; continuing without it"
    elif command -v apk >/dev/null 2>&1; then
        apk add --no-cache build-base pkgconf opus-dev git ca-certificates curl
        apk add --no-cache ffmpeg || note "FFmpeg unavailable in apk; continuing without it"
    elif command -v pacman >/dev/null 2>&1; then
        pacman -S --noconfirm --needed base-devel pkgconf opus git ca-certificates curl
        pacman -S --noconfirm --needed ffmpeg || note "FFmpeg unavailable in pacman; continuing without it"
    else
        note "no supported package manager detected; ensure a C toolchain, pkg-config and libopus are installed"
    fi
}

build_targets() {
    if ! command -v cargo >/dev/null 2>&1; then
        [ "$DEPS" = 1 ] || die "cargo is missing and --no-deps was specified"
        step "Installing Rust toolchain"
        export HOME=${HOME:-/root}
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
        # shellcheck disable=SC1091
        [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
    fi
    command -v cargo >/dev/null 2>&1 || die "cargo is not on PATH"

    local memory swap
    memory=$(awk '/^MemTotal:/ { print int($2 / 1024) }' /proc/meminfo 2>/dev/null || true)
    swap=$(awk '/^SwapTotal:/ { print int($2 / 1024) }' /proc/meminfo 2>/dev/null || printf '0')
    if [ -n "$memory" ] && [ "$memory" -lt 1500 ] && [ "${swap:-0}" -lt 256 ]; then
        note "warning: ${memory} MB RAM and no swap; add 2 GB swap if linking runs out of memory (see docs/vps.md)"
    fi

    local args=(build --release --locked --target-dir "$SOURCE/target" -p rustlink --bin rustlink)
    if [ "$INSTALL_BOT" = 1 ]; then
        step "Building Rustlink node and Discord music bot"
        args+=(-p rustlink-discord-bot --example music)
    else
        step "Building Rustlink node"
    fi
    if [ "$NATIVE" = 1 ]; then
        (cd "$SOURCE" && RUSTFLAGS="-C target-cpu=native" cargo "${args[@]}")
    else
        (cd "$SOURCE" && cargo "${args[@]}")
    fi
}

uninstall() {
    step "Removing systemd service(s)"
    if command -v systemctl >/dev/null 2>&1; then
        systemctl disable --now rustlink-bot rustlink 2>/dev/null || true
        rm -f /etc/systemd/system/rustlink-bot.service /etc/systemd/system/rustlink.service
        systemctl daemon-reload
    fi
    note "systemd unit(s) removed; data in $DIRECTORY was preserved"
    note "to remove all data: rm -rf $DIRECTORY && userdel $SERVICE_USER"
}

install_files() {
    step "Installing user, directory and binaries"
    if ! user_exists "$SERVICE_USER"; then
        command -v useradd >/dev/null 2>&1 && command -v groupadd >/dev/null 2>&1 ||
            die "useradd/groupadd missing (on Alpine: apk add shadow)"
        group_exists "$SERVICE_USER" || groupadd --system "$SERVICE_USER"
        local shell=/bin/false c
        for c in /usr/sbin/nologin /sbin/nologin; do [ -x "$c" ] && { shell=$c; break; }; done
        useradd --system --gid "$SERVICE_USER" --home-dir "$DIRECTORY" --shell "$shell" "$SERVICE_USER"
    fi
    SERVICE_GROUP=$(id -gn "$SERVICE_USER")
    install -d -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$DIRECTORY"
    install -m 0755 "$SOURCE/target/release/rustlink" "$DIRECTORY/rustlink"
    if [ "$INSTALL_BOT" = 1 ]; then
        install -m 0755 "$SOURCE/target/release/examples/music" "$DIRECTORY/rustlink-bot"
    fi
    install -m 0644 "$SOURCE/LICENSE" "$SOURCE/NOTICE" "$DIRECTORY/"
}

write_config() {
    step "Writing $DIRECTORY/config.yaml"
    local template="$SOURCE/config.example.yaml"
    local esc_listen esc_pw ffmpeg_val=ffmpeg auto_dl=true
    esc_listen=$(sed_replacement "$LISTEN_ADDR")
    esc_pw=$(sed_replacement "$PASSWORD")
    [ -x /usr/bin/ffmpeg ] && { ffmpeg_val=/usr/bin/ffmpeg; auto_dl=false; }
    (
        umask 0027
        if [ -f "$template" ]; then
            sed -e "s|^address:.*|address: $esc_listen|" \
                -e "s|^password:.*|password: \"$esc_pw\"|" \
                -e "s|^ffmpeg:.*|ffmpeg: $ffmpeg_val|" \
                -e "s|^autoDownload:.*|autoDownload: $auto_dl|" \
                "$template" >"$DIRECTORY/config.yaml"
        else
            printf 'address: %s\npassword: "%s"\nffmpeg: %s\nautoDownload: %s\nlogDirectory: logs\nlogRetentionDays: 14\n' \
                "$LISTEN_ADDR" "$PASSWORD" "$ffmpeg_val" "$auto_dl" >"$DIRECTORY/config.yaml"
        fi
    )
    chown "root:$SERVICE_GROUP" "$DIRECTORY/config.yaml"
    chmod 0640 "$DIRECTORY/config.yaml"
}

set_password() {
    local esc_pw
    esc_pw=$(sed_replacement "$PASSWORD")
    if grep -q '^password:' "$DIRECTORY/config.yaml"; then
        sed -i -e "s|^password:.*|password: \"$esc_pw\"|" "$DIRECTORY/config.yaml"
    else
        printf '\npassword: "%s"\n' "$PASSWORD" >>"$DIRECTORY/config.yaml"
    fi
    note "updated password in $DIRECTORY/config.yaml"
}

read_config_password() {
    sed -n -e 's/^password:[[:space:]]*"\(.*\)"[[:space:]]*$/\1/p' \
           -e "s/^password:[[:space:]]*'\(.*\)'[[:space:]]*$/\1/p" \
           -e 's/^password:[[:space:]]*\([^"'"'"'#[:space:]][^#[:space:]]*\).*$/\1/p' \
           "$DIRECTORY/config.yaml" | head -n 1
}

set_env_var() {
    local file=$1 key=$2 val=$3 esc
    esc=$(sed_replacement "$val")
    if grep -q "^${key}=" "$file"; then
        sed -i -e "s|^${key}=.*|${key}=${esc}|" "$file"
    else
        printf '%s=%s\n' "$key" "$val" >>"$file"
    fi
}

write_bot_env() {
    local bot_env="$DIRECTORY/bot.env" node_pw
    node_pw=${PASSWORD:-$(read_config_password)}
    [ -n "$node_pw" ] || die "could not determine node password from $DIRECTORY/config.yaml"
    if [ -f "$bot_env" ]; then
        note "preserving existing $bot_env"
        [ -z "$BOT_TOKEN" ] || set_env_var "$bot_env" DISCORD_BOT_TOKEN "$BOT_TOKEN"
        [ -z "$PASSWORD" ] || set_env_var "$bot_env" RUSTLINK_PASSWORD "$PASSWORD"
        if [ "$PORT_SET" = 1 ] || [ "$ADDRESS_SET" = 1 ] || ! grep -q '^RUSTLINK_URL=' "$bot_env"; then
            set_env_var "$bot_env" RUSTLINK_URL "$BOT_URL"
        fi
    else
        step "Writing $bot_env"
        (
            umask 0027
            printf 'DISCORD_BOT_TOKEN=%s\nRUSTLINK_PASSWORD=%s\nRUSTLINK_URL=%s\n' \
                "$BOT_TOKEN" "$node_pw" "$BOT_URL" >"$bot_env"
        )
    fi
    chown "root:$SERVICE_GROUP" "$bot_env"
    chmod 0640 "$bot_env"
}

render_unit() {
    local src=$1 dst=$2 esc_dir
    esc_dir=$(sed_replacement "$DIRECTORY")
    sed -e "s|/opt/rustlink|$esc_dir|g" \
        -e "s|^User=rustlink|User=$SERVICE_USER|" \
        -e "s|^Group=rustlink|Group=$SERVICE_GROUP|" \
        "$src" >"$dst"
    chmod 0644 "$dst"
}

install_units() {
    step "Installing systemd unit(s)"
    render_unit "$SOURCE/deploy/rustlink.service" /etc/systemd/system/rustlink.service
    if [ "$INSTALL_BOT" = 1 ]; then
        render_unit "$SOURCE/deploy/rustlink-bot.service" /etc/systemd/system/rustlink-bot.service
    fi
    systemctl daemon-reload
}

wait_active() {
    local unit=$1 i
    systemctl enable "$unit" >/dev/null 2>&1 || true
    systemctl restart "$unit"
    for i in 1 2 3 4 5 6 7 8 9 10; do
        sleep 1
        systemctl is-active --quiet "$unit" && return 0
    done
    journalctl -u "$unit" -n 40 --no-pager || true
    die "$unit failed to start"
}

machine_address() {
    local found=
    if command -v ip >/dev/null 2>&1; then
        found=$(ip route get 1.1.1.1 2>/dev/null | awk '{for(i=1;i<NF;i++) if($i=="src"){print $(i+1);exit}}') || found=
    fi
    [ -z "$found" ] && command -v hostname >/dev/null 2>&1 && found=$(hostname -I 2>/dev/null | awk '{print $1}' || true)
    printf '%s' "${found:-<server-ip>}"
}

summary() {
    step "Installed"
    printf '    node binary    %s/rustlink\n' "$DIRECTORY"
    printf '    node config    %s/config.yaml\n' "$DIRECTORY"
    printf '    listen         %s\n' "$LISTEN_ADDR"
    if [ "$INSTALL_BOT" = 1 ]; then
        printf '    bot binary     %s/rustlink-bot\n' "$DIRECTORY"
        printf '    bot config     %s/bot.env\n' "$DIRECTORY"
    fi
    if [ "$SERVICE" = 1 ]; then
        if [ "$INSTALL_BOT" = 1 ]; then
            printf '    logs           journalctl -u rustlink -u rustlink-bot -f\n'
            printf '    restart        systemctl restart rustlink rustlink-bot\n'
        else
            printf '    logs           journalctl -u rustlink -f\n'
            printf '    restart        systemctl restart rustlink\n'
        fi
    fi
    printf '    update         git pull && sudo ./install.sh\n'
    if [ -n "$GENERATED_PASSWORD" ]; then
        printf '\n    Generated node password: %s\n' "$GENERATED_PASSWORD"
    fi
    if [ "$INSTALL_BOT" = 0 ]; then
        case $ADDRESS in
            127.0.0.1 | ::1 | \[::1\] | localhost) ;;
            *)
                local host_addr
                host_addr=$(machine_address)
                case $host_addr in \<*\> | \[*\]) ;; *:*) host_addr="[$host_addr]" ;; esac
                printf '\n    Remote bot endpoint: http://%s:%s/api/v1\n' "$host_addr" "$PORT"
                printf '    Firewall example:    ufw allow from <bot-ip> to any port %s proto tcp\n' "$PORT"
                ;;
        esac
    fi
}

main() {
    if [ "$UNINSTALL" = 1 ]; then
        uninstall
        return
    fi
    [ -f "$SOURCE/Cargo.toml" ] || die "run from repository root ($SOURCE has no Cargo.toml)"
    [ -f "$SOURCE/deploy/rustlink.service" ] || die "deploy/rustlink.service is missing"

    INSTALL_BOT=0
    if [ -n "$BOT_TOKEN" ] || [ "$WITH_BOT" = 1 ] || [ -f "$DIRECTORY/bot.env" ]; then
        INSTALL_BOT=1
    fi
    if [ "$INSTALL_BOT" = 1 ]; then
        [ -f "$SOURCE/deploy/rustlink-bot.service" ] || die "deploy/rustlink-bot.service is missing"
        if [ -z "$BOT_TOKEN" ] && ! { [ -f "$DIRECTORY/bot.env" ] && grep -q '^DISCORD_BOT_TOKEN=..*' "$DIRECTORY/bot.env"; }; then
            if [ -t 0 ]; then
                read -r -s -p 'Discord bot token: ' BOT_TOKEN; echo
            fi
            [ -n "$BOT_TOKEN" ] || die "--with-bot requires DISCORD_BOT_TOKEN or --bot-token VALUE"
            case $(LC_ALL=C printf '%s' "$BOT_TOKEN") in
                *[![:graph:]]*) die "Discord bot token cannot contain whitespace or non-ASCII characters" ;;
            esac
        fi
    fi

    if [ "$SERVICE" = 1 ] && { ! command -v systemctl >/dev/null 2>&1 || [ ! -d /run/systemd/system ]; }; then
        die "systemd is not active; re-run with --no-service"
    fi
    [ "$BUILD" = 1 ] && [ "$DEPS" = 1 ] && install_dependencies
    [ "$BUILD" = 1 ] && build_targets
    [ -x "$SOURCE/target/release/rustlink" ] || die "missing $SOURCE/target/release/rustlink"
    if [ "$INSTALL_BOT" = 1 ]; then
        [ -x "$SOURCE/target/release/examples/music" ] || die "missing $SOURCE/target/release/examples/music"
    fi

    install_files
    if [ -f "$DIRECTORY/config.yaml" ]; then
        note "preserving existing $DIRECTORY/config.yaml"
        [ -z "$PASSWORD" ] || set_password
    else
        if [ -z "$PASSWORD" ]; then
            PASSWORD=$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')
            GENERATED_PASSWORD=$PASSWORD
        fi
        write_config
    fi
    [ "$INSTALL_BOT" = 1 ] && write_bot_env

    if [ "$SERVICE" = 1 ]; then
        install_units
        step "Checking node configuration"
        (cd "$DIRECTORY" && run_as_user "$DIRECTORY/rustlink" --config config.yaml --check) ||
            die "configuration check failed"
        step "Starting systemd service(s)"
        wait_active rustlink
        note "rustlink is active"
        if [ "$INSTALL_BOT" = 1 ]; then
            wait_active rustlink-bot
            note "rustlink-bot is active"
        fi
    fi
    summary
}

main
