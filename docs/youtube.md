# YouTube

在 Discord 貼 YouTube 連結就播放，需要兩個部分：節點（解析與播放）和一個會登入 Discord 的機器人。專案附的 [`slash.rs`](../crates/discord-bot/examples/slash.rs) 與 [`music`](../crates/discord-bot/examples/music/main.rs) 是現成的直接貼連結／`/play` 機器人，做法見 [README](../README.md#examples)；這份文件說明可用的連結、節點如何取得音訊，以及遇到限制時怎麼辦。

## 可解析的輸入

保留原始分享連結即可，機器人不需要自己取得影音 CDN 網址。以下的 `dQw4w9WgXcQ` 是 11 字元的影片 ID。

| 輸入 | 結果 |
| --- | --- |
| `https://www.youtube.com/watch?v=dQw4w9WgXcQ`（含 `m.`、`music.`、`youtube-nocookie.com`） | 單一影片 |
| `https://youtu.be/dQw4w9WgXcQ?si=share-code` | 單一影片；分享參數不是影片 ID |
| `https://www.youtube.com/shorts/…`、`/live/…`、`/embed/…`、`/v/…`、`/e/…`、`/attribution_link?u=…` | 單一影片；直播能否播放取決於可用串流 |
| `https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLexample` | 只載入該影片，不把分享的單曲當成整份清單 |
| `https://www.youtube.com/playlist?list=PLexample`、`watch?list=…`、`youtube:playlist:<ID>`（含 `RD…` 合輯） | 播放清單 |
| `dQw4w9WgXcQ` | 單一影片 |
| `ytsearch:<關鍵字>`、`ytmsearch:<關鍵字>` | 搜尋結果陣列（YouTube、YouTube Music） |

網址必須是 HTTP(S)；支援分享參數、`#fragment`，以及 Discord 常見的 `<https://youtu.be/…>` 包覆。節點本身不會從一段任意文字裡找連結：「幫我播 https://…」或省略 `https://` 的 `youtu.be/…` 由 `rustlink-discord-bot::normalize_query` 與 `extract_message_query` 在機器人端自動擷取並正規化。`watch` 連結的 `v` 無效時，不會因為同時有 `list` 就改播整份清單。連結裡的 `t`／`start` 不會自動定位，機器人要自己換成毫秒並設定 `position_ms`。

## 機器人流程

1. 登入 Discord（可用 `Gateway::connect_with_messages` 自動啟用訊息接收並在未開啟 Message Content Intent 時退回免特權模式），收到 `/play`、頻道內直接貼上的音樂連結或 `@Bot <連結／關鍵字>`。
2. 要求加入語音頻道；收齊機器人自己的 `VOICE_STATE_UPDATE` 與同一個 guild 的 `VOICE_SERVER_UPDATE`，兩者必須來自同一次加入。
3. 建立 Rustlink session，持續讀取事件 WebSocket，再呼叫 `Session::set_voice`。
4. `resolve` 網址，取得曲目 token，再 `play`。

| Rustlink 欄位 | 來源 |
| --- | --- |
| `Client::new` 的 password | 設定檔的 `password` |
| `create_session` 的 `bot_id` | 機器人的 user ID（字串） |
| `VoiceState.session_id`、`channel_id` | 機器人自己的 `VOICE_STATE_UPDATE` |
| `VoiceState.token`、`endpoint` | `VOICE_SERVER_UPDATE`；endpoint 必須已就緒且非 null |

Discord bot token 不屬於以上任何欄位，也不是 Rustlink password。收到 `voice.connected` 才代表語音傳輸與 DAVE 金鑰準備好；API 回應 `set_voice` 或 `play` 成功，不代表聽得到聲音。移動頻道或收到 `voice.closed` 後，要重新取得並更新整組 voice 資料。

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
        LoadResult::Playlist(_) => return Err("這是播放清單，請先選曲".into()),
        LoadResult::Search(_) => return Err("這是搜尋結果，請先選一首".into()),
        _ => return Err("沒有可播放的曲目".into()),
    };
    Ok(session.play(guild_id, &track.token).await?)
}
```

已知是單首影片時，可用 `session.play_query(guild_id, url)`。`play` 預設**替換**正在播放的曲目；Rustlink 沒有佇列，要排歌的機器人要自己維護每個 guild 的佇列，並依 `track.ended` 的 `reason` 決定何時播下一首，不要把 `replaced`、`stopped` 當成自然播完。

## 節點如何取得音訊

節點用純 Rust 原生 InnerTube 協定取得直接音訊網址，不執行播放器 JavaScript、不用 `yt-dlp`，也不把檔案存到硬碟。依序嘗試：

| 順序 | 用戶端 | 說明 |
| --- | --- | --- |
| 1 | `IOS_PRODUCER 0.147.945` | 免 PO token、免登入即可穩定取得完整長度的 AAC-LC（itag 140） |
| 2 | `VISIONOS 0.1` | 自動透過 `/youtubei/v1/visitor_id`（與 `/sw.js_data` 後備）引導並快取匿名 `visitorData`，提供高音質 WebM Opus（itag 251）與 AAC-LC；若快取憑證遭機器人驗證攔截會自動失效重取一次 |
| 3 | `ANDROID_VR 1.60.19` | 精簡欄位備援用戶端 |
| 4 | `ANDROID` | 版本可用 `youtubeClientVersion` 覆寫；未設定 `youtubePoToken` 且檔案大於 1 MiB 時，開啟串流前會先探測尾端 1 位元組確認未受 ~900 KiB 截斷限制 |

搜尋與播放清單優先使用 `ANDROID`，失敗時自動退回 `VISIONOS`；`RD…` 合輯清單若 `/browse` 無項目會自動改走 `/next` 面板解析。某個用戶端被要求「確認不是機器人」，或影音伺服器（googlevideo）回應 403／尾端截斷時，會自動換下一個用戶端。選音軌時優先非自動配音、原音、未動態壓縮與非 HE-AAC（優先 Opus 與 AAC-LC）的版本，再比較位元率。

googlevideo 的音訊每次讀 128 KiB，下一段從上一段結束處開始，並檢查回傳的位移與總長；較大或沒有結束位置的 Range 請求會被拒絕。

## 被 YouTube 擋下時

2026-11-25 實測結果：

- `IOS_PRODUCER 0.147.945` 與搭配自動引導 `visitorData` 的 `VISIONOS 0.1` 對 VEVO、Sony Music Japan、WMG 等熱門官方音樂影片皆能避開「Sign in to confirm you're not a bot」並整首完整串流至結尾，雙重獨立原生備援無須 `yt-dlp`。
- `ANDROID_VR 1.60.19` 在多數熱門官方 MV 會被要求登入驗證；`IOS 21.x` 僅回傳無直連網址的 SABR 格式（已自預設鏈移除）。
- `ANDROID` 在未提供有效 PO token 時，開頭雖回傳 206，但讀過約 900 KiB（約一分鐘）後會回傳 403；節點現已透過尾端位元組預檢提前攔截，避免播到一分鐘斷音。

YouTube 會依網路位址調整規則，這些結果可能隨時改變。處理方式：

1. 設定 **`youtubeProxy`**（`http://`、`socks4://`、`socks5://`、`socks5h://`，可含 `使用者:密碼@`）：YouTube 的 API 與音訊請求全部改從代理出去，其他音源不受影響。代理無法使用時，YouTube 載入會直接失敗，不會改為直連。請在該出口實測：YouTube 對資料中心與住宅位址的信任程度各不相同。
2. 取得 PO token 後填 **`youtubePoToken`**（與 `youtubeVisitorData`）：token 會放進 player 請求，並以 `pot` 參數附在 googlevideo 網址上（不會送到其他主機）。Rustlink 不會自己產生或更新 token，也不保證每個用戶端都接受。
3. `youtubeNative: false` 會停用 YouTube。

來源解析失敗時，API 直接回傳錯誤；媒體下載或解碼中途失敗，由 `track.failed` 事件回報。

## 已驗證與未驗證

驗證過：單元與整合測試（本機模擬 HTTP、WebSocket、UDP 與 DAVE 對端）；真實 YouTube 的整首讀取（`cargo test -p rustlink-source --test live_youtube -- --ignored`，可用 `RUSTLINK_YOUTUBE_PROXY` 一併測代理）；一首歌在真實 Discord 語音頻道完整播放。

未驗證：其他影片、其他網路環境、長時間連續播放、直播串流。用戶端版本與 YouTube 的接受條件隨時可能改變，需要時請更新 `crates/source/src/innertube.rs` 裡的用戶端定義。

完整欄位與事件見 [API v1](api.md)、[音源](sources.md)、[DAVE 與語音](voice.md)。
