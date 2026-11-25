# 第三方服務說明

Rustlink 節點作為外部影音與歌詞服務的客戶端運作，與下列第三方服務無隸屬或背書關係，使用條款由營運者自行負責。

| 服務 | 節點互動範圍 |
| --- | --- |
| **Discord** | 節點僅依機器人傳入的短效語音憑證連線 Voice Gateway v8 與 UDP，不保存任何持久憑證（主 Gateway 由機器人端的 `rustlink-discord-bot` 負責） |
| **YouTube / YouTube Music** | 以純 Rust InnerTube 讀取中繼資料、音訊串流與同步歌詞；若設定 `youtubeProxy`，所有 YouTube 流量將經由該代理轉發 |
| **SoundCloud** | 透過公開 API 讀取中繼資料與漸進式音訊串流；未設定 `soundcloudClientId` 時自動自網頁抓取快取 |
| **Spotify** | 以應用程式憑證（與選填 `spotifyRefreshToken`）讀取網頁 API 中繼資料，不擷取或解密 Spotify 音訊，實際音訊由比對後的音源提供 |
| **Deezer** | 透過公開 API 讀取中繼資料與 ISRC，播放同樣經由 YouTube 比對路徑 |
| **LRCLIB** | 向公開資料庫 `lrclib.net` 傳送曲名、歌手、專輯與時長以查詢同步歌詞 |
| **FFmpeg** | 選配的獨立 GPL 外部編解碼工具（僅用於內建管線未涵蓋的罕見容器），授權依上游發佈者為準 |

## 隱私保證

除上述音源／歌詞查詢與營運者主動啟用的 `webhook.url` 外，節點**不包含任何遙測、自動檢查更新或外部回報連線**。
