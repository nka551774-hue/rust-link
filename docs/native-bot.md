# Native Rust Discord Bot

這份文件定義 Rustlink 自帶 Discord bot 的邊界與播放流程。它是 Rustlink
自己的 bot/gateway 設計，不是 NodeLink、Lavalink 或 GolangLavalink 的
相容層，也不會啟動 `yt-dlp`、Node.js、Java 或 Go 音訊服務。

目前 `rustlink-voice` 已經能處理 Discord voice gateway、UDP/RTP、DAVE 與
Opus 傳送；`rustlink-discord-bot` 則在 Rust 內完成主 Gateway v10 登入、REST
發訊息與兩種 voice event 配對，再把 `VoiceState` 交給 Rustlink。舊的
`rustlink-client/examples/bot.rs` 仍保留為「外部程式已登入 Discord」的低階
客戶端範例；真正的原生登入橋接見 `crates/discord-bot/examples/play.rs`。

## 生命週期

Bot 必須把 Discord bot gateway 與 Rustlink session 視為兩條不同的連線：

```text
Discord Gateway (bot token)
        │  READY
        │  VOICE_STATE_UPDATE + VOICE_SERVER_UPDATE
        ▼
Rust bot gateway adapter
        │  VoiceState { session_id, channel_id, token, endpoint }
        ▼
Rustlink Session::set_voice()
        │
        ├── Client::resolve(query)
        └── Session::play(guild_id, track_token)
```

建議的最小流程如下：

1. 從環境變數讀取 bot token、guild ID、voice channel ID、Rustlink URL 與
   Rustlink password。token 不得寫入 log、錯誤訊息、事件 payload 或持久化檔案。
2. 連線 Discord Gateway，處理 `HELLO`，依 heartbeat interval 發送 heartbeat，
   再以 `IDENTIFY` 登入。播放範例只要求 `GUILDS | GUILD_VOICE_STATES`；
   只有真的要讀取訊息指令時才另外啟用 `GUILD_MESSAGES | MESSAGE_CONTENT`，
   並先在 Developer Portal 開啟對應 privileged intent。
3. 收到 `READY` 後，記住 bot user ID。發送同一 guild 的 voice state update，
   `channel_id` 指定目標語音頻道。`Gateway::set_voice_state` 預設送出
   `self_mute=false`、`self_deaf=false`；需要其他設定時使用
   `set_voice_state_with_flags`。
4. 配對同一 guild、同一 bot user 的 `VOICE_STATE_UPDATE.session_id`，以及
   `VOICE_SERVER_UPDATE.token/endpoint`。兩者都收到前不得呼叫 Rustlink。
5. 建立 Rustlink session 並先開啟事件 WebSocket；收到 `session.ready` 後呼叫
   `Session::set_voice`。這裡的 `voice.token` 是 Discord voice server token，
   不是 bot token。
6. 呼叫 `Client::resolve`，從 `LoadResult::Track`、搜尋結果或播放清單選出曲目，
   再以 `Session::play` 傳入 Rustlink 的 `rlt1.` track token。
7. 同時維持 Discord gateway heartbeat、Rustlink events WebSocket 與 voice
   transport。收到 Discord 語音重新分配、移頻道或斷線時，丟棄舊配對並重新收集
   完整的兩個 voice event，再更新 `Session::set_voice`。
8. 收到 SIGINT/SIGTERM 時，先停止播放、銷毀 Rustlink session，再關閉 gateway。
   `Gateway::next_event` 會把 Discord 的 reconnect/invalid-session opcode，或
   遠端關閉（回傳 `None`）交回呼叫端；生產 bot 必須以退避迴圈重新建立
   Gateway，並在需要時重新取得 voice state，不能把一次 WebSocket 物件當成
   永久連線。現行 crate 提供 fresh IDENTIFY；RESUME/backoff 由部署層
   supervisor 保存 READY 資訊後實作。

## 連線與安全邊界

- Gateway reconnect、heartbeat timeout、sequence number 與（若部署需要）
  RESUME 必須由 bot adapter/supervisor 管理；Rustlink voice layer 不應取得
  bot token。
- `VOICE_STATE_UPDATE` 必須檢查 `guild_id`、`user_id` 與目前欲加入的
  `channel_id`，避免把另一個 bot 或另一個 guild 的 session 綁進播放器。
  固定單一 guild 的 bot 應使用 `VoiceStatePairer::for_guild`；未綁定的
  pairer 才適合由呼叫端自行依 guild 分派結果。
- `VOICE_SERVER_UPDATE.endpoint` 為空或不符合 Discord voice host 規則時，應拒絕
  連線並保留可重試錯誤；不可退回未加密音訊。
- Rustlink API password 只放在 Bearer header；Discord bot token 只放在 Gateway
  Identify payload。兩種憑證不可互換，也不可互相寫入 telemetry 或 webhook。
- 事件佇列、heartbeat 與 reconnect 都要有上限與 deadline。不可用無界 channel
  或無限重試把斷線 bot 留在背景。

## 播放品質

Rustlink 的預設音訊設定是 48 kHz、雙聲道、192 kbit/s Opus、complexity 10。
來源層優先挑選原始、非自動配音且 bitrate 較高的音軌；DSP、重採樣與 Opus
封包都在 Rustlink 播放 pipeline 中完成。輸出品質仍受來源本身、Discord voice
能力與網路狀況限制；轉碼不能補回來源已遺失的細節。

Bot 指令應把播放查詢交給 `Client::resolve`，不要在 bot 層自己下載檔案、解析
短效 CDN URL 或呼叫外部下載器。track token 只攜帶中繼資料，Rustlink 會在實際
播放時重新解析並驗證短效媒體 URL。

## 廣告與來源限制

Rustlink 不排程或插入 pre-roll/mid-roll 廣告，且 source resolver 會略過搜尋
回應中的 promoted/ad renderer，不會主動請求播放器廣告 placement。這不等於
能移除影片原本已錄入的贊助、置入或其他內容；Bot 不應對使用者承諾「所有內容
絕對無廣告」。來源服務的條款與可用性仍由操作者負責。

## Codec 與 FFmpeg 邊界

目前 Rustlink 的常見音訊路徑是 Rust Symphonia、原生 DSP 與 Opus pipeline；
AAC、MP3、ALAC、FLAC、ISO-MP4、Matroska、Ogg、WAV 等常見 codec/container
已啟用 Symphonia 原生 feature。不支援的容器或 timescale 仍會選擇性地交給
FFmpeg fallback。FFmpeg 是可選的
外部音訊工具，不是 Lavalink/NodeLink/GolangLavalink adapter，也不是 bot
登入依賴。

若要宣稱「完全不依賴 FFmpeg」，必須先完成並驗證：

- Symphonia AAC、MP3、ALAC、ISO-MP4 等 codec/container 的明確 feature 與
  fixture 測試。
- 非 Ogg Opus（例如 WebM/Matroska）的 demux、seek、錯誤與尾端封包處理。
- 原生 pipeline 對未知容器、長串流、range 續傳、backpressure、timescale 與
  damaged input 的等價行為。
- 在沒有 FFmpeg 的環境跑完整 workspace、音訊 fixture、Discord voice simulator
  與高音質回歸。未完成前，設定 `ffmpeg: none` 只代表不使用 fallback，並不代表
  所有格式都能播放。

## Native bot 驗收條件

`crates/discord-bot/examples/play.rs` 已提供最小可編譯橋接範例；在填入真實
credentials 前，離線測試必須涵蓋：

1. HELLO/IDENTIFY/heartbeat/READY 的 gateway 狀態機。
2. 延遲或亂序的兩個 voice event 仍只在完整配對後產生一個 `VoiceState`。
3. 錯 guild、錯 user、空 endpoint、重複事件與 reconnect 不會污染現有 session。
4. Bot token 永不出現在 log；`voice.token` 與 Rustlink password 不會互相替代。
5. 本機 gateway/UDP simulator 能完成 DAVE、Opus、pause/resume、換曲與關閉。
6. 來源解析、無廣告過濾與 codec fallback 的結果可由 Rustlink 現有 tests 重現。

完成上述測試後，example 才應接入真正的 Discord credentials；離線 simulator
通過不等於已用真實 bot 帳號或真實語音頻道驗證。
