# Native Rust refactor evidence

日期：2026-11-01（Asia/Taipei）。本紀錄是本輪把播放、Discord bot 邊界與
API 重新收斂到 RustLink 原生介面的驗收摘要。

## 取代與保留

- source、media、audio、server、voice、DAVE、client 與 bot 都在本 workspace
  的 Rust crates 內；播放路徑不啟動 `yt-dlp`、`youtube-dl`、Node.js、Java
  或 Go 音訊服務。
- YouTube 走 Rust InnerTube client，取得直接的 adaptive audio URL，再由
  range／ETag 驗證的 media reader 讀取；搜尋與播放清單會略過 promoted/ad
  renderer 子樹。Rustlink 不排程 pre-roll 或 mid-roll，也不會把影片本身已
  錄入的贊助口播剪掉。
- 常見 AAC、MP3、ALAC、FLAC、ISO-MP4、Matroska、Ogg、WAV 已由 Symphonia
  原生 feature 解碼，降低不必要的 FFmpeg fallback。FFmpeg 仍是未支援容器的
  可選外部後備；Opus 編碼／解碼仍使用系統 libopus。這些邊界不宣稱「零外部
  native library」或所有格式都不需 FFmpeg。
- 預設輸出為 48 kHz、雙聲道、192 kbit/s Opus、complexity 10。來源本身的
  bitrate、平台能力與網路狀況仍限制最終聽感。

## Discord bot 原生邊界

`rustlink-discord-bot` 直接實作：

1. Discord main Gateway v10 的 HELLO、IDENTIFY、heartbeat、READY、dispatch、
   OP4 voice state，以及 token-safe URL／body 驗證；OP4 另提供明確的
   self-mute/deaf flags。
2. Discord REST v10 的 bounded `send_text`，禁止 redirect、credential URL、
   任意遠端 custom base 與無界 response body，並以 `Zeroizing` 保存 bot token。
3. `VoiceStatePairer` 將自身 bot user 的 `VOICE_STATE_UPDATE` 與
   `VOICE_SERVER_UPDATE` 依 guild 配對；可鎖定單一 guild，且 server-first 換代、
   session／頻道變更時會丟棄舊 token，不會產生跨世代混合的 `VoiceState`。
4. `examples/play.rs` 把 READY、voice pairing、Rustlink session、resolve、
   play 與關閉生命週期串成可編譯的 native bridge，僅要求 guild／voice intents。

Gateway 收到 OP7／OP9 時會交回可辨識的 `Reconnect`／`InvalidSession` 錯誤；
自動退避、RESUME 與重新取得 voice state 應由部署層 supervisor 管理，避免把
一次連線物件誤當成永久連線。範例不會在沒有操作者 credentials 時假裝完成真實
Discord e2e。

## API 決策

Rustlink 對外只承諾自己的 `/api/v1`、`rlt1.` track token、session/player
資源與事件格式。Lavalink `/v3`／`/v4`、`/loadtracks`、NodeLink 擴充欄位與
GoLink client wire contract 不被複製到核心，也沒有外部相容 runtime；能力與
遷移對照見 [native-api-matrix.md](native-api-matrix.md)。若未來需要相容舊客戶端，
必須在獨立 adapter crate 內明確指定上游版本，不能讓外部 encoded track 或未驗證
URL 進入 source、audio、voice 核心。

## 本輪驗證

截至本紀錄建立時，以下離線 gates 已通過：

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo clippy -p rustlink-discord-bot --all-targets --locked -- -D warnings`
- `cargo test -p rustlink-discord-bot --all-targets --locked --no-fail-fast`：15 passed
- `cargo test -p rustlink-protocol --lib --locked`：8 passed（VoiceState Debug 脫敏含括）
- `cargo test -p rustlink-server --all-targets --locked --no-fail-fast`：50 passed
- `cargo test -p rustlink-source -p rustlink-audio -p rustlink-voice --locked --no-fail-fast`：
  source 126 passed／1 ignored、audio 95 passed／5 ignored、voice 27 passed

最後 workspace gate：`cargo test --workspace --locked --no-fail-fast --jobs 2`，
429 passed／0 failed／6 ignored（D 槽獨立 target；log：
`D:\\rustlink-target-native-refactor-20261101\\workspace-final.log`）。上述 ignored
項目需要本機 FFmpeg 或刻意的 release microbenchmark，不能當成已執行的 live
Discord／Spotify／YouTube 帳號驗證。Gateway 目前交回 reconnect/invalid-session
與遠端 close 訊號，fresh IDENTIFY 的重連由 supervisor 負責，未宣稱本 crate 已
自動 RESUME。
