# 部署到 VPS

Rustlink 是一個執行檔加一個設定檔：提供機器人使用的 REST 與 WebSocket，在行程內解碼與濾鏡音訊、編碼 Opus，並自己連 Discord 語音 gateway，所以主機不需要任何執行環境，也不需要其他輔助行程（只有內建解碼器不處理的容器才需要 FFmpeg）。這份文件以 Debian／Ubuntu 為例：建置、容量、設定、systemd 單元與防火牆。

> **YouTube 與主機位址。** YouTube 依網路位址決定要不要信任請求，資料中心與住宅位址都可能被要求登入。若部署後 YouTube 對你的主機回應「Sign in to confirm you're not a bot」，請用 `youtubeProxy` 換一個出口，並在該出口實測。見 [YouTube](youtube.md)。

## 容量

每 20 毫秒要對每個播放器解碼、重採樣、濾鏡與編碼一個 frame。CPU 與記憶體用量取決於 codec、濾鏡、位元率與硬體，選播放器上限前請用預期的工作量實測。

走 FFmpeg 後備的曲目每首有自己的子行程。`maxPlayers` 限制播放器數量，`maxConcurrentLoads` 限制同時進行的來源解析。下表只是需要在負載下驗證的起點，不是容量保證：

| 主機 | `maxPlayers` | `maxConcurrentLoads` | `cacheEntries` |
| --- | --- | --- | --- |
| 1 vCPU、512 MB | 10 | 2 | 128 |
| 1 vCPU、1 GB | 25 | 3 | 256 |
| 2 vCPU、2 GB | 60 | 4 | 512 |
| 4 vCPU、4 GB 以上 | 100 | 8 | 1024 |

預設值就是最後一列。原生解碼避免了每首曲目一個 FFmpeg 行程；請依 CPU 與常駐記憶體的量測調整。

## 安裝

在 checkout 目錄執行一個指令：

```sh
sudo ./install.sh
```

（在 Windows 上取得的 checkout 可能沒有執行權限位元，`sudo bash install.sh` 是一樣的。）

它會安裝建置依賴（C 工具鏈、pkg-config、libopus，以及發行版有提供時的 FFmpeg）、在沒有的機器上安裝 Rust 工具鏈、建置 release、建立 `rustlink` 使用者與目錄、以 `config.example.yaml` 為底寫出含自己密碼的設定、安裝 `deploy/rustlink.service`、用服務使用者檢查設定，最後啟動服務；產生的密碼是它最後印出的內容。寫出的設定監聽 `0.0.0.0:2333`，其他機器上的機器人才連得到；節點放在會終止 TLS 的代理後面時，用 `--address 127.0.0.1`。

| 選項 | 作用 |
| --- | --- |
| `--dir PATH` | 安裝位置（預設 `/opt/rustlink`） |
| `--user NAME` | 執行服務的系統使用者（預設 `rustlink`） |
| `--address ADDR`、`--port PORT` | 寫出的設定監聽什麼（預設 `0.0.0.0:2333`，所有介面） |
| `--password VALUE` | 指定密碼，不用產生的 |
| `--native` | 以 `-C target-cpu=native` 建置 |
| `--no-build` | 安裝 `target/release` 裡已有的執行檔 |
| `--no-service` | 只安裝檔案，不動 systemd |
| `--no-deps` | 不安裝套件與 Rust 工具鏈 |
| `--uninstall` | 停止並移除單元，保留資料 |

再執行一次就是更新流程：設定保留，執行檔與單元被取代，服務重新啟動。以下是它手動做的事。

### 建置

```sh
apt-get update
apt-get install -y build-essential pkg-config libopus-dev git ca-certificates
curl https://sh.rustup.rs -sSf | sh -s -- -y --profile minimal
. "$HOME/.cargo/env"
git clone <本專案網址> rustlink && cd rustlink
cargo build --release --locked
```

產物是 `target/release/rustlink`。唯一值得安裝的執行期依賴是 FFmpeg，而且只為了內建解碼器不處理的容器：

```sh
apt-get install -y ffmpeg
```

用 `RUSTFLAGS="-C target-cpu=native"` 建置可能提升效能，但只適合執行檔留在建置它的那台機器；要部署到別處就維持可攜的預設，或選所有目的 CPU 都支援的指令集。CPU 型號的年份不保證支援哪些指令。

release 設定是單一 codegen unit 加 thin LTO，這讓執行檔很快，也讓連結器需要記憶體：1 GB 記憶體的機器沒有 swap 就建不完。可以在較大的機器上建置再把 `target/release/rustlink` 複製過去，或先加 swap：

```sh
fallocate -l 2G /swapfile
chmod 600 /swapfile
mkswap /swapfile
swapon /swapfile
```

### 檔案與使用者

一個系統使用者、一個目錄，執行檔與設定放在裡面：

```sh
useradd --system --home-dir /opt/rustlink --shell /usr/sbin/nologin rustlink
install -d -o rustlink -g rustlink /opt/rustlink
install -m 0755 target/release/rustlink /opt/rustlink/
install -m 0644 config.yaml /opt/rustlink/
```

節點把下載的 FFmpeg 寫進 `.tools`、每日日誌寫進 `logs`；下載暫存檔放在作業系統的暫存目錄，來源快取只在記憶體。

## 伺服器的設定

從 `config.example.yaml` 開始，改這些：

```yaml
address: 0.0.0.0:2333       # 所有介面，讓別台機器的機器人連得到
password: "..."             # 密碼是空的，節點拒絕啟動
ffmpeg: /usr/bin/ffmpeg     # 用路徑，確保用的是套件版
autoDownload: false         # 用上面指定的 FFmpeg 路徑
maxPlayers: 25              # 見上表
maxConcurrentLoads: 3
opusComplexity: 10          # 0..10；越低，每個播放器越省 CPU
cacheEntries: 256
logDirectory: logs
logRetentionDays: 14        # 更舊的每日檔案會被刪除
youtubeProxy: ""            # YouTube 要求登入時才填
```

每次修改後執行 `rustlink --check`：它解析輔助工具並驗證設定，但不監聽。

```sh
sudo -u rustlink /opt/rustlink/rustlink --check
```

`autoDownload: false`、`.tools` 裡也沒有可用的 FFmpeg 時，節點只能播內建解碼器支援的格式。

## systemd

`deploy/rustlink.service` 以系統使用者執行節點，並把機器鎖緊：除節點目錄外唯讀的檔案系統、沒有 capabilities，並限制 malloc arena 數量以壓低常駐記憶體。

```sh
install -m 0644 deploy/rustlink.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now rustlink
systemctl status rustlink
journalctl -u rustlink -f
```

單元把 `RUSTLINK_LOG_DIRECTORY` 設為空，所以節點印出的內容進 journal（journal 本來就會輪替）。刪掉那一行就改寫每日檔案，由 `logRetentionDays` 決定保留多久。每日檔名的日期是伺服器的日期：設為 UTC 的機器以 UTC 命名，不是操作者的時區；要用別的時區命名，在單元裡設定 `TZ`。

## 調校主機

為延遲敏感的服務通常要對主機做的事，有一部分由節點自己做，因為 VPS 上這些最有影響，而設定檔不是放它們的地方：

- glibc 的 malloc arena 在行程還是單執行緒時就限制為兩個。這與單元檔要求的相同，只是由節點套用，且只在沒有別人指定時：環境變數 `MALLOC_ARENA_MAX` 會保留操作者給的值。
- 每個接受的連線都設 `TCP_NODELAY`，安靜下來的連線一分鐘後探測。被強制結束、沒關 socket 的機器人，節點在兩分鐘內就回收那個 session，而不是讓核心把 socket 留兩小時。
- Opus 編碼器以其刻度的最高值運作，這是有餘裕核心的節點該做的；每首曲目的每個 frame 只編碼一次。播放器多於核心數的機器可降低 `opusComplexity`，內建管線與 FFmpeg 後備都遵循。

其餘的是啟動時讀一次的環境變數：

| 變數 | 作用 |
| --- | --- |
| `TOKIO_WORKER_THREADS` | 執行階段的 worker 數。預設每核心一個就對；只有短缺的是 REST 而不是音訊時才調高。 |
| `MALLOC_ARENA_MAX` | 覆寫 arena 上限；`0` 表示完全不限。 |
| `RUSTLINK_LOG_DIRECTORY` | 每日檔案的位置，空表示用 journal。 |
| `RUSTLINK_LOG_RETENTION_DAYS` | 保留幾個每日檔案，`0` 全部保留。 |

兩個核心設定值得花那一分鐘：節點仰賴大量下載，用 BBR；以及限制小硬碟上 journal 能用的空間。

```sh
printf 'net.core.default_qdisc=fq\nnet.ipv4.tcp_congestion_control=bbr\n' >/etc/sysctl.d/90-rustlink.conf
sysctl --system
mkdir -p /etc/systemd/journald.conf.d
printf '[Journal]\nSystemMaxUse=200M\n' >/etc/systemd/journald.conf.d/rustlink.conf
systemctl restart systemd-journald
```

環境變數啟動時才讀，所以改了要 `systemctl restart rustlink`。以上都不需要重新開機。

## 防火牆與 TLS

節點需要對外的 TCP（音源）與 UDP（Discord 語音）。對內只有機器人要連得到這個埠：

```sh
ufw allow from <機器人位址> to any port 2333 proto tcp
ufw enable
```

客戶端要 `wss://` 時，在前面放代理並讓它終止 TLS。Caddy 只要一行：

```
node.example.com {
    reverse_proxy 127.0.0.1:2333
}
```

用 nginx 要明確寫出 WebSocket upgrade：

```nginx
location / {
    proxy_pass http://127.0.0.1:2333;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_read_timeout 900s;
}
```

## 更新

```sh
git pull
sudo ./install.sh
```

安裝程式保留設定、取代其他一切，所以更新和第一次安裝是同一個指令。手動則是：

```sh
git pull
cargo build --release --locked
install -m 0755 target/release/rustlink /opt/rustlink/
systemctl restart rustlink
```

重新啟動是平順的：先通知 session 節點要離開、曲目結束、再結束行程；單元給停止動作 20 秒。

## Docker

儲存庫的映像含 FFmpeg 與 libopus，在 VPS 上的用法和別處一樣：

```sh
RUSTLINK_PASSWORD=hunter2 docker compose up -d --build
```

容器設定了哪些變數，見 `compose.yaml` 與 `Dockerfile`。
