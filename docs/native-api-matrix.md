# Rust 原生 API 對照

本文件界定 Rustlink 的原生邊界。Rustlink **不提供** Lavalink、NodeLink 或 GoLink 相容層；若未來需轉接外部協定，必須置於獨立 adapter crate，不得讓外部欄位進入核心。

## 分層邊界

| 層 | 模組 | 職責 | 明確不做 |
| --- | --- | --- | --- |
| Discord REST | `discord-bot::rest` | API v10 認證、發訊、斜線指令註冊與互動回應（含 429 退避） | 不洩漏 Bot Token 至錯誤或網址；自訂 base 僅限 Discord 或 loopback |
| Discord 主 Gateway | `discord-bot::gateway` | HELLO、IDENTIFY、`connect_with_messages`（`4014` 自動降級）、`resume`（`OP 6`）、`reconnect`、心跳與事件派發 | 不處理 Opus 編碼、播放器狀態或語音媒體封包 |
| 語音狀態配對 | `discord-bot::voice_state` | 將機器人自身的 `VOICE_STATE_UPDATE` 與 `VOICE_SERVER_UPDATE` 配對為 `VoiceState` | 拒絕其他使用者或非綁定 guild 事件；絕不混用 Bot Token |
| Rustlink Session | `rustlink-client`、`rustlink-server` | 工作階段、guild 播放器、事件流與斷線續傳 | 不接受 Lavalink／NodeLink 二進位曲目格式 |
| 音源 | `rustlink-source` | 純 Rust 解析、預解析快取、512 KiB 驗證分段讀取與 `ETag` 校驗 | 不啟動 `yt-dlp`、Node.js、JVM 或 Go 擷取器 |
| 音訊 | `rustlink-audio` | 48 kHz Opus 直通、解碼、DSP 濾鏡與 Opus 編碼 | 不宣稱轉碼超越原始母帶；不剪除影片內建口播廣告 |
| Discord 語音 | `rustlink-voice`、`rustlink-dave` | Voice Gateway v8、UDP／RTP-size、OpenMLS DAVE v1 加密 | 不登入 Discord 主 Gateway |

## 原生端點摘要

所有端點位於 `/api/v1`（詳見 [API v1](api.md)）：

- **節點與指標**：`/healthz`、`/version`、`/api/v1/info`、`/api/v1/stats`、`/metrics`
- **曲目與歌詞**：`POST /api/v1/tracks/{resolve,inspect,inspect-batch}`、`POST /api/v1/lyrics`
- **工作階段與播放器**：`/api/v1/sessions`、`/api/v1/sessions/{id}`、`/api/v1/sessions/{id}/players/{guild}`、`/api/v1/sessions/{id}/events`

## 刻意不註冊之路徑

- Lavalink：`/v3/*`、`/v4/*`、`/loadtracks`、`/decodetrack(s)`、`/routeplanner/*`
- NodeLink：mixer、TTS、chapters、workers、direct-stream 等擴充端點
- GoLink：屬 Go 客戶端函式庫，非伺服器協定
