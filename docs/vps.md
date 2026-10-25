# Running on a VPS

Rustlink is one binary and one configuration file. It serves the REST and
WebSocket surface a bot talks to, decodes and filters audio in process, encodes
Opus, and speaks the Discord voice gateway itself, so a server needs no runtime
and no helper process beyond FFmpeg for the containers the in-process pipeline
declines. This is the walkthrough for a Debian or Ubuntu server: the build, the
sizing, the configuration, a hardened systemd unit and the firewall.

## Sizing

A frame is decoded, resampled, filtered and encoded every twenty milliseconds.
CPU and memory use depend on the codec, filters, bitrate and hardware; measure
the expected workload before selecting a player limit.

Every track that falls back to FFmpeg gets its own subprocess. `maxPlayers`
bounds the number of players, while `maxConcurrentLoads` bounds concurrent
source resolutions. The table below is a starting point to validate under load,
not a capacity guarantee.

| Server | `maxPlayers` | `maxConcurrentLoads` | `cacheEntries` |
| --- | --- | --- | --- |
| 1 vCPU, 512 MB | 10 | 2 | 128 |
| 1 vCPU, 1 GB | 25 | 3 | 256 |
| 2 vCPU, 2 GB | 60 | 4 | 512 |
| 4 vCPU, 4 GB and up | 100 | 8 | 1024 |

The defaults are the last row. Native decoding avoids a per-track FFmpeg
process; use CPU and resident memory measurements to adjust these limits.

## Install

From the checkout, one command does all of it:

```sh
sudo ./install.sh
```

(A checkout made on Windows may not carry the executable bit;
`sudo bash install.sh` is the same thing.)

It installs the build dependencies (a C toolchain, pkg-config, libopus, and
FFmpeg where the distribution carries it), installs a Rust toolchain when the
machine has none, builds the release binary, creates the `rustlink` user and
the directory, writes a configuration from `config.example.yaml` with a
password of its own, installs the unit from `deploy/rustlink.service`, checks
the configuration as the service user, and starts the service. The password it
generated is the last thing it prints. The configuration it writes listens on
`0.0.0.0:2333`, so a bot on another machine reaches the node wherever it runs;
`--address 127.0.0.1` is for a node that a proxy terminates TLS for.

| Option | What it changes |
| --- | --- |
| `--dir PATH` | where the node is installed (default `/opt/rustlink`) |
| `--user NAME` | the system user it runs as (default `rustlink`) |
| `--address ADDR`, `--port PORT` | what the written configuration listens on (default `0.0.0.0:2333`, every interface) |
| `--password VALUE` | the password, instead of one that is generated |
| `--native` | build with `-C target-cpu=native` |
| `--no-build` | install the binary already in `target/release` |
| `--no-service` | install the files without touching systemd |
| `--no-deps` | install no packages and no Rust toolchain |
| `--uninstall` | stop and remove the unit, leaving the data behind |

Running it again is the update path: the configuration is kept, the binary and
the unit are replaced, and the service is restarted. What it does by hand is
the rest of this section.

### Build

```sh
apt-get update
apt-get install -y build-essential pkg-config libopus-dev git ca-certificates
curl https://sh.rustup.rs -sSf | sh -s -- -y --profile minimal
. "$HOME/.cargo/env"
git clone <this repository> rustlink && cd rustlink
cargo build --release --locked
```

The result is `target/release/rustlink`. FFmpeg is the one runtime dependency
worth installing, and only for the containers the built-in decoders decline:

```sh
apt-get install -y ffmpeg
```

A build with `RUSTFLAGS="-C target-cpu=native"` may improve performance when
the binary stays on the machine where it was built. For a binary deployed
elsewhere, keep the portable default or select an instruction set supported by
every destination CPU. CPU model dates do not guarantee instruction support.

The release profile is one codegen unit with thin LTO, which is what makes the
binary fast and also what makes the linker want memory: a machine with a
gigabyte of RAM does not get through the build without swap. Either build on a
bigger machine and copy `target/release/rustlink` over, or add a swap file
first:

```sh
fallocate -l 2G /swapfile
chmod 600 /swapfile
mkswap /swapfile
swapon /swapfile
```

### Files and user

One system user, one directory, and the binary and configuration inside it:

```sh
useradd --system --home-dir /opt/rustlink --shell /usr/sbin/nologin rustlink
install -d -o rustlink -g rustlink /opt/rustlink
install -m 0755 target/release/rustlink /opt/rustlink/
install -m 0644 config.yaml /opt/rustlink/
```

The node writes downloaded FFmpeg files into `.tools` and daily log files into
`logs`. Download staging files use the operating system's temporary directory;
source caches are kept in memory.

## Configuration for a server

Start from `config.example.yaml` and change these:

```yaml
address: 0.0.0.0:2333       # every interface, for a bot on another machine
password: "..."             # the node refuses to start with an empty one
ffmpeg: /usr/bin/ffmpeg     # a path, so the packaged build is the one used
autoDownload: false        # use the installed FFmpeg path above
maxPlayers: 25              # from the table above
maxConcurrentLoads: 3
opusComplexity: 10          # 0..10; lower spends less CPU on every player
cacheEntries: 256
logDirectory: logs
logRetentionDays: 14        # daily files older than this are deleted
```

`rustlink --check` resolves the helpers and validates the configuration without
listening, which is the step to run after every edit:

```sh
sudo -u rustlink /opt/rustlink/rustlink --check
```

With `autoDownload: false` and no usable FFmpeg path or copy in `.tools`, the
node plays only formats supported by its built-in decoders.

## systemd

`deploy/rustlink.service` runs the node as the system user with the machine
locked down around it: a read-only filesystem except the node directory, no
capabilities, and a malloc arena count that keeps resident memory down.

```sh
install -m 0644 deploy/rustlink.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now rustlink
systemctl status rustlink
journalctl -u rustlink -f
```

The unit leaves `RUSTLINK_LOG_DIRECTORY` empty, so what the node prints goes to
the journal, which already rotates it. Remove that line to write the daily
files instead, where `logRetentionDays` decides how long they stay.

The date in a daily file name is the date of the server, so a machine set to UTC
names the file after UTC rather than after the operator. Set `TZ` in the unit to
name it after another zone.

## Tuning the machine

Some of what an operator normally does to a server for a latency-sensitive
service is done by the node itself, because a VPS is where it matters and a
configuration file is the wrong place for it:

- glibc's malloc arenas are capped at two while the process is still single
  threaded. It is the same cap the unit file asks for, applied by the node, and
  only when nothing named one: `MALLOC_ARENA_MAX` in the environment keeps
  whatever value the operator gave it.
- Every accepted connection gets `TCP_NODELAY`, and one that goes quiet is
  probed after a minute. A bot that was killed without closing its socket is
  then a session the node reclaims inside two minutes rather than a socket the
  kernel would hold open for two hours.
- The Opus encoder runs at the top of its own scale, which is what a node with
  cores to spare should do, and every frame of every track is encoded once.
  A machine carrying more players than it has cores lowers `opusComplexity`,
  and the in-process pipeline and the FFmpeg fallback both follow it.

What is left is the environment, read once at start:

| Variable | Effect |
| --- | --- |
| `TOKIO_WORKER_THREADS` | The runtime's worker threads. The default, one per core, is right; raise it only when the REST surface, and not audio, is what the node is short of. |
| `MALLOC_ARENA_MAX` | Overrides the arena cap. Zero means no limit at all. |
| `RUSTLINK_LOG_DIRECTORY` | Where the daily files go, or empty for the journal. |
| `RUSTLINK_LOG_RETENTION_DAYS` | How many daily files are kept, with zero keeping all of them. |

Two kernel settings are worth the minute they take: BBR for the downloads the
node lives on, and a bound on what the journal may spend on a small disk.

```sh
printf 'net.core.default_qdisc=fq\nnet.ipv4.tcp_congestion_control=bbr\n' >/etc/sysctl.d/90-rustlink.conf
sysctl --system
mkdir -p /etc/systemd/journald.conf.d
printf '[Journal]\nSystemMaxUse=200M\n' >/etc/systemd/journald.conf.d/rustlink.conf
systemctl restart systemd-journald
```

The environment is read at start, so a variable is a `systemctl restart
rustlink` away. Nothing above needs a reboot.

## Firewall and TLS

The node needs outbound TCP for the sources and outbound UDP for Discord voice.
Inbound, only the bot has to reach the port:

```sh
ufw allow from <bot address> to any port 2333 proto tcp
ufw enable
```

For a client that wants `wss://`, put a proxy in front and let it terminate
TLS. Caddy needs one line:

```
node.example.com {
    reverse_proxy 127.0.0.1:2333
}
```

With nginx the WebSocket upgrade has to be named:

```nginx
location / {
    proxy_pass http://127.0.0.1:2333;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_read_timeout 900s;
}
```

## Updating

```sh
git pull
sudo ./install.sh
```

The installer keeps the configuration and replaces everything else, so an
update is the same command as the first install. By hand it is:

```sh
git pull
cargo build --release --locked
install -m 0755 target/release/rustlink /opt/rustlink/
systemctl restart rustlink
```

A restart is graceful: sessions are told the node is going away, tracks end,
and the process exits. The unit gives a stop twenty seconds to finish.

## Docker

The image in the repository carries FFmpeg and libopus, and is
run on a VPS the same way as anywhere else:

```sh
RUSTLINK_PASSWORD=hunter2 docker compose up -d --build
```

See the Docker section of the README for what the container sets.
