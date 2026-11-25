# VPS 與 Docker 部署指南

Rustlink 編譯為單一原生執行檔，不需安裝 JVM、Node.js 或 Python；支援透過 `install.sh` 一鍵部署 systemd 雙服務（音訊節點＋佇列音樂機器人），或以 Docker Compose 一鍵啟動。

> [!TIP]
> 若資料中心 IP 遭 YouTube 要求登入驗證，請在 `config.yaml` 設定 `youtubeProxy`（如本機 Cloudflare WARP `socks5://127.0.0.1:40000`），詳見 [YouTube 專章](youtube.md)。

## 主機容量參考

| 主機規格 | `maxPlayers` | `maxConcurrentLoads` | `cacheEntries` |
| --- | ---: | ---: | ---: |
| 1 vCPU / 512 MB | 10 | 2 | 128 |
| 1 vCPU / 1 GB | 25 | 3 | 256 |
| 2 vCPU / 2 GB | 60 | 4 | 512 |
| 4 vCPU / 4 GB+（預設） | 100 | 8 | 1024 |

註：1 GB RAM 主機若無 swap，建議於首次本機編譯前建立 2 GB swap（`fallocate -l 2G /swapfile && chmod 600 /swapfile && mkswap /swapfile && swapon /swapfile`），以供 `thin LTO` 連結器使用。

## 一鍵安裝 systemd 服務（`install.sh`）

在專案目錄下執行：

```sh
# 同時部署音訊節點（rustlink.service）與音樂機器人（rustlink-bot.service）
sudo DISCORD_BOT_TOKEN=你的BotToken ./install.sh --with-bot

# 或僅部署音訊節點（供外部機器人透過 API v1 連線）
sudo ./install.sh
```

`install.sh` 會自動安裝缺少的 C 工具鏈、`pkg-config`、`libopus`、FFmpeg 與 `rustup`，在單次 `cargo build --release --locked` 中編譯所需執行檔，建立 `rustlink` 系統使用者與 `/opt/rustlink`，寫入權限 `0640` 的 `config.yaml`（與 `bot.env`），通過 `--check` 自檢後啟動 systemd 服務。**日後更新只需執行 `git pull && sudo ./install.sh`**（若 `/opt/rustlink/bot.env` 已存在，更新時會自動重編並重啟節點與機器人，且保留既有自訂設定）。

| 選項 | 說明 |
| --- | --- |
| `--dir PATH` / `--user NAME` | 安裝目錄（預設 `/opt/rustlink`）與系統使用者（預設 `rustlink`） |
| `--address ADDR` / `--port PORT` | 節點監聽位址與連接埠（預設 `0.0.0.0:2333`；反向代理後可用 `--address 127.0.0.1`） |
| `--password VALUE` | 指定節點密碼（建議改用環境變數 `RUSTLINK_PASSWORD`；未指定時自動產生） |
| `--bot-token VALUE` / `--with-bot` | 部署 `rustlink-bot.service` 與 `/opt/rustlink/bot.env`（建議用環境變數 `DISCORD_BOT_TOKEN`） |
| `--native` | 啟用 `-C target-cpu=native` 針對本機 CPU 最佳化編譯 |
| `--no-build` / `--no-deps` / `--no-service` | 略過編譯、略過安裝系統套件，或僅安裝檔案不註冊 systemd |
| `--uninstall` | 停止並移除 `rustlink` 與 `rustlink-bot` 單元（保留 `/opt/rustlink` 資料） |

常用維運指令：

```sh
systemctl status rustlink rustlink-bot
journalctl -u rustlink -u rustlink-bot -f
sudo -u rustlink /opt/rustlink/rustlink --config /opt/rustlink/config.yaml --check
```

## Docker Compose 一鍵部署

映像內建 `libopus`、`FFmpeg`、`rustlink` 與 `rustlink-bot`，並預設啟用 `read_only` 檔案系統、`no-new-privileges` 與 `cap_drop: ALL`：

```sh
# 僅啟動音訊節點（預設綁定本機 127.0.0.1:2333）
RUSTLINK_PASSWORD=your-strong-password docker compose up -d --build

# 一鍵同時啟動音訊節點＋Discord 音樂機器人
RUSTLINK_PASSWORD=your-strong-password DISCORD_BOT_TOKEN=your-bot-token \
  docker compose --profile bot up -d --build
```

## 主機調校、防火牆與 TLS 反向代理

節點預設自動設定 `MALLOC_ARENA_MAX=2`（壓低 glibc 多執行緒常駐記憶體）與 `TCP_NODELAY`。建議額外啟用 Linux BBR 壅塞控制並限制 journal 體積：

```sh
printf 'net.core.default_qdisc=fq\nnet.ipv4.tcp_congestion_control=bbr\n' >/etc/sysctl.d/90-rustlink.conf
sysctl --system
mkdir -p /etc/systemd/journald.conf.d
printf '[Journal]\nSystemMaxUse=200M\n' >/etc/systemd/journald.conf.d/rustlink.conf
systemctl restart systemd-journald
```

若機器人位於另一台主機，請以防火牆限制僅允許機器人 IP 連入 `2333/tcp`（`ufw allow from <機器人IP> to any port 2333 proto tcp`），或透過反向代理終止 TLS（節點改綁 `127.0.0.1:2333`）：

- **Caddy**：
  ```text
  node.example.com {
      reverse_proxy 127.0.0.1:2333
  }
  ```
- **nginx**：
  ```nginx
  location / {
      proxy_pass http://127.0.0.1:2333;
      proxy_http_version 1.1;
      proxy_set_header Upgrade $http_upgrade;
      proxy_set_header Connection "upgrade";
      proxy_read_timeout 900s;
  }
  ```
