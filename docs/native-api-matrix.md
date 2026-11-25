# Rust 原生 API 對照

這份文件界定 Rustlink 的原生邊界。它**不是** Lavalink、NodeLink 或 GoLink 的相容性承諾；若日後需要支援外部協定，必須放在獨立的 adapter crate，不能讓外部欄位滲入播放核心。

## 分層

| 層 | 模組 | 責任 | 不做的事 |
| --- | --- | --- | --- |
| Discord REST | `discord-bot::rest::DiscordRest` | Discord API v10 的認證、發訊息、斜線指令與互動回應 | 不把 bot token 放進錯誤、日誌或網址；自訂 base 只允許 Discord 主機或 loopback；不處理音訊資料 |
| Discord 主 Gateway | `discord-bot::gateway` | HELLO、IDENTIFY、`Gateway::connect_with_messages`（`4014` 免特權自動降級）、`Gateway::resume`（`OP 6`）、`Gateway::reconnect`（有界退避與續傳／重新 IDENTIFY 切換）、heartbeat、READY 與 dispatch | 不編碼 Opus、不管理播放器或語音媒體傳輸 |
| 語音狀態配對 | `discord-bot::voice_state` | 配對自己的 `VOICE_STATE_UPDATE` 與 `VOICE_SERVER_UPDATE`，產生 `VoiceState`；可鎖定單一 guild | 不接受其他使用者或未綁定 guild 的事件；不把 bot token 當 voice token |
| Rustlink session | `rustlink-client`、`rustlink-server` | session、guild 播放器、事件與恢復 | 不依賴 NodeLink／Lavalink 的 encoded track 格式 |
| 音源 | `rustlink-source` | 原生解析、直接媒體網址、範圍與 ETag 驗證 | 不啟動 yt-dlp、Node、Java 或 Go 的擷取器 |
| 音訊 | `rustlink-audio` | 解碼、48 kHz DSP、Opus 編碼與濾鏡 | 不宣稱轉碼品質高於原始音源；不把移除廣告當成解碼功能 |
| Discord 語音 | `rustlink-voice`、`rustlink-dave` | Voice gateway、UDP／RTP-size、DAVE 加密與封包傳送 | 不登入 Discord 主 Gateway（屬機器人層） |

## 原生端點

所有端點在 `/api/v1`，使用 snake_case。完整欄位見 [API v1](api.md)。

| 能力 | 端點 |
| --- | --- |
| 節點資訊與狀態 | `/healthz`、`/version`、`/api/v1/info`、`/api/v1/stats`、`/metrics` |
| 音源解析 | `POST /api/v1/tracks/resolve` |
| 曲目 token | `POST /api/v1/tracks/inspect`、`inspect-batch`；`rlt1.` 是 Rustlink 自有格式 |
| Session | `POST /api/v1/sessions`、`GET／PATCH／DELETE /api/v1/sessions/{id}` |
| 播放器 | `GET／PATCH／DELETE …/players/{guild}` |
| 事件 | `GET …/events`（Rustlink 自己的 WebSocket 事件格式） |
| 歌詞 | `POST /api/v1/lyrics`、播放器的 `…/lyrics` |

## 刻意不提供

以下路徑沒有註冊：

- Lavalink 的 `/v3/*`、`/v4/*`、`/loadtracks`、`/decodetrack(s)`、`/routeplanner/*`。
- NodeLink 的 mixer、TTS、chapters、workers、YouTube runtime config 與 direct-stream 擴充端點。
- GoLink 是另一個客戶端函式庫，不是 Rustlink 伺服器的來源協定。

真的需要相容外部客戶端時，先固定上游版本與完整的 wire contract，再新增獨立的 adapter crate。adapter 只能把外部 JSON 轉成上面的原生型別，不能讓外部的 encoded track、插件欄位或未驗證的網址直接進入 source、audio 或 voice 核心。

## 完成條件

1. 機器人 REST 的錯誤不含 token；channel ID、request body 與自訂 base 有界且可驗證。
2. 機器人 Gateway 的兩種語音事件不論到達順序（含 server-first 換代）都能正確配對，且只接受自己的 user ID 與目標 guild。
3. 來源層沒有 yt-dlp、Node、Java 或 Go 程序；YouTube 只使用直接的音訊網址，影片內已錄入的內容不由播放器剪除。
4. 音訊以 48 kHz 雙聲道、可設定的 Opus 位元率與複雜度輸出，並以 fixture、loopback 語音與長串流回歸驗證。
5. API、Gateway、語音與音訊的測試不需真實 bot token，用模擬的伺服器與 WebSocket 即可重現；真實 Discord 的驗證另列為外部限制。
