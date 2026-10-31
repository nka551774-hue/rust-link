# Rust-native API matrix

這份矩陣定義 RustLink 新的原生邊界。它不是 Lavalink、NodeLink 或
DisGoLink 的相容性承諾；外部協定若日後需要支援，必須放在獨立 adapter
crate，不能把外部欄位滲入播放核心。

## 原生分層

| 層 | Rust 型別／模組 | 責任 | 不應做的事 |
| --- | --- | --- | --- |
| Discord Bot REST | `discord-bot::rest::DiscordRest` | Discord API v10 的認證、發訊息及應用程式資源請求 | 不把 bot token 放進錯誤、日誌或 URL；custom base 只允許 Discord host 或 loopback；不處理音訊資料 |
| Discord 主 Gateway | `discord-bot::gateway` | HELLO、IDENTIFY、heartbeat、READY、reconnect/invalid-session 訊號及 dispatch | 不直接編碼 Opus 或管理播放器狀態；退避重連／RESUME 由上層 supervisor 負責 |
| Voice state bridge | `discord-bot::voice_state` | 配對自身 `VOICE_STATE_UPDATE` 與 `VOICE_SERVER_UPDATE`，產生 `VoiceState`；可綁定單一 guild | 不接受其他使用者或未綁定 guild 的 voice event；不把 bot token 當 voice token |
| Rustlink session | `rustlink-client`／`rustlink-server` | session、guild player、queue、事件與恢復 | 不依賴 NodeLink/Lavalink 的 encoded track 格式 |
| Source | `rustlink-source` | 原生來源解析、直接媒體 URL、range/ETag 驗證 | 不啟動 yt-dlp、Node、Java 或 Go extractor |
| Audio | `rustlink-audio` | 解碼、48 kHz DSP、Opus 編碼與濾鏡 | 不把轉碼品質宣稱高於原始音源；不把廣告移除當成解碼功能 |
| Discord voice | `rustlink-voice`／`rustlink-dave` | Voice gateway、UDP/RTP-size、DAVE 加密與封包傳送 | 不登入 Discord 主 Gateway；該責任屬 Bot 層 |

## 目前原生端點

目前 server API 使用 `/api/v1` 與 `snake_case`：

| 能力 | 原生端點 | 狀態 |
| --- | --- | --- |
| 節點資訊／狀態 | `/healthz`、`/version`、`/api/v1/info`、`/api/v1/stats`、`/metrics` | 已有 |
| 音源解析 | `POST /api/v1/tracks/resolve` | 已有；不執行 yt-dlp |
| 曲目 token | `POST /api/v1/tracks/inspect`、`inspect-batch` | 已有；`rlt1.` 是 Rustlink 自有格式 |
| Session | `POST/GET/PATCH/DELETE /api/v1/sessions/{id}` | 已有 |
| Player | `GET/PATCH/DELETE .../players/{guild}` | 已有 |
| Session events | `GET .../events` | 已有；Rustlink WebSocket 事件格式 |
| 歌詞 | `POST /api/v1/lyrics`、player lyrics | 已有 |
| Bot 發訊息 | `DiscordRest::send_text` | Rust SDK 層；不經 Rustlink server 轉送 token |

## 明確不是相容層

以下路徑目前刻意不註冊：

- Lavalink `/v3/*`、`/v4/*`、`/loadtracks`、`/decodetrack(s)`、
  `/routeplanner/*`。
- NodeLink 的 mixer、TTS、chapters、workers、YouTube runtime config 與
  direct-stream 擴充端點。
- DisGoLink 的 Go client API；它是另一個客戶端函式庫，不是 RustLink
  server 的來源協定。

若產品真的需要外部客戶端相容，先固定上游版本與完整 wire contract，
再新增 `crates/lavalink-adapter`。adapter 只能把外部 JSON 轉換成上述原生
型別，不能讓外部 encoded track、插件欄位或未驗證 URL 直接進入 source、
audio 或 voice 核心。

## 驗收邊界

原生 API 的完成條件是：

1. Bot REST 錯誤不含 token，channel ID、request body 與 custom base 有界且可驗證。
2. Bot Gateway 的兩種 voice event 不論到達順序（含 server-first 換代）都能
   正確配對，且只接受自己的 user ID 與目標 guild。
3. 來源層沒有 `yt-dlp`／Node／Java／Go 程序啟動；YouTube 只使用允許的
   直接音訊 URL，影片內已錄入的內容不由播放器剪除。
4. 音訊以 48 kHz 雙聲道、可設定 Opus bitrate/complexity 輸出，並以
   fixture、loopback voice 與長串流回歸驗證。
5. API、Gateway、voice、audio 的測試可在沒有真實 bot token 的情況下用
   mock server／mock WebSocket 重現；真實 Discord 驗證另列為外部限制。
