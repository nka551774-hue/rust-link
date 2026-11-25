# 原生 Rust Discord 機器人

`rustlink-discord-bot` 是 Rustlink 自帶的機器人函式庫：在 Rust 內登入 Discord、處理斜線指令、配對語音事件，再把 `VoiceState` 交給 Rustlink。它不是 NodeLink、Lavalink 或 GoLink 的相容層，也不啟動 yt-dlp、Node.js、Java 或 Go 程序。

## 提供什麼

| 部分 | 內容 |
| --- | --- |
| `Gateway` | Discord 主 Gateway v10：HELLO、IDENTIFY、heartbeat、READY、dispatch、opcode 4 語音狀態，以及 `Gateway::connect_with_messages`（`4014` 免特權自動降級）、`Gateway::resume`（`OP 6` 工作階段續傳）與 `Gateway::reconnect`（有界退避、自動選擇續傳或重新 IDENTIFY） |
| `DiscordRest` | REST v10：發訊息、註冊 guild 斜線指令、回應與編輯互動（deferred response） |
| `VoiceStatePairer` | 把機器人自己的 `VOICE_STATE_UPDATE` 與 `VOICE_SERVER_UPDATE` 依 guild 配對成 `VoiceState`；可鎖定單一 guild |
| `Interaction`、`BotEvent` | 事件與互動資料；`Interaction::option` 讀指令參數 |

四個可執行範例：

| 範例 | 說明 |
| --- | --- |
| `crates/client/examples/bot.rs` | 只示範客戶端，voice 資料從環境變數讀入，不登入 Discord |
| `crates/discord-bot/examples/play.rs` | 登入、加入指定頻道、播放一個查詢（`DISCORD_BOT_TOKEN`、`DISCORD_GUILD_ID`、`DISCORD_CHANNEL_ID`、`RUSTLINK_URL`、`RUSTLINK_PASSWORD`） |
| `crates/discord-bot/examples/slash.rs` | `/play`、頻道直接貼連結與 `@Bot <連結／關鍵字>` 機器人（`DISCORD_BOT_TOKEN`、`RUSTLINK_PASSWORD`，可選 `RUSTLINK_URL`） |
| `crates/discord-bot/examples/music/` | 完整的佇列音樂機器人，同時支援斜線指令與直接貼連結播放，用 `.env` 設定（見下） |

`slash.rs` 與 `music` 啟動時透過 `Gateway::connect_with_messages` 連線：若已在 Discord Developer Portal 開啟 Message Content Intent，使用者在語音頻道時直接於文字頻道貼上 YouTube／SoundCloud／Spotify／Deezer 連結即會自動加入並播放；若未開啟該權限（Discord 回應 4014），會自動退回免特權連線，此時 `/play` 與 `@Bot <連結／關鍵字>` 依然立即可用，且支援手機分享文字擷取與省略 `https://` 的連結（如 `youtu.be/…`）。

## 音樂機器人（`music` 範例）

```sh
cp .env.example .env     # 填入兩個必要值；.env 已被 git 忽略
cargo run -p rustlink-discord-bot --example music            # 讀 ./.env
cargo run -p rustlink-discord-bot --example music -- my.env  # 指定別的檔案
```

啟動時先連 Rustlink 確認可用，連不上就直接報錯結束，不等到第一個指令才失敗。真正的環境變數優先於 `.env` 的同名項目，所以容器或 shell 可以單獨覆寫一個值；缺少 `.env` 時，只要環境變數有必要的值也能啟動。

| 變數 | 預設 | 說明 |
| --- | --- | --- |
| `DISCORD_BOT_TOKEN` | 必填 | 機器人 token；透過 `Gateway::connect_with_messages` 連線，若已開啟 Message Content Intent 可自動播放頻道裸連結，未開啟（`4014`）則自動降級為免特權模式（`/play` 與 `@Bot` 皆可用） |
| `RUSTLINK_PASSWORD` | 必填 | 節點設定檔的 `password` |
| `RUSTLINK_URL` | `http://127.0.0.1:2333` | 節點位址 |
| `DISCORD_GUILD_IDS` | 空 | 只在這些伺服器註冊指令（逗號分隔）；空表示機器人所在的每個伺服器 |
| `BOT_DEFAULT_VOLUME` | `100` | 起始音量 0–200 |
| `BOT_MAX_QUEUE` | `100` | 每個伺服器佇列上限 1–1000 |
| `BOT_IDLE_LEAVE_SECONDS` | `120` | 沒有歌可播多久後離開語音頻道；`0` 表示留到 `/stop` |

| 指令 | 行為 |
| --- | --- |
| `/play query` | 連結、影片 ID 或關鍵字。普通文字會自動變成 `ytsearch:`（Rustlink 對無法分類的文字回 `empty`），搜尋只加入最相符的一首；播放清單整份加入，超過佇列上限的部分略過並告知 |
| `/skip`、`/pause`、`/resume`、`/stop` | 控制播放；`/stop` 清空佇列並離開頻道。這些指令要求使用者與機器人在同一個語音頻道 |
| `/queue`、`/nowplaying` | 顯示佇列（前 10 首）與目前曲目的播放位置 |
| `/volume level` | 0–200，同樣要求與機器人在同一個語音頻道；Discord 傳來的選項只保留字串，所以 `level` 以文字讀入並驗證 |

設計上值得注意的幾點：

- **自動接歌**：收到 `track.ended` 且原因是 `finished` 或 `load_failed` 才換下一首，而且曲目必須與目前播放的相同；被 `/skip` 取代（`replaced`）或 `/stop` 的曲目不會重複推進佇列。無法開始播放的曲目會被略過，繼續下一首。
- **換頻道**：機器人正在別的頻道播放時，`/play` 會拒絕；閒置時才會被叫到新的頻道。
- **被移出頻道**：收到機器人自己的語音狀態變成沒有頻道（被人踢出或 Discord 以 4014 關閉）時，清空該伺服器的佇列並釋放 Rustlink 的播放器，下一次 `/play` 會重新加入。
- **上次留下的語音狀態**：啟動時若發現機器人還待在某個語音頻道（上次沒有正常結束），會先送出離開，避免 Rustlink 沒有對應的語音連線。
- **同一個 token 只能跑一個**：兩個程式用同一個 bot token 時，會同時回應同一個指令；此外 `set_guild_commands` 會**取代**該伺服器的整組指令。

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

`Gateway::next_event` 會把 Discord 的 `Reconnect`（`OP 7`）、`InvalidSession`（`OP 9`）或遠端關閉（回傳 `None`）交回呼叫端。呼叫端可直接呼叫 `Gateway::reconnect(cause)`：當持有有效 `session_id`／`sequence` 且非前一次尚未收到 `OP 0` 確認的續傳時，會優先以 `Gateway::resume`（`OP 6`）續傳工作階段；若續傳在確認前中斷、收到不可續傳的 `OP 9`（`d: false`）或關閉碼 `4007`／`4009`，則自動退回全新的 `IDENTIFY`（`OP 2`）。重連內建有界退避（`1s..=5s`，遇 `4008` 限流固定退避 `5s`），遇 `4014` 且含 `MESSAGE_CONTENT` 時會自動剝離該特權 intent 重連，其餘永久設定或認證錯誤（`4004`、`4010..=4014`）則直接回傳錯誤停止重試。當退回 `IDENTIFY` 重新收到 `BotEvent::Ready` 時，機器人應清除舊的語音就緒標記，並對仍在播放或等待加入的伺服器重新送出 `OP 4`（`set_voice_state`）。

## 連線與安全界線

- `VOICE_STATE_UPDATE` 要檢查 `guild_id`、`user_id` 與預期的 `channel_id`，避免別的機器人或別的 guild 的 session 綁進播放器。固定單一 guild 的機器人用 `VoiceStatePairer::for_guild`。
- `VOICE_SERVER_UPDATE.endpoint` 為 `null` 或空白時視為語音伺服器重新分配中，會清除已發送標記並等待下一個有效 `endpoint`（回傳 `Ok(None)`）；格式無效的端點則拒絕，不可退回明文音訊。
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
