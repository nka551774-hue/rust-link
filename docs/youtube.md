# 把 YouTube 連結交給 Rustlink 播放

Rustlink 提供音源解析、播放器與 Discord 語音傳送；`rustlink-client` 是這個服務的 Rust API 客戶端。本專案目前沒有登入 Discord、接收訊息、註冊 Slash command 或管理歌曲佇列的機器人程式。單獨啟動 `start.bat`／`start.sh` 後，在 Discord 貼上連結不會自動播放。

你的 Rust Discord bot 需要接收指令或訊息中的 URL，要求加入語音頻道，再把語音資料與曲目交給 Rustlink。

## 可交給解析器的連結

以下例子中的 `dQw4w9WgXcQ` 是 11 字元影片 ID。保留原始分享連結即可，不必由 bot 自行取得影音 CDN 的下載網址。

| 輸入 | 解析結果 |
| --- | --- |
| `https://www.youtube.com/watch?v=dQw4w9WgXcQ` | 單一影片 |
| `https://m.youtube.com/watch?v=dQw4w9WgXcQ` | 單一影片 |
| `https://music.youtube.com/watch?v=dQw4w9WgXcQ` | 單一影片 |
| `https://youtu.be/dQw4w9WgXcQ?si=share-code` | 單一影片；分享追蹤參數不會成為影片 ID |
| `https://www.youtube.com/shorts/dQw4w9WgXcQ` | 單一影片 |
| `https://www.youtube.com/live/dQw4w9WgXcQ` | 單一影片；能否播放仍取決於可用串流 |
| `https://www.youtube.com/embed/dQw4w9WgXcQ` | 單一影片 |
| `https://www.youtube.com/v/dQw4w9WgXcQ` | 單一影片 |
| `https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLexample` | 優先解析指定影片，避免把分享單曲當成整份清單 |
| `https://www.youtube.com/playlist?list=PLexample` | 播放清單 |
| `https://www.youtube.com/watch?list=PLexample` | 沒有指定影片的播放清單 |
| `dQw4w9WgXcQ` | 單一影片 |

純播放清單也可用 `youtube:playlist:<清單 ID>`。搜尋使用 `ytsearch:<關鍵字>` 或 `ytmsearch:<關鍵字>`，回傳搜尋結果陣列。

網址需使用 HTTP(S)。支援上述路徑後的分享 query 與 `#fragment`，以及 Discord 常見的單層 `<https://youtu.be/dQw4w9WgXcQ>` 包覆。若訊息是「幫我播這首 https://…」或 Markdown 連結，bot 應先取出 URL；解析器不會從任意訊息文字中尋找連結。`watch` 連結的 `v` 無效時，不會因為同時有 `list` 而改播整份清單。

連結中的 `t`／`start` 不會自動設定播放位置。需要從指定秒數開始時，由 bot 轉成毫秒，透過播放器的 `position_ms` 或 `Session::seek` 設定。

## Discord bot 必須先接好的部分

1. 用你的 Discord SDK 登入 bot，接收播放指令或訊息。若要讀取一般伺服器訊息全文，通常還需要在 Developer Portal 與 SDK 啟用 Message Content intent；使用 Slash command 的 URL 參數則不需要讀取一般訊息全文。本專案沒有實作這些 handler。
2. 由 Discord bot 要求加入目標語音頻道，並接收該 guild 的語音狀態事件。
3. 收齊 bot 自己的 `VOICE_STATE_UPDATE` 與同 guild 的 `VOICE_SERVER_UPDATE`。兩種事件可能先後到達；不要混用不同次加入或移動頻道的資料。
4. 建立 Rustlink session，開啟並持續讀取 events WebSocket，再呼叫 `Session::set_voice`。

| Rustlink 欄位 | 值的來源 |
| --- | --- |
| `Client::new` 的 password | Rustlink 設定檔的 `password` |
| `create_session` 的 `bot_id` | Discord bot 的 user ID，使用字串 |
| `set_voice` 的 `guild` | 此次要求加入的 guild ID，使用字串 |
| `VoiceState.session_id` | bot 自己的 `VOICE_STATE_UPDATE.session_id` |
| `VoiceState.channel_id` | bot 自己的 `VOICE_STATE_UPDATE.channel_id`，填入 `Some(channel_id)` |
| `VoiceState.token` | `VOICE_SERVER_UPDATE.token` |
| `VoiceState.endpoint` | `VOICE_SERVER_UPDATE.endpoint`，必須已就緒且非 null |

Discord bot token 不屬於上述 `voice.token` 或 Rustlink password。DAVE 握手與金鑰處理由 Rustlink 完成，bot 需提供有效的整組 voice 資料，包含 channel ID。

收到 `voice.connected` 才代表語音傳輸與當次協商需要的 DAVE 金鑰已準備完成。API 接受 `set_voice`／`play` 的回應不代表使用者已聽到聲音。移動頻道、重新加入或遇到 `voice.closed` 時，bot 應重新取得並更新語音資料。

## URL → resolve → play

傳給音源的 identifier 在 API v1 的欄位名稱是 `query`。先解析，再取得曲目的 `token` 播放：

```http
POST /api/v1/tracks/resolve
Authorization: Bearer <Rustlink password>
Content-Type: application/json

{"query":"https://youtu.be/dQw4w9WgXcQ?si=share-code"}
```

單首影片回傳 `kind: "track"`，曲目 token 位於 `result.token`。把它交給同一 guild 的播放器：

```http
PATCH /api/v1/sessions/<Rustlink session ID>/players/<guild ID>
Authorization: Bearer <Rustlink password>
Content-Type: application/json

{"track":{"token":"rlt1.…"}}
```

以下函式是可放進既有 Rust bot 的串接片段，並非獨立 Discord bot。呼叫前要有已建立的 `Client`／`Session`、持續讀取 session events 的工作，以及從 Discord 收齊的 `VoiceState`：

```rust,no_run
use rustlink_client::{Client, LoadResult, Player, Session, VoiceState};

async fn play_youtube_url(
    client: &Client,
    session: &Session,
    guild_id: &str,
    voice: VoiceState,
    url: &str,
) -> Result<Player, Box<dyn std::error::Error + Send + Sync>> {
    session.set_voice(guild_id, voice).await?;
    let track = match client.resolve(url).await? {
        LoadResult::Track(track) => *track,
        LoadResult::Playlist(_) => return Err("這是播放清單，請先選曲或加入 bot 的佇列".into()),
        LoadResult::Search(_) => return Err("這是搜尋結果，請先選取一首曲目".into()),
        _ => return Err("沒有可播放的曲目".into()),
    };
    Ok(session.play(guild_id, &track.token).await?)
}
```

已確定是單首影片時，也能在完成 voice 設定後使用 `session.play_query(guild_id, url).await?`。它要求解析結果為單一曲目；搜尋結果與清單仍需先 `resolve` 並選取曲目。

`Session::play`／`play_query` 預設會替換正在播放的曲目。Rustlink 沒有自動排歌功能；如果每貼一個連結都要排到下一首，bot 應維護每個 guild 的佇列，並依 `track.ended` 的 `reason` 決定何時播放下一首，避免把 `replaced`／`stopped` 誤當自然播放結束。

## 現有範例的用途

[`crates/client/examples/bot.rs`](../crates/client/examples/bot.rs) 是可執行的 Rustlink 客戶端範例，從環境變數讀取已取得的 Discord voice 憑證；它不會自己登入 Discord 或取得這些憑證。

```sh
cargo run -p rustlink-client --example bot -- "https://youtu.be/dQw4w9WgXcQ"
```

需要設定 `RUSTLINK_PASSWORD`、`DISCORD_BOT_ID`、`DISCORD_GUILD_ID`、`DISCORD_CHANNEL_ID`、`DISCORD_VOICE_TOKEN`、`DISCORD_VOICE_ENDPOINT`、`DISCORD_VOICE_SESSION_ID`；`RUSTLINK_URL` 預設為 `http://127.0.0.1:2333`。

範例開啟事件串流、設定 voice、解析輸入並播放，按 Ctrl+C 後清理 session。若輸入是搜尋結果或清單，它只選第一首，不會自動播放餘下曲目，也沒有正式 bot 所需的指令處理或重連控制。README 的短程式碼是串接示意，其中取得及設定 voice 的部分需由你的 bot 補上。

## 播放限制與驗證範圍

辨認分享 URL、取得曲目中繼資料、實際取得音訊串流，是不同步驟。原生 YouTube 播放需要 InnerTube 客戶端取得直接音訊 URL；要求播放器腳本還原簽章、特殊串流協定、有效 PO token 或受到帳戶／地區／平台限制時，仍可能失敗。

`youtubePoToken` 只把你提供的 token 帶入 player request，不會自動產生或更新 token，也不保證其他串流端點接受它。`youtubeNative: false` 會停用 YouTube。來源解析失敗由 API 回覆錯誤；媒體下載或解碼的後續錯誤由 `track.failed` 事件回報。

Googlevideo 音訊使用 128 KiB 分段下載，避免此次實測遇到的無結束位置或較大 Range 請求遭 403 拒絕。後續段落會檢查位移與檔案總長並連續交給解碼器；正常換段不會等待重試。選取音軌時優先非自動配音、原音及未套用動態壓縮的版本，再比較位元率。

必須持續呼叫 `events.next()` 才會處理 WebSocket Ping／Pong。session 逾期或 Rustlink 重啟後要重建；語音重連也需要 bot 重新取得 Discord voice 資料。

2026-10-25 曾對影片 `1CXveaq_vFU` 進行實際 YouTube 音源驗證：原生 InnerTube 使用 `ANDROID 20.10.38` 與相符的 User-Agent、`en/US` 請求環境，未提供 cookies 或 PO token，取得 18 個直接媒體 URL。選取繁體中文原音、非 DRC 的 Opus `itag 251` 後，以 HTTP Range 取得 128 KiB，伺服器回覆 206；再以 FFmpeg 成功解碼 1 秒並確認不是全靜音。

這次實測範圍是指定影片的解析、片段下載與 1 秒解碼，尚未驗證全片或真實 Discord 語音播放，也不代表其他影片或其他網路環境一定可播。客戶端版本與上游的接受條件可能改變。

協定整合測試另涵蓋本機 HTTP／WebSocket／UDP 模擬與 DAVE 互通驗證。完整欄位與事件見 [API v1](api.md)、[音源](sources.md)及 [DAVE](voice.md)。
