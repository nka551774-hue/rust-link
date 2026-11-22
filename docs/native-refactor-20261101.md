# 原生重構紀錄（2026-11-01，歷史）

2026-11-01（Asia/Taipei）把播放、Discord 機器人邊界與 API 收斂到 Rustlink 原生介面的驗收摘要。內容是當時的狀態；之後新增的斜線指令、YouTube 用戶端與單人頻道的 DAVE 見 [原生機器人](native-bot.md)、[YouTube](youtube.md)、[DAVE 與語音](voice.md)。

## 取代與保留

- source、media、audio、server、voice、DAVE、client 與機器人都在本 workspace 的 Rust crates 內；播放路徑不啟動 yt-dlp、youtube-dl、Node.js、Java 或 Go 音訊服務。
- YouTube 走 Rust InnerTube 用戶端取得直接的 adaptive 音訊網址，再由做了範圍與 ETag 驗證的 media reader 讀取；搜尋與播放清單略過推廣子樹。Rustlink 不排程片頭或中途廣告，也不剪掉影片本身已錄入的贊助口播。
- 常見的 AAC、MP3、ALAC、FLAC、ISO-MP4、Matroska、Ogg、WAV 由 Symphonia 原生解碼，減少不必要的 FFmpeg 後備。FFmpeg 仍是不支援容器的選配外部後備，Opus 編碼與解碼仍用系統 libopus；不宣稱「零外部原生函式庫」，也不宣稱所有格式都不需要 FFmpeg。
- 預設輸出 48 kHz 雙聲道、192 kbit/s Opus、複雜度 10；最終聽感仍受來源位元率、平台能力與網路限制。

## 機器人邊界

`rustlink-discord-bot` 直接實作：

1. Discord 主 Gateway v10 的 HELLO、IDENTIFY、heartbeat、READY、dispatch 與 opcode 4 語音狀態，以及不含 token 的網址與 body 驗證；opcode 4 另提供明確的 self-mute／deaf 旗標。
2. Discord REST v10 的有界 `send_text`：禁止 redirect、網址中的認證資訊、任意遠端的自訂 base 與無界的回應 body；bot token 以 `Zeroizing` 保存。
3. `VoiceStatePairer` 把機器人自己的 `VOICE_STATE_UPDATE` 與 `VOICE_SERVER_UPDATE` 依 guild 配對；可鎖定單一 guild，server-first 換代、session 或頻道變更時會丟棄舊 token，不會產生跨世代混合的 `VoiceState`。
4. `examples/play.rs` 把 READY、語音配對、Rustlink session、resolve、play 與關閉串成可編譯的橋接範例，只要求 guild 與 voice intent。

Gateway 收到 opcode 7 或 9 時，交回可辨識的 `Reconnect`、`InvalidSession` 錯誤；自動退避、RESUME 與重新取得語音狀態由部署層負責。範例不會在沒有操作者憑證時假裝完成真實的 Discord 端到端測試。

## API 決策

Rustlink 對外只承諾自己的 `/api/v1`、`rlt1.` 曲目 token、session 與播放器資源及事件格式。Lavalink `/v3`、`/v4`、`/loadtracks`、NodeLink 擴充欄位與 GoLink 的客戶端 wire contract 沒有被複製到核心，也沒有外部相容的執行期。對照見 [Rust 原生 API 對照](native-api-matrix.md)。

## 當時的驗證

離線 gate 全部通過：`cargo fmt --all --check`；`cargo clippy --workspace --all-targets --locked -- -D warnings`；機器人 crate 15 項、protocol 8 項（含 `VoiceState` 的 Debug 遮蔽）、server 50 項、source 126 項（1 ignored）、audio 95 項（5 ignored）、voice 27 項測試通過。最後的 workspace gate：`cargo test --workspace --locked --no-fail-fast`，**429 passed、0 failed、6 ignored**。

ignored 的項目需要本機 FFmpeg，或是刻意不在一般測試執行的 release microbenchmark，不能當成已執行真實 Discord、Spotify 或 YouTube 帳號的驗證。Gateway 當時只交回 reconnect、invalid-session 與遠端關閉的訊號，沒有宣稱本 crate 已自動 RESUME。
