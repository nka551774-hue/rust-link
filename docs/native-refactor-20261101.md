# 原生重構紀錄（2026-11-01，歷史）

本頁記錄 2026-11-01 將播放管線、Discord 機器人邊界與 API 收斂至純 Rust 原生介面的驗收快照。後續新增的斜線指令、`connect_with_messages` 自動降級、YouTube 用戶端備援與單人頻道 DAVE 見 [原生機器人](native-bot.md)、[YouTube](youtube.md) 與 [DAVE 與語音](voice.md)。

## 架構決策

- **純 Rust 媒體鏈**：`source`、`audio`、`server`、`voice`、`dave`、`client` 與 `discord-bot` 全數由工作區內 Rust Crates 實作，不執行 `yt-dlp`、Node.js、JVM 或 Go 服務。
- **音源與解碼**：YouTube 透過原生 InnerTube 取得 adaptive 音訊串流並驗證 `Content-Range` 與強 `ETag`；AAC、MP3、ALAC、FLAC、MP4、Matroska、Ogg、WAV 由 Symphonia 與原生 Opus 管線解碼，罕見容器才使用選配 FFmpeg，Opus 編解碼連結系統 `libopus`。
- **機器人邊界**：`rustlink-discord-bot` 實作 Gateway v10、無 Token 洩漏風險的 `DiscordRest`、`VoiceStatePairer` 與 `examples/play.rs`。
- **自有協定**：僅公開 `/api/v1` 與 `rlt1.` 曲目權杖，不複製 Lavalink `/v3`／`/v4` 或 NodeLink 擴充端點（見 [Rust 原生 API 對照](native-api-matrix.md)）。

## 當時離線驗證結果

`cargo fmt --all --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings` 與 `cargo test --workspace --locked --no-fail-fast` 全數通過（**429 passed、0 failed、6 ignored**）。
