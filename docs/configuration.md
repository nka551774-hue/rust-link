# 設定

節點依序讀取工作目錄下的 `config.yaml`、`config.yml`、`config.json`，或 `--config`／`-c` 指定的檔案。欄位名稱嚴格比對：拼錯的選項會讓啟動失敗並指出欄位名，比靜默忽略更容易發現問題。

找不到設定檔、環境變數也沒有密碼時，節點會寫出含隨機密碼的 `config.yaml` 再啟動，所以執行檔可以直接雙擊。環境變數優先於設定檔，容器因此不必掛載設定檔。`config.example.yaml` 是含註解的完整範本；`config.yaml` 已被 `.gitignore` 排除，不會進入版本庫。

## 節點

| 選項 | 預設 | 說明 |
| --- | --- | --- |
| `address` | `0.0.0.0:2333` | 監聽位址。預設開放所有介面，讓其他機器上的機器人可以連入；放在會終止 TLS 的反向代理後面時用 `127.0.0.1:2333`。 |
| `password` | 空 | 客戶端以 `Authorization: Bearer <password>` 傳送的密碼；沒有密碼無法啟動。 |

## 媒體工具

| 選項 | 預設 | 說明 |
| --- | --- | --- |
| `ffmpeg` | `ffmpeg` | FFmpeg 執行檔：裸名稱、路徑或 `none`。裸名稱代表 `toolsDirectory` 內的檔案，**不會**到 `PATH` 找，確保每個節點用的是給它的那一版。`none` 完全關閉 FFmpeg。 |
| `toolsDirectory` | `.tools` | 下載的輔助工具存放處。 |
| `autoDownload` | `false` | 缺少輔助工具（既不在 `toolsDirectory`，也沒有明確路徑）時自動安裝。 |
| `inProcessAudio` | `true` | 在行程內解碼、濾鏡與編碼。關閉後全部交給 FFmpeg；兩種引擎的解碼器與 DSP 實作不同，CPU 成本與濾鏡輸出可能有差異。 |
| `lyricsSource` | `lrclib` | `lrclib` 先查資料庫、再查 YouTube Music；`none` 關閉歌詞，節點不會對外連線。 |

FFmpeg 只在內建管線不處理的容器才需要；沒有 FFmpeg 的節點仍能播放支援的格式。啟動摘要會回報 FFmpeg 是否可用。

## 日誌

| 選項 | 預設 | 說明 |
| --- | --- | --- |
| `logLevel` | `info` | `debug`、`info`、`warn`、`error`。 |
| `logFormat` | `text` | `text` 給人看，`json` 給收集器；JSON 不輸出字樣 logo。 |
| `logColor` | `auto` | `auto` 在終端機上著色、重導向時不著色；`always`、`never` 強制指定。遵守環境變數 `NO_COLOR`。 |
| `logDirectory` | `logs` | 每日檔案 `rustlink-YYYY-MM-DD.log`，日期變動時換檔；空字串表示只輸出到主控台。檔案內容不著色。 |
| `logRetentionDays` | `14` | 保留幾天的檔案，啟動與換日時刪除更舊的；`0` 全部保留（日誌由外部輪替時使用）。只會動這個節點寫的檔案。 |

啟動摘要以 `Rustlink starting` 開頭，每項設定一筆 `INFO`，不產生多行單筆記錄，方便主控台、文字檔與 JSON 都能逐行檢視。

## 音源

| 選項 | 預設 | 說明 |
| --- | --- | --- |
| `youtubeNative` | `true` | 用原生 InnerTube 用戶端解析 YouTube；關閉會停用 YouTube 解析與播放。 |
| `youtubeEndpoint` | `https://youtubei.googleapis.com` | InnerTube 端點。 |
| `youtubeMusicEndpoint` | `https://music.youtube.com` | YouTube Music 用戶端與歌詞資料庫使用的端點。 |
| `youtubeClientVersion` | 空 | 覆寫 `ANDROID` 用戶端的版本與對應的 User-Agent；空字串使用內建的 `20.10.38`。其他用戶端保留各自的版本。 |
| `youtubePoToken` | 空 | 你提供的 proof-of-origin token：放進 player 請求，並以 `pot` 參數附在 googlevideo 音訊網址上。 |
| `youtubeVisitorData` | 空 | 搭配 token 的 visitor data。 |
| `youtubeProxy` | 空 | 讓所有 YouTube 請求（API 與音訊）改走代理：`http://`、`socks4://`、`socks5://`、`socks5h://`，可含 `使用者:密碼@`。YouTube 對節點的位址回應「Sign in to confirm you're not a bot」時使用。代理無法使用時，YouTube 載入直接失敗，不會改為直連；其他音源不經代理。 |
| `soundcloudNative` | `true` | 用公開 API 解析 SoundCloud。 |
| `soundcloudApiEndpoint` | `https://api-v2.soundcloud.com` | API 端點。 |
| `soundcloudWebEndpoint` | `https://soundcloud.com` | 抓取 client id 的網站。 |
| `soundcloudClientId` | 空 | client id；空字串表示抓取並快取。 |
| `soundcloudClientIdTtlSeconds` | `21600` | 抓到的 client id 保留多久。 |
| `spotifyClientId`、`spotifyClientSecret` | 空 | Spotify 應用程式憑證。沒有 client id 時，`spsearch:` 會被明確拒絕，不會偷偷換成別的結果。 |
| `spotifyRefreshToken` | 空 | 使用者 refresh token，讀取私人或編輯精選播放清單時需要。 |
| `spotifyMarket` | `US` | 目錄請求的市場國碼。 |
| `deezerApiEndpoint` | `https://api.deezer.com` | Deezer API 端點。 |
| `allowPrivateNetworks` | `false` | 允許來源連到私有或特殊用途位址。關閉可避免客戶端讓節點去連它所在的內網。`youtubeProxy` 本身不受此限（通常在本機）；但由代理自己解析網域的 `http://`、`socks5h://`，目標位址是代理解析的，節點無法檢查。 |

## 濾鏡

`filters` 把濾鏡名稱對應到「客戶端是否可使用」。沒列出的濾鏡預設啟用；設為 `false` 後，要求該濾鏡的播放器更新會以 `Following filters are disabled in the config: ...` 失敗。名稱：`volume`、`equalizer`、`karaoke`、`timescale`、`tremolo`、`vibrato`、`rotation`、`distortion`、`channelMix`、`lowPass`。

## 容量

| 選項 | 預設 | 說明 |
| --- | --- | --- |
| `maxPlayers` | `100` | 全節點的播放器上限。 |
| `maxSessions` | `100` | 全節點的工作階段上限。 |
| `maxConcurrentLoads` | `8` | 同時進行的解析數；等待名額也計入 `loadTimeoutSeconds`。 |
| `maxPlaylistTracks` | `100` | 一份播放清單或專輯最多取幾首。 |
| `searchResults` | `5` | 一次搜尋回傳幾筆。 |
| `cacheEntries` | `1024` | 解析結果快取筆數。 |
| `cacheTtlSeconds` | `300` | 快取保留秒數。 |
| `loadTimeoutSeconds` | `30` | 一次解析的時限。 |
| `playerUpdateSeconds` | `5` | 工作階段回報播放位置的間隔。 |
| `statsSeconds` | `60` | WebSocket 推送 `node.stats` 事件的間隔。 |
| `eventBuffer` | `256` | 等待 WebSocket 的事件數。客戶端停止讀取時，佇列滿了就會被關閉；節點不會靜默丟事件，以免客戶端顯示的狀態和實際不符。 |
| `opusBitrate` | `192000` | Opus 目標位元率（bit/s），範圍 `16000`–`256000`。越高占用的網路頻寬越多。 |
| `opusComplexity` | `10` | 編碼器複雜度 0–10。降低可減少每個播放器每個 frame 的 CPU 成本，適合播放器數多於核心數的節點；內建管線與 FFmpeg 後備都遵循。 |

預設輸出是 48 kHz 雙聲道 Opus、20 ms 封包、192 kbit/s 可變位元率、複雜度 10。兩種引擎都會明確設定位元率、複雜度、VBR、應用類型與封包遺失／FEC；FEC 能否生效取決於 Opus 編碼模式與接收端，不保證補回每一個遺失的封包。

只有**省略**的設定會用新預設值；既有的 `opusBitrate: 128000` 或 `RUSTLINK_OPUS_BITRATE` 會保留。提高位元率只減少額外的壓縮損失，轉碼無法補回原始音源已缺少的細節；最終聽感仍受來源、濾鏡、Discord 語音頻道與聽者網路影響。這是有損編碼，不是無損或品質保證。

原生管線在套用頻率相關濾鏡前先轉成 48 kHz。重採樣會保留開頭並沖出結尾；Ogg Opus 的 pre-skip、輸出增益與結尾 granule 在解碼時處理，編碼器延遲的樣本也會沖出。RTP 一律使用完整 20 ms 封包，不帶 Ogg 的裁切資訊，所以最後可能多一個含編碼延遲與靜音填充的封包。兩種引擎不保證產生相同的編碼位元組或相同的 DSP 樣本。

## 限流

`ratelimits.enable` 開關各路由的 token bucket。限流器在驗證**之後**執行：先檢查密碼，通過的請求再依「路由＋工作階段」各自計算額度，所以一個客戶端狂打不會用掉別的客戶端或別的路由的額度。密碼錯誤的請求不會消耗任何合法客戶端的額度，只會消耗該來源 IP 自己的失敗登入額度（IPv6 以 /64 計）；公開路由（如 `/version`）依來源 IP 計算。收到限流的請求回 `429`。

## Webhook

| 選項 | 預設 | 說明 |
| --- | --- | --- |
| `webhook.enable` | `false` | 把節點事件送到外部端點。 |
| `webhook.url` | 空 | Discord webhook 網址收到 embed；其他端點收到事件本身的 JSON。 |
| `webhook.stateLatencySeconds` | `5` | 整個節點播放器狀態報告的間隔；`0` 表示端點只收事件。 |

投遞在單一背景執行緒進行，慢的端點不會卡住產生事件的播放器。投遞失敗不會讓節點停止：暫時性的錯誤（連線失敗、`429`、5xx）最多重試 4 次，退避並遵守 `Retry-After`（上限 60 秒）；仍失敗、被端點拒絕，或佇列已滿時，該事件會被丟棄並記錄警告（第 1 次與之後每 100 次）。節點不會因 webhook 掛掉而離線。

## 環境變數

環境變數覆寫設定檔，名稱如下：

- 節點與工具：`RUSTLINK_ADDRESS`、`RUSTLINK_PASSWORD`、`RUSTLINK_FFMPEG`、`RUSTLINK_TOOLS_DIRECTORY`、`RUSTLINK_AUTO_DOWNLOAD`、`RUSTLINK_IN_PROCESS_AUDIO`、`RUSTLINK_LYRICS_SOURCE`
- 日誌：`RUSTLINK_LOG_LEVEL`、`RUSTLINK_LOG_FORMAT`、`RUSTLINK_LOG_COLOR`、`RUSTLINK_LOG_DIRECTORY`、`RUSTLINK_LOG_RETENTION_DAYS`
- YouTube：`RUSTLINK_YOUTUBE_NATIVE`、`RUSTLINK_YOUTUBE_ENDPOINT`、`RUSTLINK_YOUTUBE_MUSIC_ENDPOINT`、`RUSTLINK_YOUTUBE_CLIENT_VERSION`、`RUSTLINK_YOUTUBE_PO_TOKEN`、`RUSTLINK_YOUTUBE_VISITOR_DATA`、`RUSTLINK_YOUTUBE_PROXY`
- 其他音源：`RUSTLINK_SOUNDCLOUD_NATIVE`（或 `RUSTLINK_SOUNDCLOUD`）、`RUSTLINK_SOUNDCLOUD_API_ENDPOINT`、`RUSTLINK_SOUNDCLOUD_WEB_ENDPOINT`、`RUSTLINK_SOUNDCLOUD_CLIENT_ID`、`RUSTLINK_SOUNDCLOUD_CLIENT_ID_TTL_SECONDS`、`RUSTLINK_DEEZER_API_ENDPOINT`、`RUSTLINK_ALLOW_PRIVATE_NETWORKS`（或 `RUSTLINK_PRIVATE_NETWORKS`）
- 容量：`RUSTLINK_MAX_PLAYERS`、`RUSTLINK_MAX_SESSIONS`、`RUSTLINK_MAX_CONCURRENT_LOADS`、`RUSTLINK_MAX_PLAYLIST_TRACKS`、`RUSTLINK_SEARCH_RESULTS`、`RUSTLINK_CACHE_ENTRIES`、`RUSTLINK_CACHE_TTL_SECONDS`、`RUSTLINK_LOAD_TIMEOUT_SECONDS`、`RUSTLINK_PLAYER_UPDATE_SECONDS`、`RUSTLINK_STATS_SECONDS`、`RUSTLINK_EVENT_BUFFER`、`RUSTLINK_OPUS_BITRATE`、`RUSTLINK_OPUS_COMPLEXITY`
- 限流與 webhook：`RUSTLINK_RATELIMITS_ENABLE`、`RUSTLINK_WEBHOOK_ENABLE`（或 `RUSTLINK_WEBHOOK_ENABLED`）、`RUSTLINK_WEBHOOK_URL`、`RUSTLINK_WEBHOOK_STATE_LATENCY_SECONDS`

開關型變數接受 `1/true/yes/on` 與 `0/false/no/off`；無法辨識的值（包含空字串）會被忽略，因此不會意外打開 `RUSTLINK_ALLOW_PRIVATE_NETWORKS` 這類防護。無法解析的數字同樣被忽略，但範圍檢查仍會擋下超出範圍的值。

Spotify 設定沿用 Spotify 儀表板發出的名稱（亦支援 `RUSTLINK_` 前綴；空字串不會洗掉設定檔中的值）：`SPOTIFY_CLIENT_ID`、`SPOTIFY_CLIENT_SECRET`、`SPOTIFY_REFRESH_TOKEN`、`SPOTIFY_MARKET`。

密碼、`youtubePoToken`、`youtubeVisitorData`、`youtubeProxy`（可能含帳密）、SoundCloud client id、Spotify 憑證與 webhook 網址，在 `Debug` 輸出中都會被遮蔽。
