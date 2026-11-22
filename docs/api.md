# Rustlink API v1

預設 origin：`http://127.0.0.1:2333`。JSON 使用 snake_case；時間與位置以毫秒表示，只有 `resume_timeout_secs` 以秒表示。Discord ID 是字串；位置、音量與期限是 JSON 整數，濾鏡參數可以是浮點數。

除 `/healthz` 與 `/version` 外，所有端點（含 WebSocket 與 metrics）都要求：

```http
Authorization: Bearer <設定檔 password>
Content-Type: application/json
```

password 是整個節點的存取金鑰，持有者可以管理節點上的所有 session。每個回應都帶 `Rustlink-Api-Version: 1` 與 `Server: Rustlink/<version>`。錯誤使用對應的 HTTP 狀態碼與統一結構：

```json
{"error":{"code":"bad_request","message":"query must contain 1..8192 bytes","path":"/api/v1/tracks/resolve"},"timestamp_ms":1790000000000}
```

## 節點與曲目

| 方法與路徑 | 輸入與回應 |
| --- | --- |
| `GET /healthz` | 免驗證；`status`、`name`、`version` |
| `GET /version` | 免驗證；純文字版本 |
| `GET /api/v1/info` | 軟體與 API 版本、音源、濾鏡、能力與容量 |
| `GET /api/v1/stats` | players、sessions、playing_players、uptime_ms、memory、cpu |
| `GET /api/v1/metrics`、`/metrics` | Prometheus 文字 |
| `POST /api/v1/tracks/resolve` | `{"query":"ytsearch:lofi"}`，回傳解析結果 |
| `POST /api/v1/tracks/inspect` | `{"token":"rlt1.…"}`，回傳曲目 |
| `POST /api/v1/tracks/inspect-batch` | `{"tokens":["rlt1.…"]}`，回傳 `{"tracks":[…]}` |
| `POST /api/v1/lyrics` | `{"token":"rlt1.…"}`，回傳歌詞，沒有則 204 |

`resolve` 的 `query` 可以是網址、影片 ID 或音源搜尋前綴，見[音源](sources.md)。空白或超過 8192 位元組的 `query` 回 400；音源失敗回 502；沒有結果是 200 的 `{"kind":"empty","result":null}`。

成功的結果：

- `kind: track`：`result` 是一首曲目。
- `kind: search`：`result` 是曲目陣列。
- `kind: playlist`：`result` 是 `{"metadata":{"name":"…","selected_track":-1},"extra":{},"tracks":[…]}`。

曲目：

```json
{
  "token": "rlt1.…",
  "metadata": {
    "identifier": "source-id",
    "source": "youtube",
    "title": "曲名 🎶",
    "author": "演出者",
    "duration_ms": 180000,
    "position_ms": 0,
    "is_stream": false,
    "is_seekable": true,
    "uri": "https://www.youtube.com/watch?v=source-id",
    "artwork_url": null,
    "isrc": null
  },
  "extra": {},
  "user_data": {}
}
```

token 是 `rlt1.` 加上不含 padding 的 Base64URL UTF-8 JSON，只攜帶曲目中繼資料；不含短效的媒體網址，也不是認證憑證或簽章。播放時才由音源重新解析並驗證網址。單一 token 最多 256 KiB、batch 最多 1000 筆、所有請求 body 最多 1 MiB。舊的二進位曲目格式會被拒絕。

歌詞包含 `source_name`、`provider`、`text`、`lines`、`extra`。同步歌詞的每一行有 `timestamp_ms`、`duration_ms`、`text`、`extra`；沒有時間資料時 `lines` 可為 null。

## 工作階段

`POST /api/v1/sessions`：

```json
{"bot_id":"123456789012345678","resume_timeout_secs":60}
```

回 201，`Location` 指向該 session：

```json
{
  "session_id":"48-character-random-hex-id",
  "bot_id":"123456789012345678",
  "connected":false,
  "resume":true,
  "resume_timeout_secs":60,
  "event_url":"/api/v1/sessions/48-character-random-hex-id/events"
}
```

`resume_timeout_secs` 可省略，預設 60，範圍 1–86400。建立後要在這個期限內連上事件 WebSocket，並持續處理 Ping／Pong。連線存在時 session 不會過期；只有首次尚未連線或斷線後才開始倒數，REST 請求不會重新計時。

| 方法與路徑 | 行為 |
| --- | --- |
| `GET /api/v1/sessions/{id}` | 取得 session 狀態 |
| `PATCH /api/v1/sessions/{id}` | `{"resume":true,"resume_timeout_secs":120}`，欄位可省略 |
| `DELETE /api/v1/sessions/{id}` | 銷毀播放器與語音連線，回 204，可重複呼叫 |
| `GET /api/v1/sessions/{id}/events` | WebSocket upgrade；每個 session 只允許一個訂閱者 |

`resume: false` 會在事件連線結束後清掉 session；目前沒有連線時，session 立即成為過期狀態。過期或已銷毀回 404，重複訂閱回 409，容量用盡回 503。session 只存在記憶體，節點重啟後不保留。

## 播放器

播放器以 Discord guild ID 為 key，同一個 session 每個 guild 一個。

| 方法與路徑 | 行為 |
| --- | --- |
| `GET /api/v1/sessions/{id}/players` | `{"players":[…]}` |
| `GET /api/v1/sessions/{id}/players/{guild}` | 取得播放器 |
| `PATCH /api/v1/sessions/{id}/players/{guild}` | 建立或更新，回傳播放器 |
| `DELETE /api/v1/sessions/{id}/players/{guild}` | 銷毀，回 204 |
| `GET /api/v1/sessions/{id}/players/{guild}/lyrics` | 目前曲目的歌詞；沒有則 204 |

播放：

```json
{"track":{"token":"rlt1.…","user_data":{"requester":"123"}},"volume":80}
```

也可以用 `{"track":{"query":"https://…"}}`，但 `query` 必須解析成單一曲目；搜尋與播放清單要先 `resolve` 再挑 token。`token` 與 `query` 互斥。加上 `?replace=false` 會保留已載入的曲目，預設則替換。

其他操作可以合併在同一次 PATCH：

```json
{"paused":true,"position_ms":30000,"end_ms":90000,"volume":75,"filters":{"timescale":{"speed":1.1}}}
```

- 省略的欄位保留現有值。
- `{"track":null}` 停止曲目，但保留播放器與語音連線。
- `{"end_ms":null}` 清除結束位置；`end_ms` 的整數值必須大於零。
- `{"filters":{}}` 清除濾鏡；非空的 `filters` 會取代整組設定。
- `volume` 範圍 0–1000，100 是原音量；`position_ms` 不得小於零。
- 新曲目預設解除暫停，同一個請求可指定 `paused:true`。

播放器回應包含 `guild_id`、`track`、`volume`、`paused`、`filters`、`voice` 與 `state`；`state` 有 `timestamp_ms`、`position_ms`、`connected`、`ping_ms`。PATCH 接受曲目之後，語音連線與解碼在背景進行，成功或失敗由事件回報。

## 轉送 Discord 語音狀態

你的 Rust 機器人負責登入 Discord 並要求加入語音頻道。收齊同一個 guild 的兩種 gateway 事件後，呼叫 `Session::set_voice`，或 PATCH：

```json
{"voice":{"token":"voice-server-token","endpoint":"host.discord.media:443","session_id":"discord-voice-session","channel_id":"987654321098765432"}}
```

| Rustlink 欄位 | Discord 來源 |
| --- | --- |
| 建立 session 的 `bot_id` | 機器人的 user ID |
| `voice.session_id`、`voice.channel_id` | 該機器人的 `VOICE_STATE_UPDATE` |
| `voice.token`、`voice.endpoint` | 同一個 guild 的 `VOICE_SERVER_UPDATE` |

只轉送機器人自己的語音狀態，且 endpoint 必須已就緒、非 null。重新加入或移動頻道時要更新整組 voice；離開頻道可以銷毀播放器。**不要**把 Discord bot token 填進 `voice.token`。

節點支援 Discord voice gateway v8 與 DAVE v1：`/api/v1/info` 的 `capabilities.discord_dave` 為 true、`voice.max_dave_protocol_version` 為 1。這是伺服器能力，不是每條連線的即時加密狀態，實際版本由 Discord 協商。`voice.connected` 只在 UDP 傳輸與所需的 DAVE 金鑰都就緒後才發出，整個握手最多 30 秒。

DAVE 的 key package、成員提案、Commit／Welcome、換金鑰與降級／升級都由 Rustlink 處理，機器人不必傳額外欄位（但 `channel_id` 必填，因為它是 MLS group ID）。沒有有效的 DAVE 金鑰、版本未知或驗證失敗時，不會暗中改送明文音訊。細節見 [DAVE 與語音](voice.md)。

語音斷線時會發出 `voice.closed`、`voice.disconnected`。機器人應重新取得 Discord 的語音狀態再更新整組 `voice`；事件 WebSocket 的 session 恢復**不會**自動恢復 Discord 的語音 WebSocket。

## WebSocket 事件

把 session 的 `event_url` 接在 origin 後面，將 HTTP(S) 改成 WS(S)，並帶同樣的 Bearer 標頭。第一則一定是 `session.ready`，即使已有排隊的事件：

```json
{"type":"session.ready","timestamp_ms":1790000000000,"api_version":"1","session_id":"…","resumed":false,"players":[]}
```

| type | 主要的額外欄位 |
| --- | --- |
| `player.updated` | `guild_id`、`state`；間隔由 `playerUpdateSeconds` 決定 |
| `node.stats` | 節點統計與 `frames`；間隔由 `statsSeconds` 決定 |
| `track.started` | `guild_id`、`track` |
| `track.ended` | `guild_id`、`track`、`reason` |
| `track.failed` | `guild_id`、`track`、`exception` |
| `track.stalled` | `guild_id`、`track`、`threshold_ms` |
| `voice.connected`、`voice.disconnected` | `guild_id`、`channel_id`；斷線可含 `reason` |
| `voice.closed` | `guild_id`、`code`、`reason`、`by_remote` |
| `request.rejected` | `error.code: rest_required`，表示應改用 REST 控制 |

所有 session 事件都有 `type`、`timestamp_ms`、`session_id`。曲目結束原因是 `finished`、`replaced`、`stopped`、`load_failed` 或 `cleanup`。

伺服器每 25 秒送一次 Ping，60 秒沒收到任何訊息就斷線；Rust 客戶端只要持續呼叫 `events.next()` 就會自動回 Pong。事件寫入超過 10 秒會斷線；事件佇列滿了會銷毀 session，避免悄悄漏事件。

在期限內重新連同一個 URL，`session.ready.resumed` 為 true，並先送最新的播放器快照，再送排隊的事件。這些事件可能比快照早，請依 timestamp 處理；沒有事件游標，也沒有恰好一次的投遞保證。過期或節點重啟後要建立新 session。
