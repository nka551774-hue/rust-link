# 音源系統

`POST /api/v1/tracks/resolve` 的 `query` 會在發出任何網路請求前依前綴與網域完成靜態路由。

## 查詢路由規則

| 查詢格式 | 目標音源 |
| --- | --- |
| `ytsearch:<關鍵字>`、`ytmsearch:<關鍵字>` | YouTube / YouTube Music 搜尋 |
| `youtube:playlist:<ID>` | YouTube 播放清單（含 `RD...` 合輯） |
| 11 字元 `[A-Za-z0-9_-]` | YouTube 單一影片 ID |
| `scsearch:<關鍵字>`、`soundcloud:<路徑>` | SoundCloud 搜尋或曲目／清單 |
| `spsearch:<關鍵字>`、`spotify:<...>` | Spotify 搜尋或 URI |
| `dzsearch:<關鍵字>`、`dzisrc:<ISRC>`、`deezer:<...>` | Deezer 搜尋、ISRC 或資源 |
| `youtube.com` / `youtu.be` / `soundcloud.com` / `open.spotify.com` / `deezer.com` HTTP(S) 網址 | 對應平台原生解析（支援子網域） |
| 其他 `http://` / `https://` 網址 | 直接媒體串流探測 |
| 其餘文字 | 回傳 `empty`（機器人層可用 `normalize_query` 自動轉為 `ytsearch:`） |

帶有效 `v=` 的 YouTube `watch` 連結優先載入該單曲；欲載入整份播放清單請使用 `/playlist?list=...` 或 `youtube:playlist:<ID>`。

## 各平台實作細節

- **YouTube**：純 Rust InnerTube 依序嘗試 `VISIONOS`（自動引導快取匿名 `visitorData`，優先取得 48 kHz WebM Opus `itag 251` 直通）、`IOS_PRODUCER`、`ANDROID_VR`、`ANDROID`（未設 `youtubePoToken` 且大於 1 MiB 時先探測尾端 1 byte 防止 ~900 KiB 403 截斷）；搜尋與清單優先用 `ANDROID` 並以 `VISIONOS` 備援。googlevideo 以 **512 KiB** 驗證區間分段讀取搭配 **2 MiB** 背景預讀緩衝與強 `ETag` `If-Range` 校驗。詳見 [YouTube 專章](youtube.md)。
- **SoundCloud**：公開 API v2 搭配自動抓取快取的 `client_id`，優先使用漸進式串流；沒有時改用 HLS（Opus > MP3 > AAC，分段串成單一串流在程序內解碼）。搜尋結果會略過只能播 30 秒試聽的 Go+ 曲目，加密（DRM）HLS 不支援。
- **Spotify / Deezer**：透過官方 API 擷取曲目、專輯與歌單中繼資料（Spotify 支援 `/intl-*/`、`/embed/` 與 `spotifyRefreshToken`；分頁嚴格鎖定同一 API origin），播放時以 ISRC 優先與標題／歌手／時長雙向比對至 YouTube 完整音源，排除未要求的翻唱、現場、混音或預覽版本。
- **Direct HTTP(S)**：直接探測 MP3、FLAC、Ogg Opus/Vorbis、MP4/M4A、WebM、WAV 標頭；最多跟隨 5 次安全轉址（禁止 HTTPS 降級 HTTP，跨主機自動剝除專屬標頭）。

## 快取、併發與 SSRF 防護

- **共用快取与合併查詢**：LRU 快取受 `cacheEntries`、`cacheTtlSeconds` 與 32 MiB 記憶體預算約束，命中直接共享 `Arc<LoadResult>`（錯誤不快取）；同時查詢同一字串由 `Singleflight` 共用單次請求，總併發受 `maxConcurrentLoads` 與 `loadTimeoutSeconds` 限制。
- **私有網路攔截**：`allowPrivateNetworks` 預設為 `false`，於連線建立前攔截解析至私有或特殊用途 IP 的網域（`youtubeProxy` 本機代理位址除外；若想保留節點端目標 IP 檢查，代理建議選用 `socks5://` 而非遠端解析的 `socks5h://` / `http://`）。
- **選配 FFmpeg 自動下載**：`autoDownload: true` 時，Windows 取自 BtbN GPL 發行檔、Linux/macOS 取自 martin-riedl.de，皆核對對應 `.sha256` 並拒絕小於 1 MiB 的異常回應。
