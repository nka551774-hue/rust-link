# 音源

把識別字串放進 `POST /api/v1/tracks/resolve` 的 `query` 欄位。識別字串可以是搜尋關鍵字、影片 ID、目錄連結或一般網址；用哪個來源，在發出任何網路請求之前就決定了。

## 路由

先檢查前綴，因為搜尋關鍵字可以是任何內容，包括看起來像網址的字串。

| 識別字串 | 來源 |
| --- | --- |
| `ytsearch:<關鍵字>`、`ytmsearch:<關鍵字>` | YouTube；後者為 YouTube Music |
| `youtube:playlist:<ID>` | YouTube 播放清單（含 `RD…` 合輯） |
| 11 個 `A-Za-z0-9_-` 字元 | YouTube 影片 ID |
| `scsearch:<關鍵字>`、`soundcloud:<路徑>` | SoundCloud 搜尋或曲目／清單路徑 |
| `spsearch:<關鍵字>`、`spotify:<…>` | Spotify |
| `dzsearch:<關鍵字>`、`dzisrc:<ISRC>`、`deezer:<…>` | Deezer |
| `https://youtube.com/…`、`https://youtu.be/…` | YouTube |
| `https://soundcloud.com/…` | SoundCloud |
| `https://open.spotify.com/…` | Spotify |
| `https://deezer.com/…` | Deezer |
| 其他 `http`／`https` 網址 | 直接讀取 |
| 其餘 | `empty`：沒有來源可以回答 |

主機比對接受子網域，所以 `music.youtube.com` 是 YouTube。YouTube 連結可以帶分享參數與 fragment。有效的影片 ID 優先於同時附帶的 `list`，所以分享播放清單裡的一首歌，載入的是那首歌；要載入整份清單，請用不含影片 ID 的清單網址或 `youtube:playlist:<ID>`。支援的連結形式與機器人接線見 [YouTube](youtube.md)。

## 各來源

### YouTube

- **解析**：用 InnerTube 用戶端取得中繼資料與直接的音訊網址。播放時依序嘗試 `IOS_PRODUCER`、`VISIONOS`（自動透過 `/youtubei/v1/visitor_id` 與 `/sw.js_data` 引導並快取匿名 `visitorData`）、`ANDROID_VR`、`ANDROID`（未設定 `youtubePoToken` 且內容大於 1 MiB 時先探測尾端 1 位元組，攔截約 900 KiB 處 403 截斷）；搜尋與播放清單優先使用 `ANDROID`，失敗時自動退回 `VISIONOS`，而 `RD…` 合輯清單若 `/browse` 無項目則自動改走 `/next` 面板解析。中繼資料被某個用戶端拒絕時會換下一個；播放時 CDN 回應可重試的拒絕，會在同一個期限內換下一個用戶端。
- **搜尋與清單**：搜尋最多回傳 `searchResults` 筆；播放清單最多讀 `maxPlaylistTracks` 首、最多六頁；影片是一首曲目，音訊網址從 player 回應取得。YouTube Music 搜尋用網頁音樂用戶端。
- **全部在 Rust 內完成**：不執行播放器 JavaScript，也不啟動外部下載器。所有原生用戶端都失敗時，來源錯誤直接回給 API 客戶端。
- **代理**：設定 `youtubeProxy` 後，YouTube 的 API 與音訊請求都改走該代理，其他來源不受影響。
- **PO token**：`youtubePoToken` 放進 player 請求，並以 `pot` 參數附在 googlevideo 網址上；`youtubeVisitorData` 提供搭配的 visitor data。Rustlink 不產生也不更新這些憑證。沒有有效 token 時，YouTube 不信任的網路可能只給前約 900 KiB（約一分鐘）音訊就回 403；用戶端排序與實測結果見 [YouTube](youtube.md)。
- **限制**：只有需要簽章還原（`signatureCipher`）或專有串流協定的格式不會被還原或重組；地區、帳號或平台限制仍可能讓請求被拒，API 會回傳來源錯誤。原生解析不保證每支影片都能播放。

googlevideo 的音訊以 128 KiB 位元組範圍讀取：每一段從下一個尚未讀的位元組開始，並檢查回傳的位移與總長，正常換段不會睡眠，也不消耗失敗重試額度。這避免了 CDN 拒絕沒有結束位置或太大的 Range 請求。其他主機維持一般的串流請求。總長未知的部分回應會被拒絕，而不是靜默截斷播放；總長未知的完整 200 回應則仍支援（直播串流）。

每個部分內容都會依宣告的位元組範圍檢查，包含 chunked 傳輸。強 ETag 會留下來供 `If-Range` 使用，ETag 衝突時在替換的位元組進入解碼器前就結束播放。沒有強 ETag 時，位移與長度檢查無法偵測所有同大小的替換。Range 內容必須使用 identity 編碼：HTTP 客戶端要求 identity，也不會透明解壓縮媒體回應（見[架構](architecture.md)）。

### SoundCloud

公開 API 加 client id。`soundcloudClientId` 為空時，從網站抓取並快取 `soundcloudClientIdTtlSeconds`。播放使用 API 回報的漸進式串流；只有 HLS 的曲目回傳來源錯誤，因為原生管線不組裝 HLS 分段。關閉 `soundcloudNative` 即停用此來源。

### Spotify

只取中繼資料。歌曲、專輯、播放清單透過網頁 API 解析；播放時改在 YouTube 搜尋，並用標題、歌手與長度比對相近的錄音。Spotify 識別字串需要設定好的 client 憑證，播放另需 `youtubeNative`。

直接把 `open.spotify.com/track/…`、`/album/…`、`/playlist/…` 貼進同一個 `query` 即可。支援 `/intl-zh-TW/` 這類地區前綴、`/embed/` 連結、分享參數、fragment 與 Discord 的 `<…>` 包覆；`spotify:track:…` URI 也可以。資源 ID 在送出任何請求前先驗證；Podcast、本機檔案與 Spotify 短連結不會被當成歌曲。

在設定檔填 `spotifyClientId`、`spotifyClientSecret`（或 `SPOTIFY_CLIENT_ID`、`SPOTIFY_CLIENT_SECRET`）。私人或編輯精選播放清單另需有適當權限的 `spotifyRefreshToken`。分頁的網址必須留在同一個 Spotify API origin 與版本路徑，回應無法把 access token 轉送到別的主機。

播放不使用 Spotify 的 30 秒預覽，也不解密 Spotify 的音訊，而是用標題、歌手與長度在 YouTube 找完整錄音。單一字元的歌名或歌手可以比對。標題含有未要求的翻唱、現場版、混音、卡拉 OK、變速版或預覽標記的候選會被排除。沒有候選達到比對門檻時回傳錯誤。比對只依據中繼資料，不能保證就是同一個錄音。

### Deezer

透過公開 API 取得中繼資料，含 `dzisrc:` 的 ISRC 查詢。播放走與 Spotify 相同的 YouTube 比對路徑；Rustlink 不取得 Deezer 的音訊或預覽串流。

### 一般 HTTP

讀取網址並探測媒體位元組；可辨識的音訊串流可直接播放。HTML 頁面與 M3U、PLS、XSPF 這類清單不會被展開成媒體網址；沒有支援的音訊容器時回傳來源錯誤。其他網站沒有通用的擷取器：一般網址必須直接提供 HTTP 解析器與音訊管線讀得懂的媒體。

重新導向（301、302、303、307、308）最多跟隨 5 次，每一跳都重新檢查網址與連線目標，不允許從 https 降到 http，換主機時不再送出供應者專用標頭。已知大小的檔案（例如索引在檔尾的 m4a）會讀到足以辨識為止，直播串流則只看第一個視窗。續傳遇到暫時性錯誤（429、5xx、連不上）會重試，而不是直接結束播放。

## 音質與廣告

YouTube 選擇音軌時優先原始、非自動配音的音軌，並保留原本的動態，而不是只因位元率較高就選動態壓縮版；等價的版本中選回報位元率最高的。輸出品質由 `opusBitrate`、`opusComplexity` 決定（見[設定](configuration.md)）。轉碼無法補回來源缺少的細節，Discord 端的結果也取決於語音頻道能力與網路。

Rustlink 不排程片頭或中途廣告：只開啟被選中錄音的音訊網址，略過 YouTube 結果中的推廣子樹，也不取得播放器的廣告版位。已錄在影片裡的廣告或贊助仍是該錄音的一部分，Rustlink 不偵測也不剪掉。

## 快取與限制

解析結果依 `cacheTtlSeconds` 快取，最多 `cacheEntries` 筆與 32 MiB 記帳預算，最久未用的先淘汰。快取保存具型別的共用結果，命中時不必解析 JSON，也不複製每首曲目。空結果與其他結果一樣會快取；錯誤在當次解析結束後不保留，之後的請求可以重試。

同時請求同一個識別字串的客戶端共用一次解析與它的結果，包含錯誤與太大而無法快取的結果；錯誤只分享給當下正在等的呼叫者。

同時最多 `maxConcurrentLoads` 個解析。等待名額、等待既有解析與各 provider 請求共用同一個 `loadTimeoutSeconds`；重試與目錄分頁不會各自取得新的時限。

## 私有網路

`allowPrivateNetworks` 預設關閉。檢查在解析器內進行，也就是建立連線前的最後一處：網址被接受之後才解析到私有或特殊用途位址的名稱，仍然會被攔下。這讓客戶端無法把節點指向它所在的網路。設定的 `youtubeProxy` 是例外（通常在本機）。限制：`http://` 與 `socks5h://` 代理自己解析目標網域，節點看不到解析結果，所以經由這類代理的 YouTube 請求沒有這項檢查（目標網址來自 YouTube 的回應，不是客戶端輸入；其他來源不經代理）。想保留本機檢查，可改用 `socks5://`，它由節點解析目標。

## 媒體工具

`ffmpeg` 指名選配的編解碼輔助工具。裸名稱代表 `toolsDirectory` 內的檔案，不會到 `PATH` 找，節點因此永遠執行給它的那一版，而不是機器上剛好裝的版本。明確指定的路徑是刻意的選擇，不會被自動安裝取代。

`autoDownload` 開啟時，缺少的工具會安裝到 `toolsDirectory`：

| 工具 | 來源 |
| --- | --- |
| Windows 的 FFmpeg | 此架構的 BtbN GPL 壓縮檔，並以其 `checksums.sha256` 驗證 |
| Linux 與 macOS 的 FFmpeg | martin-riedl.de，先試正式版、再試 snapshot |

兩種來源的檢查碼都來自下載內容的同一個發佈者（Linux 與 macOS 是建置旁邊的 `.sha256`），所以它擋得住傳輸損毀與不一致的檔案，擋不住發佈者本身被入侵。要更強的保證，請自行安裝 FFmpeg 並用 `ffmpeg` 指定路徑。

小於 1 MiB 的內容會被當成錯誤頁或傳輸被截斷而不是工具，直接丟棄。
