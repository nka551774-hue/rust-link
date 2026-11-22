# Rustlink

用 **Rust** 寫成的 Discord 音訊節點。機器人透過 Rustlink 自己的 **API v1**（REST 加 WebSocket 事件）建立工作階段與播放器；音源解析、解碼、濾鏡、Opus 編碼、Discord 語音連線與 DAVE 端到端加密都由節點在 Rust 內完成。

- 播放時**不會下載整個檔案**，也不執行 yt-dlp、Node.js、Java 或 Go：音訊每次以 128 KiB 分段讀取，讀到就在記憶體裡解碼並送進語音頻道。
- 不依賴也不相容 Lavalink、NodeLink 或 GoLink，使用自己的 API 與 `rlt1.` 曲目 token。能力邊界見 [Rust 原生 API 對照](docs/native-api-matrix.md)。
- 音訊工具只有系統 libopus（編碼）與選配的 FFmpeg（只處理內建解碼器不支援的容器）。

## 功能

- `/api/v1` REST、Bearer 驗證、WebSocket 事件，以及非同步 Rust 客戶端 `rustlink-client`。
- Discord voice gateway v8、DAVE v1（OpenMLS）、RTP-size 傳輸加密。
- YouTube／YouTube Music（InnerTube）、SoundCloud、Spotify 與 Deezer（只取中繼資料，音訊改由 YouTube 比對）、一般 HTTP 音訊。
- Symphonia 原生解碼（AAC、MP3、ALAC、FLAC、MP4、WebM／Matroska、Ogg、WAV）、自製 DSP 濾鏡、48 kHz 雙聲道 192 kbit/s Opus（可調）。
- 播放、暫停、定位、音量、濾鏡、歌詞，斷線後可恢復工作階段。
- 容量與限流、Prometheus `/metrics`、日誌檔輪替、選用 webhook。

## 快速開始

需要 Rust 1.88 以上與 libopus。啟動腳本會檢查並安裝建置依賴（Linux 支援 apt、dnf、pacman、apk，可能需要 sudo；Windows 使用 MSYS2 UCRT64 與 GNU Rust，缺少時透過 winget 安裝）：

```sh
bash start.sh        # Linux
start.bat            # Windows x64，也可以直接雙擊
```

第一次啟動若沒有設定檔也沒有 `RUSTLINK_PASSWORD`，節點會寫出含隨機密碼的 `config.yaml`，之後沿用。預設監聽 `0.0.0.0:2333`。腳本會把參數轉給節點，例如 `--check`（檢查設定與工具）、`--version`、`--config <檔案>`。

手動建置：

```sh
cargo build --release --locked --bin rustlink
target/release/rustlink --config config.yaml
```

所有設定見 [設定](docs/configuration.md)；`config.example.yaml` 是完整範本。

## 在 Discord 用 `/play` 播 YouTube

`crates/discord-bot/examples/slash.rs` 是一個完整的 Rust 機器人：註冊 `/play url:<連結或關鍵字>`，加入使用者所在的語音頻道並播放。先啟動節點，再執行：

```sh
export DISCORD_BOT_TOKEN=...        # 只放環境變數，不要寫進檔案或 log
export RUSTLINK_PASSWORD=...        # 同 config.yaml 的 password
cargo run -p rustlink-discord-bot --example slash
```

指令會註冊到機器人所在的每個伺服器，不必等全域指令生效。機器人只要求 `GUILDS` 與 `GUILD_VOICE_STATES` intent，不需要 Message Content。

> **YouTube 在你的網路上可能要求登入。** 對它不信任的網路位址，YouTube 會回應「Sign in to confirm you're not a bot」，或只給前約 900 KiB（約一分鐘）的音訊。節點因此先試不需要 token 的 `IOS_PRODUCER` 用戶端；仍然被擋時，在設定裡填 `youtubeProxy`，讓 YouTube 的流量改走代理。細節見 [YouTube](docs/youtube.md)。

## 音樂機器人範例（`.env` 設定）

`crates/discord-bot/examples/music/` 是專為連接 Rustlink 寫的音樂機器人：有佇列、跳過、暫停、音量，閒置時自動離開語音頻道。設定放在 `.env`：

```sh
cp .env.example .env        # 填入 DISCORD_BOT_TOKEN 與 RUSTLINK_PASSWORD（.env 已被 git 忽略）
cargo run -p rustlink-discord-bot --example music
```

指令：`/play`（連結或關鍵字，播放清單會整份加入）、`/skip`、`/pause`、`/resume`、`/stop`、`/queue`、`/nowplaying`、`/volume`。其他設定項與環境變數覆寫見 [`.env.example`](.env.example) 與[原生機器人](docs/native-bot.md)。同一個 bot token 一次只能跑一個機器人範例，否則兩個程式會回應同一個指令。

## 自己的 Rust 機器人

客戶端 crate 隨專案提供，尚未發佈到 crates.io：

```toml
[dependencies]
rustlink-client = { path = "../rustlink/crates/client" } # 改成實際位置
tokio = { version = "1", features = ["macros", "rt-multi-thread", "signal"] }
```

```rust,no_run
use rustlink_client::{Client, LoadResult};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new("http://127.0.0.1:2333", std::env::var("RUSTLINK_PASSWORD")?)?;
    let session = client.create_session(&std::env::var("DISCORD_BOT_ID")?, 60).await?;
    let mut events = session.events().await?;
    let reader = tokio::spawn(async move {
        while let Some(event) = events.next().await? {
            println!("{event}");
        }
        Ok::<_, rustlink_client::Error>(())
    });

    // 機器人加入語音頻道後，先用 VOICE_STATE_UPDATE 與 VOICE_SERVER_UPDATE
    // 組出 voice，呼叫 session.set_voice(guild_id, voice)。voice.token 不是 bot token。
    if let LoadResult::Search(tracks) = client.resolve("ytsearch:lofi").await? {
        if let Some(track) = tracks.first() {
            session.play(&std::env::var("DISCORD_GUILD_ID")?, &track.token).await?;
        }
    }
    tokio::signal::ctrl_c().await?;
    session.destroy().await?;
    reader.abort();
    Ok(())
}
```

四個可執行範例，由簡到完整：

| 範例 | 用途 |
| --- | --- |
| `crates/client/examples/bot.rs` | 只示範客戶端；voice 資料從環境變數讀入，不登入 Discord |
| `crates/discord-bot/examples/play.rs` | 登入 Discord，加入指定頻道並播放一個查詢 |
| `crates/discord-bot/examples/slash.rs` | 只有 `/play` 的最小斜線指令機器人 |
| `crates/discord-bot/examples/music/` | 完整的音樂機器人：佇列與八個指令，用 `.env` 設定 |

## Docker 與 VPS

```sh
RUSTLINK_PASSWORD=your-password docker compose up --build
```

映像含 libopus 與 FFmpeg。Debian／Ubuntu 主機可用 `sudo bash install.sh`，或部署 `deploy/rustlink.service`；細節與調校見 [VPS 部署](docs/vps.md)。跨機器連線請用防火牆限制來源，TLS 交給反向代理。

## Workspace

| Crate | 職責 |
| --- | --- |
| `rustlink-protocol` | API 型別與版本化曲目 token |
| `rustlink-client` | 非同步 REST／WebSocket 客戶端 |
| `rustlink-config` | 設定檔、環境變數與驗證 |
| `rustlink-source` | 音源解析、媒體讀取與探測 |
| `rustlink-audio` | 解碼、DSP、Opus 與 FFmpeg 後備管線 |
| `rustlink-dave` | DAVE v1：MLS、轉換狀態、Opus frame 加密 |
| `rustlink-voice` | Discord voice gateway v8、UDP 探測、RTP-size 傳輸加密 |
| `rustlink-discord-bot` | Discord 主 Gateway v10、REST、斜線指令互動、voice state 配對 |
| `rustlink-lyrics` | LRCLIB 與 YouTube Music 歌詞 |
| `rustlink-server` | API、工作階段、播放器、監控 |
| `rustlink` | 節點執行檔 |

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

測試以本機 HTTP、WebSocket、UDP 模擬 Discord，並用另一個 Rust 實作（`davey`）當 DAVE 對端。連真實 YouTube 的測試預設略過，要手動執行：

```sh
cargo test -p rustlink-source --test live_youtube -- --ignored --nocapture
```

## 文件

| 文件 | 內容 |
| --- | --- |
| [API v1](docs/api.md) | 端點、請求、事件、語音狀態對應 |
| [設定](docs/configuration.md) | 所有設定項與環境變數 |
| [音源](docs/sources.md) | 識別字串路由、各來源行為、快取與網路防護 |
| [YouTube](docs/youtube.md) | 連結格式、用戶端、代理與播放限制 |
| [DAVE 與語音](docs/voice.md) | 協定、金鑰、連線與失敗處理 |
| [原生 Discord 機器人](docs/native-bot.md) | `rustlink-discord-bot` 的流程與安全界線 |
| [架構](docs/architecture.md) | 元件責任與 HTTP 資料完整性 |
| [Rust 原生 API 對照](docs/native-api-matrix.md) | 與 Lavalink／NodeLink 的關係 |
| [VPS 部署](docs/vps.md)、[第三方服務](docs/third-party.md) | 部署、外部服務說明 |
| [效能](docs/performance.md)、[審查紀錄](docs/optimization-audit.md)、[原生重構](docs/native-refactor-20261101.md)、[Discord 語音審查](docs/reviews/round3-discord.md)、[全庫審查](docs/reviews/takeover-full-review.md) | 歷史量測與審查（含日期） |

## 授權

GPL-3.0-or-later，見 [LICENSE](LICENSE)。協定參考與第三方來源見 [NOTICE](NOTICE)。
