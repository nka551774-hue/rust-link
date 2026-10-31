# Rustlink

以 **Rust** 撰寫的 Discord 音訊服務，使用專案自訂的 **Rustlink API v1**。伺服器、客戶端、DAVE 協定狀態機與音訊封裝均由 Rust 實作。

機器人透過 REST 管理工作階段與播放器，透過 WebSocket 接收播放事件。音源解析、音訊解碼、濾鏡、Opus 編碼與 Discord 語音連線由 Rustlink 處理。

服務不依賴 NodeLink、Lavalink 或 Golalink／GoLink，採用自己的 API 與曲目格式。FFmpeg／libopus 保留作音訊工具；Rust 模組邊界、依賴驗證與歷史歸屬見 [獨立 Rust 架構](docs/architecture.md)。

## 功能

- `/api/v1` REST API、Bearer 驗證及 `rustlink-client` 非同步 Rust 客戶端。
- Discord voice gateway v8、DAVE v1 端到端加密、MLS 成員管理與 Opus 音訊傳送。
- 自訂 `rlt1.` 曲目 token，使用 UTF-8 JSON 與 Base64URL，可保存中文及 emoji。
- YouTube／YouTube Music 原生 InnerTube、SoundCloud、Spotify／Deezer 中繼資料與 HTTP 音源；音源解析由 Rust 直接處理。
- Symphonia 原生解碼（包含 AAC、MP3、ALAC、ISO-MP4、FLAC、WebM／Matroska、Ogg、WAV）、自製 DSP 濾鏡與系統 libopus 編碼；FFmpeg 僅作為內建管線無法解碼容器的選配後備。
- 播放、停止、暫停、定位、音量、濾鏡、歌詞、斷線後恢復工作階段。
- 容量限制、請求限流、Prometheus `/metrics`、日誌及選用 webhook。

`/api/v1/info` 回報 `capabilities.discord_dave: true` 與語音協定版本。DAVE 使用 OpenMLS 與 RustCrypto；音訊金鑰僅在 Discord 發出 Execute Transition 後切換。協商版本 1 時，未取得有效金鑰不會送出音訊。詳見 [DAVE 與語音架構](docs/voice.md)。

## 建置與啟動

Linux：

```sh
bash start.sh
```

Windows x64：雙擊 **`start.bat`**，或在終端執行：

```bat
start.bat
```

腳本會切換到專案目錄、檢查建置依賴並建置 release。Linux 支援 apt、dnf、pacman、apk，安裝系統依賴時可能需要 sudo。Windows 使用 MSYS2 UCRT64 的 GCC／libopus 與 GNU Rust；缺少 MSYS2 時透過 winget 安裝。首次需網路，編譯可能花幾分鐘。Windows 的 PowerShell 輔助檔 `start.ps1` 必須與 BAT 放在一起。

首次沒有設定檔與密碼環境變數時，程式產生含隨機密碼的 `config.yaml`，後續啟動保留現有設定。`RUSTLINK_PASSWORD`、`RUSTLINK_ADDRESS` 可覆寫設定；預設監聽 `0.0.0.0:2333`。`RUSTLINK_SKIP_BOOTSTRAP=1` 禁止腳本自動安裝依賴；`RUSTLINK_MSYS2_ROOT` 可指定 Windows 的 MSYS2 路徑。

腳本會轉送參數，例如 `bash start.sh --config config.yaml`、`start.bat --check`、`start.bat --version`。手動建置需要 Rust 1.88 以上及 libopus：

```sh
cargo build --release --locked --bin rustlink
target/release/rustlink --config config.yaml
```

Windows 手動建置產物為 `target/release/rustlink.exe`；腳本建置產物位於 `target/<host-target>/release/`。`--help` 顯示參數，`--check` 檢查設定及媒體工具，`--version` 顯示版本。

FFmpeg 依使用的音訊容器選配。YouTube 若要求 PO token，需自行提供有效的 token 與 visitor data；Rustlink 不會自動產生。設定方式見 [configuration.md](docs/configuration.md) 及 [sources.md](docs/sources.md)。

原生 YouTube 播放需要客戶端 API 回傳直接音訊 URL；需播放器腳本還原簽章、特殊串流協定或遭平台拒絕時，會回報來源錯誤。支援範圍與 SoundCloud 的串流限制列於音源文件。

YouTube 分享網址可以直接傳給 `Client::resolve`，不需要改成搜尋文字。支援一般影片、`youtu.be`、YouTube Music、Shorts 等網址；影片網址附帶 `list` 時仍載入該影片。Discord 指令接線與完整範例見 [貼 YouTube 連結播放](docs/youtube.md)。

Spotify 的歌曲、專輯與播放清單連結也可直接傳給 `Client::resolve`，支援地區前綴、分享參數與嵌入連結。設定 `SPOTIFY_CLIENT_ID`／`SPOTIFY_CLIENT_SECRET` 後讀取歌曲資料，再以歌名、歌手與長度匹配完整的 YouTube 音源；不使用 Spotify 的短版預覽。詳細設定與匹配限制見 [音源文件](docs/sources.md)。

新設定預設使用 **48 kHz 雙聲道、192 kbit/s Opus、complexity 10**，既有明確設定仍會保留。YouTube 優先選原始音軌與未動態壓縮版本，再比較音訊 bitrate。Rustlink 不插入片頭／中途廣告，也不將搜尋中的廣告項目當作歌曲；影片原本錄入的置入內容仍屬音源的一部分。轉碼不會增加原始音源已缺失的細節。

## Rust 機器人串接

客戶端 crate 隨本專案提供，尚未發佈到 crates.io。在機器人的 `Cargo.toml` 加入本機路徑：

```toml
[dependencies]
rustlink-client = { path = "../rustlink/crates/client" } # 改成你的專案位置
tokio = { version = "1", features = ["macros", "rt-multi-thread", "signal"] }
```

主要流程：

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

    // 機器人加入 Discord 語音頻道後，先呼叫 session.set_voice(guild_id, voice)。
    // voice 來自 VOICE_STATE_UPDATE 與 VOICE_SERVER_UPDATE；不是 Discord bot token。
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

完整範例（包含語音狀態轉送與錯誤時清理）：[crates/client/examples/bot.rs](crates/client/examples/bot.rs)。

```sh
cargo run -p rustlink-client --example bot -- "ytsearch:lofi"
cargo run -p rustlink-client --example bot -- "https://youtu.be/1CXveaq_vFU"
```

範例讀取 `RUSTLINK_URL`、`RUSTLINK_PASSWORD`、`DISCORD_BOT_ID`、`DISCORD_GUILD_ID`、`DISCORD_CHANNEL_ID`、`DISCORD_VOICE_TOKEN`、`DISCORD_VOICE_ENDPOINT`、`DISCORD_VOICE_SESSION_ID`。語音參數必須由你的 Rust Discord 機器人取得並隨連線更新；範例不會自行登入 Discord。

## API 文件

- [API v1](docs/api.md)：端點、請求、回應、事件與 Discord 語音狀態對應。
- [DAVE 與語音架構](docs/voice.md)：協定、加密流程、測試範圍與連線診斷。
- [設定](docs/configuration.md)、[音源](docs/sources.md)、[第三方服務](docs/third-party.md)、[VPS 部署](docs/vps.md)。

- [效能與量測](docs/performance.md)：快取、音訊與加密的改善、實測數字及重跑方法。
- [本輪審查與驗證](docs/optimization-audit.md)：YouTube／Spotify、高音質設定、修復項目及測試限制。

## Docker／VPS

```sh
RUSTLINK_PASSWORD=your-password docker compose up --build
```

映像包含 libopus 與 FFmpeg。VPS 可使用 `sudo bash install.sh`，或部署 `deploy/rustlink.service`。跨機器連線使用防火牆限制存取，TLS 交由反向代理處理。

## Workspace

| Crate | 職責 |
| --- | --- |
| `rustlink-protocol` | 原生 API 型別與版本化曲目 token |
| `rustlink-client` | 非同步 REST／WebSocket Rust 客戶端；不依賴音訊解碼器或 libopus |
| `rustlink-config` | 設定、環境變數及驗證 |
| `rustlink-source` | 音源解析、下載與探測 |
| `rustlink-audio` | 解碼、DSP、Opus 與 FFmpeg 後備管線 |
| `rustlink-dave` | DAVE v1、MLS 提案／Commit／Welcome、轉換狀態與 Opus 加密 |
| `rustlink-voice` | Discord voice gateway v8、心跳、UDP 探測與 RTP-size 傳輸加密 |
| `rustlink-discord-bot` | Discord main Gateway v10、bot IDENTIFY/heartbeat、訊息 REST 與 voice state 配對；不把 bot token 傳給 voice transport |
| `rustlink-lyrics` | LRCLIB／YouTube Music 歌詞 |
| `rustlink-server` | API、工作階段、播放器及監控 |
| `rustlink` | 伺服器執行檔 |

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

API 整合測試驗證 Rust 客戶端、事件、重連、容量與狀態變更。DAVE 測試以另一個 Rust 實作作為接收端，涵蓋成員進出、金鑰切換、竄改與重播；語音測試啟動本機 WebSocket／UDP 模擬 Discord，驗證完整握手與雙層音訊加密。`rustlink-discord-bot` 另以離線測試驗證 Gateway payload 與 voice event pairing；真實 Discord bot 播放仍需操作者提供帳號、權限與語音頻道。

原生 bot 的環境變數與生命週期見 [Native Rust Discord Bot](docs/native-bot.md)；外部 Lavalink/NodeLink/GoLink wire contract 不會混入核心，能力對照見 [Rust-native API matrix](docs/native-api-matrix.md)，本輪盤點與驗證證據見 [native refactor evidence](docs/native-refactor-20261101.md)。

## 授權與來源

GPL-3.0-or-later，見 [LICENSE](LICENSE)。專案歷史、協定參考及第三方來源保留於 [NOTICE](NOTICE)。
