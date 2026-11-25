# Rustlink API v1

預設位址：`http://127.0.0.1:2333`。JSON 使用 `snake_case`；時間與位置單位皆為毫秒（僅 `resume_timeout_secs` 為秒）。Discord ID 為字串；位置、音量與時限為整數，濾鏡參數可為浮點數。

除 `/healthz` 與 `/version` 外，所有端點（含 WebSocket 與 `/metrics`）皆須攜帶 `Authorization`（含 JSON body 時另須 `Content-Type: application/json`）：

```http
Authorization: Bearer <設定檔 password>
Content-Type: application/json
```

`password` 為全節點管理金鑰。所有回應皆附帶 `Rustlink-Api-Version: 1` 與 `Server: Rustlink/<version>`。錯誤採統一結構：

```json
{"error":{"code":"bad_request","message":"query must contain 1..8192 bytes","path":"/api/v1/tracks/resolve"},"timestamp_ms":1790000000000}
```

## 節點與曲目端點

| 方法與路徑 | 輸入與回應 |
| --- | --- |
| `GET /healthz` | 免驗證；回傳 `status`、`name`、`version` |
| `GET /version` | 免驗證；純文字版本號 |
| `GET /api/v1/info` | 版本、音源、濾鏡、能力與容量設定 |
| `GET /api/v1/stats` | `players`、`sessions`、`playing_players`、`uptime_ms`、`memory`、`cpu` |
| `GET /api/v1/metrics`、`/metrics` | Prometheus 文字格式指標 |
| `POST /api/v1/tracks/resolve` | `{"query":"ytsearch:lofi"}` → 解析結果 |
| `POST /api/v1/tracks/inspect` | `{"token":"rlt1...."}` → 單首曲目 |
| `POST /api/v1/tracks/inspect-batch` | `{"tokens":["rlt1...."]}` → `{"tracks":[...]}` |
| `POST /api/v1/lyrics` | `{"token":"rlt1...."}` → 歌詞，無結果回 `204` |

`resolve` 的 `query` 支援網址、影片 ID 或搜尋前綴（見 [音源](sources.md)）；空白或超過 8192 bytes 回 `400`，上游失敗回 `502`，無結果回 `200` `{"kind":"empty","result":null}`。成功時 `kind` 為 `track`（單曲）、`search`（曲目陣列）或 `playlist`（`{"metadata":{"name":"...","selected_track":-1},"extra":{},"tracks":[...]}`）。

曲目結構：

```json
{
  "token": "rlt1....",
  "metadata": {
    "identifier": "source-id",
    "source": "youtube",
    "title": "曲名",
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

`token` 為 `rlt1.` 加上無填充 Base64URL UTF-8 JSON，僅含曲目中繼資料（不含短效 CDN 網址或認證憑證），播放時才重新解析驗證串流。單一 token 上限 256 KiB、批次上限 1000 筆、請求 body 上限 1 MiB。歌詞含 `source_name`、`provider`、`text`、`lines`（每行含 `timestamp_ms`、`duration_ms`、`text`、`extra`）與 `extra`。

## 工作階段（Sessions）

建立工作階段：`POST /api/v1/sessions`

```json
{"bot_id":"123456789012345678","resume_timeout_secs":60}
```

回傳 `201 Created`（`Location` 指向該 session）：

```json
{
  "session_id": "48-character-random-hex-id",
  "bot_id": "123456789012345678",
  "connected": false,
  "resume": true,
  "resume_timeout_secs": 60,
  "event_url": "/api/v1/sessions/48-character-random-hex-id/events"
}
```

`resume_timeout_secs` 預設 `60`（範圍 `1..=86400`）。建立後須在期限內連上事件 WebSocket；連線存活期間不過期，首次未連或斷線後才開始倒數（REST 請求不重置倒數）。

| 方法與路徑 | 行為 |
| --- | --- |
| `GET /api/v1/sessions/{id}` | 查詢 session 狀態 |
| `PATCH /api/v1/sessions/{id}` | 更新 `{"resume":true,"resume_timeout_secs":120}` |
| `DELETE /api/v1/sessions/{id}` | 銷毀所有播放器與語音連線，回 `204`（冪等） |
| `GET /api/v1/sessions/{id}/events` | WebSocket 升級（每 session 限單一訂閱者，重複回 `409`） |

`resume: false` 會在事件連線關閉時立即銷毀 session。已過期回 `404`，容量滿回 `503`；節點重啟不保留記憶體 session。

## 播放器（Players）

每個 session 依 Discord `guild_id` 管理獨立播放器：

| 方法與路徑 | 行為 |
| --- | --- |
| `GET /api/v1/sessions/{id}/players` | `{"players":[...]}` |
| `GET /api/v1/sessions/{id}/players/{guild}` | 查詢播放器 |
| `PATCH /api/v1/sessions/{id}/players/{guild}` | 建立或更新播放器（`?replace=false` 保留既有曲目） |
| `DELETE /api/v1/sessions/{id}/players/{guild}` | 銷毀播放器，回 `204` |
| `GET /api/v1/sessions/{id}/players/{guild}/lyrics` | 目前播放曲目歌詞，無結果回 `204` |

播放與控制可合併於單次 `PATCH`（省略欄位保留原值）：

```json
{
  "track": {"token": "rlt1....", "user_data": {"play_id": 1}},
  "paused": false,
  "position_ms": 30000,
  "end_ms": 90000,
  "volume": 80,
  "filters": {"timescale": {"speed": 1.1}}
}
```

- `track` 可傳 `token` 或單曲 `query`（兩者互斥）；`{"track": null}` 停止播放並保留語音連線。
- `{"end_ms": null}` 清除結束點（整數須 `> 0`）；`{"filters": {}}` 清除全部濾鏡；`volume` 範圍 `0..=1000`（`100` 為原音）。
- 回應含 `guild_id`、`track`、`volume`、`paused`、`filters`、`voice` 與 `state`（`timestamp_ms`、`position_ms`、`connected`、`ping_ms`）。

## 轉送 Discord 語音狀態

機器人收齊同一伺服器的 `VOICE_STATE_UPDATE`（機器人自身 `session_id`、`channel_id`）與 `VOICE_SERVER_UPDATE`（非空 `endpoint`、`token`）後，呼叫 `Session::set_voice` 或 `PATCH`：

```json
{"voice":{"token":"voice-server-token","endpoint":"host.discord.media:443","session_id":"discord-voice-session","channel_id":"987654321098765432"}}
```

- **切勿**將 Discord Bot Token 填入 `voice.token`。`channel_id` 必填（作為 DAVE v1 MLS Group ID）。
- 節點完整實作 Voice Gateway v8 與 DAVE v1（`/api/v1/info` 回報 `capabilities.discord_dave: true`）；`voice.connected` 僅在 UDP 與協商所需的 DAVE 金鑰皆就緒後發出（握手上限 30 秒，詳見 [DAVE 與語音](voice.md)）。
- 收到 `voice.closed` 或移動頻道時，機器人應重新收齊語音狀態並更新整組 `voice`。

## WebSocket 事件

將 `event_url` 改為 `ws(s)://` 並帶相同 Bearer 標頭連線；首則訊息必為 `session.ready`。所有事件皆含 `type`、`timestamp_ms`、`session_id`：

| `type` | 主要欄位與觸發時機 |
| --- | --- |
| `session.ready` | `api_version`、`resumed`、`players` |
| `player.updated` | `guild_id`、`state`（每 `playerUpdateSeconds` 推送） |
| `node.stats` | 節點資源與 `frames` 統計（每 `statsSeconds` 推送） |
| `track.started` | `guild_id`、`track` |
| `track.ended` | `guild_id`、`track`、`reason`（`finished` / `replaced` / `stopped` / `load_failed` / `cleanup`） |
| `track.failed` | `guild_id`、`track`、`exception` |
| `track.stalled` | `guild_id`、`track`、`threshold_ms` |
| `voice.connected` / `voice.disconnected` | `guild_id`、`channel_id`（斷線可含 `reason`） |
| `voice.closed` | `guild_id`、`code`、`reason`、`by_remote` |
| `request.rejected` | `error.code: rest_required`（控制命令請走 REST） |

伺服器每 25 秒送 Ping（60 秒無回應斷線），寫入逾時 10 秒；佇列滿時關閉並銷毀 session，絕不靜默漏事件。期限內重連時 `resumed: true`，先送最新播放器快照再沖出暫存事件。
