# 原生 Rust Discord 機器人

`rustlink-discord-bot` 是 Rustlink 自帶的機器人函式庫：在 Rust 內登入 Discord、處理斜線指令、配對語音事件，再把 `VoiceState` 交給 Rustlink。它不是 NodeLink、Lavalink 或 GoLink 的相容層，也不啟動 yt-dlp、Node.js、Java 或 Go 程序。

## 提供什麼

| 部分 | 內容 |
| --- | --- |
| `Gateway` | Discord 主 Gateway v10：HELLO、IDENTIFY、heartbeat、READY、dispatch、opcode 4 語音狀態；收到 reconnect／invalid-session 或遠端關閉時交還呼叫端 |
| `DiscordRest` | REST v10：發訊息、註冊 guild 斜線指令、回應與編輯互動（deferred response） |
| `VoiceStatePairer` | 把機器人自己的 `VOICE_STATE_UPDATE` 與 `VOICE_SERVER_UPDATE` 依 guild 配對成 `VoiceState`；可鎖定單一 guild |
| `Interaction`、`BotEvent` | 事件與互動資料；`Interaction::option` 讀指令參數 |

三個可執行範例：

| 範例 | 說明 |
| --- | --- |
| `crates/client/examples/bot.rs` | 只示範客戶端，voice 資料從環境變數讀入，不登入 Discord |
| `crates/discord-bot/examples/play.rs` | 登入、加入指定頻道、播放一個查詢（`DISCORD_BOT_TOKEN`、`DISCORD_GUILD_ID`、`DISCORD_CHANNEL_ID`、`RUSTLINK_URL`、`RUSTLINK_PASSWORD`） |
| `crates/discord-bot/examples/slash.rs` | `/play url:<連結或關鍵字>` 斜線指令機器人（`DISCORD_BOT_TOKEN`、`RUSTLINK_PASSWORD`，可選 `RUSTLINK_URL`） |

`slash.rs` 在每個 `GUILD_CREATE` 為該伺服器註冊指令，追蹤各使用者所在的語音頻道，收到 `/play` 後先回覆「處理中」，透過 Rustlink 解析並播放第一首可播的曲目，再編輯回應；機器人尚未在使用者的頻道時，會先加入，等語音配對完成再播放。它沒有佇列，新的 `/play` 會替換正在播放的曲目。

## 生命週期

機器人要把 Discord Gateway 與 Rustlink session 視為兩條獨立的連線：

```text
Discord Gateway（bot token）
        │ READY
        │ VOICE_STATE_UPDATE + VOICE_SERVER_UPDATE
        ▼
VoiceStatePairer
        │ VoiceState { session_id, channel_id, token, endpoint }
        ▼
Rustlink Session::set_voice()
        ├── Client::resolve(query)
        └── Session::play(guild_id, track_token)
```

1. 從環境變數讀 bot token、Rustlink 網址與密碼；token 不可寫進 log、錯誤訊息、事件內容或檔案。
2. 連線 Gateway、處理 HELLO、依 interval 送 heartbeat 並 IDENTIFY。播放只需要 `GUILDS | GUILD_VOICE_STATES`；只有要讀訊息指令時才另外要求 `GUILD_MESSAGES | MESSAGE_CONTENT`，且要先在 Developer Portal 開啟該 privileged intent。
3. 收到 `READY` 後記住機器人的 user ID 與 application ID。要加入頻道時送同一個 guild 的語音狀態更新（預設 `self_mute=false`、`self_deaf=false`，要其他值用 `set_voice_state_with_flags`）。
4. 配對同一個 guild、同一個機器人的 `VOICE_STATE_UPDATE.session_id` 與 `VOICE_SERVER_UPDATE.token`／`endpoint`。兩者都到齊之前不可呼叫 Rustlink。
5. 建立 Rustlink session，先開事件 WebSocket，收到 `session.ready` 後呼叫 `Session::set_voice`。這裡的 `voice.token` 是 Discord 語音伺服器的 token，不是 bot token。
6. `Client::resolve` 後，從 `LoadResult` 挑曲目，用 `Session::play` 傳入 `rlt1.` token。
7. 同時維持 Discord heartbeat、Rustlink 事件 WebSocket 與語音傳輸。被移動頻道、重新分配或斷線時，丟掉舊的配對，重新收齊兩個事件，再更新 `set_voice`。
8. 收到 SIGINT／SIGTERM 時，先停止播放、銷毀 Rustlink session，再關閉 Gateway。

`Gateway::next_event` 會把 Discord 的 reconnect、invalid-session 或遠端關閉（回傳 `None`）交回呼叫端。正式的機器人必須用退避迴圈重建 Gateway 並重新取得語音狀態，不能把一次 WebSocket 物件當成永久連線。本函式庫只提供全新的 IDENTIFY，沒有自動 RESUME 與退避。

## 連線與安全界線

- `VOICE_STATE_UPDATE` 要檢查 `guild_id`、`user_id` 與預期的 `channel_id`，避免別的機器人或別的 guild 的 session 綁進播放器。固定單一 guild 的機器人用 `VoiceStatePairer::for_guild`。
- `VOICE_SERVER_UPDATE.endpoint` 為空或不符合 Discord 語音主機規則時要拒絕，保留可重試的錯誤；不可退回明文音訊。
- Rustlink 的 API password 只放在 Bearer 標頭，Discord bot token 只放在 Gateway IDENTIFY 與 REST 的授權標頭；兩者不可互換，也不可寫進 telemetry 或 webhook。語音傳輸層不會拿到 bot token。
- `DiscordRest` 禁止 redirect、網址裡的認證資訊與無界的回應 body；自訂 base 只允許 Discord 主機或 loopback；bot token 以 `Zeroizing` 保存，`Debug` 輸出會遮蔽。
- 事件佇列、heartbeat 與重連都要有上限與期限，不可用無界的 channel 或無限重試。

## 播放品質與來源

預設輸出是 48 kHz 雙聲道、192 kbit/s Opus、複雜度 10；來源層優先選原始、非自動配音、位元率較高的音軌。輸出仍受來源、Discord 語音頻道與網路限制，轉碼無法補回來源缺少的細節。

機器人應把查詢交給 `Client::resolve`，不要自己下載檔案、解析短效的 CDN 網址或呼叫外部下載器。曲目 token 只含中繼資料，Rustlink 在實際播放時才重新解析並驗證網址。YouTube 的用戶端、代理與限制見 [YouTube](youtube.md)。

Rustlink 不排程或插入廣告，並略過搜尋結果裡的推廣項目；但已錄在影片裡的贊助或置入不會被剪掉，不要向使用者承諾「絕對沒有廣告」。來源服務的條款與可用性由操作者負責。

## Codec 與 FFmpeg

常見的 AAC、MP3、ALAC、FLAC、ISO-MP4、Matroska、Ogg、WAV 由 Symphonia 原生解碼；不支援的容器與部分 timescale 組合才選擇性交給 FFmpeg。FFmpeg 是選配的外部音訊工具，不是相容層，也不是機器人登入的依賴。設定 `ffmpeg: none` 只代表不使用後備，不代表所有格式都能播。

## 測試範圍

離線測試涵蓋：Gateway 的 HELLO／IDENTIFY／heartbeat／READY 狀態機；延遲或亂序的兩個語音事件仍只在完整配對後產生一個 `VoiceState`；錯的 guild／user、空 endpoint、重複事件與重連不污染現有 session；bot token 不出現在 log；`voice.token` 與 Rustlink password 不互相替代。

`slash.rs` 已用真實的 Discord 機器人帳號與語音頻道執行過：機器人一個人在頻道裡完整播完一首 YouTube 歌曲（2026-11-22）。離線模擬本身不等於真實帳號驗證。
