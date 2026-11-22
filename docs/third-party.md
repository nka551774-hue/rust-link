# 第三方服務

節點是它並不擁有的服務的客戶端。這裡的任何服務都與本專案無隸屬、背書或支援關係，其條款由操作者自己負責。

## Discord

節點用機器人已有的憑證連接語音 gateway，其餘不連 Discord API：擁有 session 的機器人送來語音狀態，節點依此行動。除了連線存續期間的 voice token 之外，不保存任何東西。（`rustlink-discord-bot` 是另一個函式庫，由機器人行程使用，不在節點內。）

## YouTube

曲目中繼資料與音訊透過 YouTube 的內部 API，以 Rust 用戶端讀取。這個介面可能不預警地改變；大規模運行節點的操作者，需要自己考慮 YouTube 的條款怎麼說。若設定了 `youtubeProxy`，YouTube 的所有請求（含音訊）都會經過該代理，代理的營運者看得到這些流量。

設定檔提供的 visitor data 與 proof-of-origin token 會送到設定的 YouTube 端點，Rustlink 不產生它們。

## SoundCloud

中繼資料與音訊來自公開 API。設定沒有提供的 client id 會從網站抓取並快取，這與瀏覽器的做法相同；設定提供的則原樣使用。

## Spotify

只讀取中繼資料：用應用程式 client id，可選的使用者 refresh token，透過網頁 API。不從 Spotify 取得音訊，因為取不到。每個項目以標題與歌手在別的服務找對應的影片，所以聽者聽到的是那個服務的版本。沒有 client id 的節點會拒絕 Spotify 識別字串，不會亂猜。

## Deezer

中繼資料來自公開 API。播放用 YouTube 的比對結果（以標題、歌手與長度挑選）；不取得 Deezer 的音訊或預覽串流。

## LRCLIB

歌詞讀自免費公開的資料庫 `lrclib.net`。節點送出曲目的標題、歌手、專輯與長度，並在項目存續期間快取回傳的內容。

## YouTube Music

資料庫沒有某首曲目的歌詞時，用音源層同一個 InnerTube 用戶端讀取 YouTube Music 的同步歌詞，所以這個目錄請求帶有相同的 visitor data。

## FFmpeg

FFmpeg 是另一個有自己授權的程式，由操作者安裝，或由 `autoDownload` 安裝。節點安裝的是附 libopus 的 GPL 建置，因為編碼器需要它；壓縮檔裡也有 `ffprobe`，節點不使用。授權與條款以其上游發佈者為準。

## 節點送出什麼

除了上面列出的請求，沒有任何關於節點或聽者的資料離開節點：沒有遙測、沒有檢查更新，也沒有中央服務。只有操作者設定了 webhook，才會呼叫它，事件也只會送到那個網址。
