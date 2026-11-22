# YouTube

在 Discord 貼 YouTube 連結就播放，需要兩個部分：節點（解析與播放）和一個會登入 Discord 的機器人。專案附的 [`slash.rs`](../crates/discord-bot/examples/slash.rs) 是現成的 `/play` 機器人，做法見 [README](../README.md#在-discord-用-play-播-youtube)；這份文件說明可用的連結、節點如何取得音訊，以及遇到限制時怎麼辦。

## 可解析的輸入

保留原始分享連結即可，機器人不需要自己取得影音 CDN 網址。以下的 `dQw4w9WgXcQ` 是 11 字元的影片 ID。

| 輸入 | 結果 |
| --- | --- |
| `https://www.youtube.com/watch?v=dQw4w9WgXcQ`（含 `m.`、`music.` 子網域） | 單一影片 |
| `https://youtu.be/dQw4w9WgXcQ?si=share-code` | 單一影片；分享參數不是影片 ID |
| `https://www.youtube.com/shorts/…`、`/live/…`、`/embed/…`、`/v/…` | 單一影片；直播能否播放取決於可用串流 |
| `https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLexample` | 只載入該影片，不把分享的單曲當成整份清單 |
| `https://www.youtube.com/playlist?list=PLexample`、`watch?list=…`、`youtube:playlist:<ID>` | 播放清單 |
| `dQw4w9WgXcQ` | 單一影片 |
| `ytsearch:<關鍵字>`、`ytmsearch:<關鍵字>` | 搜尋結果陣列（YouTube、YouTube Music） |

網址必須是 HTTP(S)；支援分享參數、`#fragment`，以及 Discord 常見的 `<https://youtu.be/…>` 包覆。節點不會從一段任意文字裡找連結：「幫我播 https://…」要由機器人先取出網址。`watch` 連結的 `v` 無效時，不會因為同時有 `list` 就改播整份清單。連結裡的 `t`／`start` 不會自動定位，機器人要自己換成毫秒並設定 `position_ms`。

## 機器人流程

1. 登入 Discord，收到指令與其中的網址。若用斜線指令，不需要 Message Content intent。
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

節點用 InnerTube 用戶端取得直接的音訊網址，不執行播放器 JavaScript、不用 yt-dlp，也不把檔案存到硬碟。依序嘗試：

| 順序 | 用戶端 | 說明 |
| --- | --- | --- |
| 1 | `IOS_PRODUCER 0.147.945` | 不需要 PO token；只提供 AAC（itag 140） |
| 2 | `ANDROID_VR 1.60.19` | Lavalink youtube-source 的首選；只送必要欄位 |
| 3 | `ANDROID` | 版本可用 `youtubeClientVersion` 覆寫 |
| 4 | `IOS` | |

搜尋與播放清單固定使用 `ANDROID`，不受上面順序影響。某個用戶端被拒絕，或影音伺服器（googlevideo）回應 403 時，會換下一個用戶端。選音軌時優先非自動配音、原音、未動態壓縮的版本，再比較位元率。

googlevideo 的音訊每次讀 128 KiB，下一段從上一段結束處開始，並檢查回傳的位移與總長；較大或沒有結束位置的 Range 請求會被拒絕。

## 被 YouTube 擋下時

2026-11-22 在一個台灣住宅網路上的實測：

- `IOS_PRODUCER` 對 11 首測試歌曲都能整首讀完，包含 `ANDROID_VR` 被要求「確認不是機器人」的歌；`f2u9F-IT9wY`（4:22）在 Discord 語音頻道從頭播到尾，結束原因為 `finished`。
- `ANDROID_VR 1.60.19` 只有少數歌能播完（10 首熱門歌裡 1 首），其餘被要求登入。
- 較新的 `ANDROID_VR` 版本、`ANDROID`、`IOS` 在沒有有效 PO token 時，約 900 KiB（約一分鐘）後被 403 拒絕。
- 換成 Cloudflare WARP 的出口沒有改善；產生新版 PO token 也沒有用。

YouTube 會依網路位址調整規則，這些結果可能隨時改變。處理方式：

1. 設定 **`youtubeProxy`**（`http://`、`socks4://`、`socks5://`、`socks5h://`，可含 `使用者:密碼@`）：YouTube 的 API 與音訊請求全部改從代理出去，其他音源不受影響。代理無法使用時，YouTube 載入會直接失敗，不會改為直連。請在該出口實測：YouTube 對資料中心與住宅位址的信任程度各不相同。
2. 取得 PO token 後填 **`youtubePoToken`**（與 `youtubeVisitorData`）：token 會放進 player 請求，並以 `pot` 參數附在 googlevideo 網址上（不會送到其他主機）。Rustlink 不會自己產生或更新 token，也不保證每個用戶端都接受。
3. `youtubeNative: false` 會停用 YouTube。

來源解析失敗時，API 直接回傳錯誤；媒體下載或解碼中途失敗，由 `track.failed` 事件回報。

## 已驗證與未驗證

驗證過：單元與整合測試（本機模擬 HTTP、WebSocket、UDP 與 DAVE 對端）；真實 YouTube 的整首讀取（`cargo test -p rustlink-source --test live_youtube -- --ignored`，可用 `RUSTLINK_YOUTUBE_PROXY` 一併測代理）；一首歌在真實 Discord 語音頻道完整播放。

未驗證：其他影片、其他網路環境、長時間連續播放、直播串流。用戶端版本與 YouTube 的接受條件隨時可能改變，需要時請更新 `crates/source/src/innertube.rs` 裡的用戶端定義。

完整欄位與事件見 [API v1](api.md)、[音源](sources.md)、[DAVE 與語音](voice.md)。
