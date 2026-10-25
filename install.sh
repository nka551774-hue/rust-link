#!/usr/bin/env bash
#
# Rustlink installer.
#
# It builds the node from this checkout and installs it the way docs/vps.md
# describes: a system user, one directory, the systemd unit, and a configuration
# written from config.example.yaml with a password of its own. Running it again
# is the update path: the configuration is kept, the binary and the unit are
# replaced, and the service is restarted.
#
#   sudo ./install.sh
#   sudo ./install.sh --address 127.0.0.1 --port 2333
#   sudo ./install.sh --native --password hunter2
#
set -euo pipefail

PROGRAM=$(basename "$0")
SOURCE=$(cd "$(dirname "$0")" && pwd)

DIRECTORY=/opt/rustlink
SERVICE_USER=rustlink
ADDRESS=0.0.0.0
PORT=2333
PASSWORD=
NATIVE=0
BUILD=1
SERVICE=1
DEPS=1
UNINSTALL=0
GENERATED_PASSWORD=

usage() {
    cat <<'TEXT'
Usage: sudo ./install.sh [options]

  --dir PATH         where the node is installed (default: /opt/rustlink)
  --user NAME        system user it runs as (default: rustlink)
  --address ADDRESS  listen address (default: 0.0.0.0, every interface)
  --port PORT        listen port (default: 2333)
  --password VALUE   password for clients (default: one is generated)
  --native           build with -C target-cpu=native
  --no-build         install the binary already in target/release
  --no-service       install the files without touching systemd
  --no-deps          do not install packages or a Rust toolchain
  --uninstall        stop and remove the service, leaving the data behind
  -h, --help         this text

A first run writes the configuration, creates the user and the directory,
installs the unit from deploy/rustlink.service, and starts the service. Later
runs keep the configuration and replace everything else.

The written configuration listens on every interface, because the client is
usually a bot on another machine; the firewall is what limits who reaches it.
Use --address 127.0.0.1 for a node behind a proxy that terminates TLS.
TEXT
}

step() { printf '\n==> %s\n' "$*"; }
note() { printf '    %s\n' "$*"; }
die() { printf '%s: %s\n' "$PROGRAM" "$*" >&2; exit 1; }
while [ $# -gt 0 ]; do
    case $1 in
        --dir) [ $# -ge 2 ] || die "--dir needs a path"; DIRECTORY=$2; shift 2 ;;
        --user) [ $# -ge 2 ] || die "--user needs a name"; SERVICE_USER=$2; shift 2 ;;
        --address) [ $# -ge 2 ] || die "--address needs a value"; ADDRESS=$2; shift 2 ;;
        --port) [ $# -ge 2 ] || die "--port needs a number"; PORT=$2; shift 2 ;;
        --password) [ $# -ge 2 ] || die "--password needs a value"; PASSWORD=$2; shift 2 ;;
        --native) NATIVE=1; shift ;;
        --no-build) BUILD=0; shift ;;
        --no-service) SERVICE=0; shift ;;
        --no-deps) DEPS=0; shift ;;
        --uninstall) UNINSTALL=1; shift ;;
        -h | --help) usage; exit 0 ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
done

# Everything below is written into a configuration file or a unit file, so the
# values are checked for the characters that would break either of them.
case $PORT in '' | *[!0-9]*) die "--port needs a number" ;; esac
[ "$PORT" -ge 1 ] && [ "$PORT" -le 65535 ] || die "--port must be between 1 and 65535"
case $ADDRESS in '' | *[[:space:]]*) die "--address cannot be empty or contain spaces" ;; esac
case $DIRECTORY in /*) ;; *) die "--dir needs an absolute path" ;; esac
case $DIRECTORY in *[[:space:]\|]*) die "--dir cannot contain spaces or a pipe" ;; esac
case $SERVICE_USER in '' | *[!A-Za-z0-9_-]*) die "--user needs a plain name" ;; esac
if [ -n "$PASSWORD" ]; then
    case $PASSWORD in *[\"\\]* | *[[:space:]]*) die "--password cannot contain quotes, backslashes or spaces" ;; esac
fi

[ "$(id -u)" = 0 ] || die "this installer needs root: run it again with sudo"

user_exists() { id -u "$1" >/dev/null 2>&1; }
group_exists() { grep -q "^$1:" /etc/group; }

# Runs a command as the service user, which is how the configuration is checked
# the way the service will read it.
run_as_user() {
    if command -v runuser >/dev/null 2>&1; then
        runuser -u "$SERVICE_USER" -- "$@"
    elif command -v sudo >/dev/null 2>&1; then
        sudo -u "$SERVICE_USER" -- "$@"
    else
        su -s /bin/sh -c "$(printf '%q ' "$@")" "$SERVICE_USER"
    fi
}

package_manager() {
    local candidate
    for candidate in apt-get dnf yum zypper apk pacman; do
        if command -v "$candidate" >/dev/null 2>&1; then
            printf '%s' "$candidate"
            return
        fi
    done
    printf 'none'
}

install_packages() {
    local manager=$1
    shift
    case $manager in
        apt-get)
            export DEBIAN_FRONTEND=noninteractive
            apt-get update -qq
            apt-get install -y --no-install-recommends "$@"
            ;;
        dnf | yum) "$manager" install -y "$@" ;;
        zypper) zypper --non-interactive install --no-recommends "$@" ;;
        apk) apk add --no-cache "$@" ;;
        pacman) pacman -Sy --noconfirm --needed "$@" ;;
        *) return 1 ;;
    esac
}

install_dependencies() {
    local manager build_packages
    # An update run on a machine that has already built the node should not
    # touch the package manager at all.
    if command -v cargo >/dev/null 2>&1 &&
        { command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1; } &&
        pkg-config --exists opus 2>/dev/null; then
        note "the build dependencies are already installed"
        return
    fi
    manager=$(package_manager)
    if [ "$manager" = none ]; then
        note "no package manager this script knows; install a C toolchain, pkg-config and libopus by hand"
        return
    fi
    case $manager in
        apt-get) build_packages="build-essential pkg-config libopus-dev git ca-certificates curl" ;;
        dnf | yum) build_packages="gcc gcc-c++ make pkgconf-pkg-config opus-devel git ca-certificates curl" ;;
        zypper) build_packages="gcc gcc-c++ make pkg-config libopus-devel git ca-certificates curl" ;;
        apk) build_packages="build-base pkgconf opus-dev git ca-certificates curl" ;;
        pacman) build_packages="base-devel pkgconf opus git ca-certificates curl" ;;
    esac
    step "Installing the build dependencies"
    # The splitting is the point: the lists above are lists.
    # shellcheck disable=SC2086
    install_packages "$manager" $build_packages
    step "Installing FFmpeg"
    # A distribution without FFmpeg in its repositories loses the containers the
    # in-process pipeline does not take, and nothing else.
    # shellcheck disable=SC2086
    install_packages "$manager" ffmpeg ||
        note "FFmpeg was not installed; the in-process pipeline still plays the common containers"
}

install_rust() {
    step "Installing the Rust toolchain"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    # What the installer put on the PATH, for this shell.
    # shellcheck disable=SC1091
    if [ -f "$HOME/.cargo/env" ]; then
        . "$HOME/.cargo/env"
    fi
}

build_node() {
    if ! command -v cargo >/dev/null 2>&1; then
        [ "$DEPS" = 1 ] || die "cargo is not installed and --no-deps was given"
        install_rust
    fi
    command -v cargo >/dev/null 2>&1 || die "cargo is not on the PATH"
    local memory swap
    memory=$(awk '/^MemTotal:/ { print int($2 / 1024) }' /proc/meminfo 2>/dev/null || printf '')
    swap=$(awk '/^SwapTotal:/ { print int($2 / 1024) }' /proc/meminfo 2>/dev/null || printf '0')
    if [ -n "$memory" ] && [ "$memory" -lt 1500 ] && [ "${swap:-0}" -lt 256 ]; then
        note "note: ${memory} MB of RAM and no swap; the release linker wants more (docs/vps.md has the swap recipe)"
    fi
    step "Building the node"
    if [ "$NATIVE" = 1 ]; then
        note "with -C target-cpu=native, which is right because the binary stays on this machine"
        (cd "$SOURCE" && RUSTFLAGS="-C target-cpu=native" cargo build --release --locked)
    else
        (cd "$SOURCE" && cargo build --release --locked)
    fi
}

uninstall() {
    step "Removing the service"
    if command -v systemctl >/dev/null 2>&1; then
        systemctl disable --now rustlink 2>/dev/null || true
        rm -f /etc/systemd/system/rustlink.service
        systemctl daemon-reload
    fi
    # The configuration, the logs and the downloaded helpers are the operator's
    # data, so they are named rather than removed.
    note "the unit is gone; nothing in $DIRECTORY was touched"
    note "remove them with: rm -rf $DIRECTORY && userdel $SERVICE_USER"
}

install_files() {
    step "Creating the user and the directory"
    if ! user_exists "$SERVICE_USER"; then
        # Busybox systems have their own adduser, which this does not speak.
        if ! command -v useradd >/dev/null 2>&1 || ! command -v groupadd >/dev/null 2>&1; then
            die "useradd and groupadd are missing; on Alpine that is apk add shadow"
        fi
        group_exists "$SERVICE_USER" || groupadd --system "$SERVICE_USER"
        # The login shell of an account that is never logged into, wherever the
        # distribution keeps it.
        local shell=/bin/false
        local candidate
        for candidate in /usr/sbin/nologin /sbin/nologin; do
            if [ -x "$candidate" ]; then
                shell=$candidate
                break
            fi
        done
        useradd --system --gid "$SERVICE_USER" --home-dir "$DIRECTORY" \
            --shell "$shell" "$SERVICE_USER"
    fi
    SERVICE_GROUP=$(id -gn "$SERVICE_USER")
    install -d -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$DIRECTORY"
    install -m 0755 "$SOURCE/target/release/rustlink" "$DIRECTORY/rustlink"
    install -m 0644 "$SOURCE/LICENSE" "$SOURCE/NOTICE" "$DIRECTORY/"
}

write_config() {
    step "Writing the configuration"
    local template="$SOURCE/config.example.yaml"
    local escaped_address escaped_password
    escaped_address=$(sed_replacement "$ADDRESS")
    escaped_password=$(sed_replacement "$PASSWORD")
    if [ -f "$template" ]; then
        # The example is the configuration with the three values this run knows
        # better, so every comment and every other default stays where it is.
        sed -e "s|^address:.*|address: $escaped_address:$PORT|" \
            -e "s|^password:.*|password: \"$escaped_password\"|" \
            -e "s|^autoDownload:.*|autoDownload: true|" \
            "$template" >"$DIRECTORY/config.yaml"
    else
        cat >"$DIRECTORY/config.yaml" <<TEXT
address: $ADDRESS:$PORT
password: "$PASSWORD"
autoDownload: true
logDirectory: logs
logRetentionDays: 14
TEXT
    fi
    chown "$SERVICE_USER:$SERVICE_GROUP" "$DIRECTORY/config.yaml"
    chmod 0640 "$DIRECTORY/config.yaml"
}

sed_replacement() {
    # CLI values are literal data: '&' must not expand to the matched line,
    # and a delimiter in a password must not become another sed command.
    printf '%s' "$1" | sed 's/[\\&|]/\\&/g'
}

set_password() {
    local escaped_password
    escaped_password=$(sed_replacement "$PASSWORD")
    sed -i -e "s|^password:.*|password: \"$escaped_password\"|" "$DIRECTORY/config.yaml"
    note "the password in $DIRECTORY/config.yaml was replaced"
}

install_unit() {
    step "Installing the systemd unit"
    local escaped_directory
    escaped_directory=$(sed_replacement "$DIRECTORY")
    # The unit in the repository names the default directory and user; those
    # names are rewritten to the ones this run was given.
    sed -e "s|/opt/rustlink|$escaped_directory|g" \
        -e "s|^User=rustlink|User=$SERVICE_USER|" \
        -e "s|^Group=rustlink|Group=$SERVICE_GROUP|" \
        "$SOURCE/deploy/rustlink.service" >/etc/systemd/system/rustlink.service
    chmod 0644 /etc/systemd/system/rustlink.service
    systemctl daemon-reload
}

check_configuration() {
    step "Checking the configuration"
    # Resolve the optional FFmpeg helper and validate before starting service.
    (cd "$DIRECTORY" && run_as_user "$DIRECTORY/rustlink" --config config.yaml --check)
}

start_service() {
    step "Starting the service"
    systemctl enable rustlink >/dev/null 2>&1 || true
    systemctl restart rustlink
    local attempt
    for attempt in 1 2 3 4 5 6 7 8 9 10; do
        sleep 1
        systemctl is-active --quiet rustlink && break
    done
    if ! systemctl is-active --quiet rustlink; then
        journalctl -u rustlink -n 40 --no-pager || true
        die "the service did not start"
    fi
    note "rustlink is running"
}

summary() {
    step "Installed"
    cat <<TEXT
    binary         $DIRECTORY/rustlink
    configuration  $DIRECTORY/config.yaml
    address        $ADDRESS:$PORT
TEXT
    if [ "$SERVICE" = 1 ]; then
        cat <<TEXT
    logs           journalctl -u rustlink -f
    restart        systemctl restart rustlink
TEXT
    else
        cat <<TEXT
    start          cd $DIRECTORY && ./rustlink --config config.yaml
TEXT
    fi
    cat <<TEXT
    update         git pull && sudo ./install.sh
TEXT
    if [ -n "$GENERATED_PASSWORD" ]; then
        cat <<TEXT

    The password for clients is $GENERATED_PASSWORD
    It is in the configuration as well; change it if this node is reachable
    from anywhere else.
TEXT
    fi
    case $ADDRESS in
        127.0.0.1 | ::1 | localhost)
            cat <<TEXT

    The node listens on the loopback address, so only this machine reaches it.
    A bot on another machine wants --address 0.0.0.0 and a firewall rule;
    docs/vps.md has both, and the proxy snippets for wss://.
TEXT
            ;;
        *)
            cat <<TEXT

    A bot on another machine joins at:

        http://$(machine_address):$PORT/api/v1

    with Authorization: Bearer <password>. Create a session with POST /api/v1/sessions.
    The node is only as private
    as the firewall around it, so let the bot through and nothing else:

        ufw allow from <bot address> to any port $PORT proto tcp

    For wss:// put a proxy in front; docs/vps.md has the Caddy and nginx
    snippets and the sizing table for the machine this runs on.
TEXT
            ;;
    esac
}

# The address a client on another machine would use, which is the first one the
# machine reports about itself. It is a hint rather than an answer: a machine
# behind NAT knows only its own address, and the operator knows the public one.
machine_address() {
    local found=
    # The address the machine would use to reach the internet is the one a
    # client on another machine usually has to name, and it is the one behind
    # the default route rather than the first one the machine happens to list.
    if command -v ip >/dev/null 2>&1; then
        found=$(ip route get 1.1.1.1 2>/dev/null |
            awk '{for (i = 1; i < NF; i++) if ($i == "src") { print $(i + 1); exit }}') || found=
    fi
    if [ -z "$found" ] && command -v hostname >/dev/null 2>&1; then
        found=$(hostname -I 2>/dev/null | awk '{print $1}') || found=
    fi
    if [ -z "$found" ]; then
        found="<this machine's address>"
    fi
    printf '%s' "$found"
}
main() {
    if [ "$UNINSTALL" = 1 ]; then
        uninstall
        return
    fi
    [ -f "$SOURCE/Cargo.toml" ] ||
        die "run this from the repository checkout: $SOURCE has no Cargo.toml"
    [ -f "$SOURCE/deploy/rustlink.service" ] ||
        die "deploy/rustlink.service is missing from $SOURCE"
    # A systemctl binary is not the same thing as systemd being the init system,
    # and a container has the first without the second.
    if [ "$SERVICE" = 1 ] &&
        { ! command -v systemctl >/dev/null 2>&1 || [ ! -d /run/systemd/system ]; }; then
        die "systemd is not running here; install with --no-service and start the node yourself"
    fi
    if [ "$BUILD" = 1 ] && [ "$DEPS" = 1 ]; then
        install_dependencies
    fi
    if [ "$BUILD" = 1 ]; then
        build_node
    fi
    [ -x "$SOURCE/target/release/rustlink" ] ||
        die "no binary at $SOURCE/target/release/rustlink; build it, or drop --no-build"
    install_files
    if [ -f "$DIRECTORY/config.yaml" ]; then
        note "$DIRECTORY/config.yaml is left as it is"
        [ -z "$PASSWORD" ] || set_password
    else
        if [ -z "$PASSWORD" ]; then
            # The same shape of password the node writes for itself: sixteen
            # random bytes as hexadecimal.
            PASSWORD=$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')
            GENERATED_PASSWORD=$PASSWORD
        fi
        write_config
    fi
    if [ "$SERVICE" = 1 ]; then
        install_unit
        check_configuration || die "the node refused the configuration, which is printed above"
        start_service
    fi
    summary
}

main
