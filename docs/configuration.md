# 設定指南

節點依序讀取工作目錄下的 `config.yaml`、`config.yml`、`config.json`，或 `--config`／`-c` 指定路徑。設定採 `deny_unknown_fields` 嚴格校驗，拼字錯誤會在啟動時明確報錯。

若無設定檔且環境變數未提供密碼，節點會自動寫出含隨機密碼的 `config.yaml` 並啟動。**環境變數優先於設定檔**，容器可直接透過環境變數配置。完整範本見 [`config.example.yaml`](../config.example.yaml)。

## 節點與媒體工具

| 選項 | 預設 | 說明 |
| --- | --- | --- |
| `address` | `0.0.0.0:2333` | 監聽位址；置於 TLS 反向代理後請改用 `127.0.0.1:2333` |
| `password` | 空 | 必填 Bearer 驗證密碼 |
| `ffmpeg` | `ffmpeg` | 裸名稱（僅搜尋 `toolsDirectory`，不查 `PATH`）、絕對路徑或 `none` |
| `toolsDirectory` | `.tools` | 自動下載輔助工具存放目錄 |
| `autoDownload` | `false` | 缺少 FFmpeg 時自動下載並驗證 SHA-256 |
| `inProcessAudio` | `true` | 程序內原生解碼、DSP 與編碼；設為 `false` 則全數交由 FFmpeg |
| `lyricsSource` | `lrclib` | `lrclib`（含 YouTube Music 備援）或 `none` |

## 日誌

| 選項 | 預設 | 說明 |
| --- | --- | --- |
| `logLevel` | `info` | `debug`、`info`、`warn`、`error` |
| `logFormat` | `text` | `text` 或結構化 `json` |
| `logColor` | `auto` | `auto`、`always`、`never`（遵守 `NO_COLOR`） |
| `logDirectory` | `logs` | 每日輪替檔 `rustlink-YYYY-MM-DD.log` 目錄；空字串僅輸出至主控台 |
| `logRetentionDays` | `14` | 每日日誌保留天數；`0` 永久保留 |

## 音源與網路防護

| 選項 | 預設 | 說明 |
| --- | --- | --- |
| `youtubeNative` | `true` | 啟用純 Rust InnerTube 解析與播放 |
| `youtubeEndpoint` | `https://youtubei.googleapis.com` | InnerTube API 端點 |
| `youtubeMusicEndpoint` | `https://music.youtube.com` | YouTube Music 搜尋與歌詞端點 |
| `youtubeClientVersion` | 空 | 覆寫 `ANDROID` 用戶端版本（預設內建 `20.10.38`） |
| `youtubePoTokenFile` | 空（初始範本為 `potoken.yaml`） | 獨立 PO Token 設定檔路徑（支援 YTDLnis `GVS`/`Player`/`Subs` YAML 或原文直貼、`mweb.gvs+...` 及瀏覽器 `v1/player` Payload；存檔即自動免重啟熱載入） |
| `youtubeRequirePoToken` | `false` | 設為 `true` 時，`poToken` 未設定就拒絕所有 YouTube / YouTube Music 請求；一鍵腳本只在你輸入 PO token 時開啟 |
| `youtubePoToken` / `youtubeGvsPoToken` / `youtubePlayerPoToken` / `youtubeVisitorData` | 空 | 內嵌或環境變數覆寫之 Proof-of-Origin Token（通用 / GVS / Player）與 Visitor Data |
| `youtubeProxy` | 空 | 將所有 YouTube API 與音訊流量導向 `http(s)://`、`socks4(a)://` 或 `socks5(h)://` 代理；代理失敗時直接報錯不退回直連 |
| `soundcloudNative` | `true` | 啟用 SoundCloud 公開 API 解析 |
| `soundcloudApiEndpoint` / `soundcloudWebEndpoint` | 官方端點 | API 與抓取 `client_id` 的網頁端點 |
| `soundcloudClientId` / `soundcloudClientIdTtlSeconds` | 空 / `21600` | 空字串時自動抓取並快取 `client_id` |
| `spotifyClientId` / `spotifyClientSecret` | 空 | Spotify 應用程式憑證（未設定時拒絕 `spsearch:` 與 Spotify 連結） |
| `spotifyRefreshToken` / `spotifyMarket` | 空 / `US` | 私人／官方精選歌單所需之 Refresh Token 與市場國碼 |
| `deezerApiEndpoint` | `https://api.deezer.com` | Deezer 公開 API 端點 |
| `allowPrivateNetworks` | `false` | 預設攔截指向內網或特殊用途位址的音源請求（SSRF 防護） |

## 濾鏡、容量、限流與 Webhook

- **`filters`**：控制是否允許客戶端啟用個別濾鏡（`volume`、`equalizer`、`karaoke`、`timescale`、`tremolo`、`vibrato`、`rotation`、`distortion`、`channelMix`、`lowPass`），未列出者預設啟用。
- **容量與音質**：

| 選項 | 預設 | 說明 |
| --- | --- | --- |
| `maxPlayers` / `maxSessions` | `100` / `100` | 全節點播放器與工作階段上限 |
| `maxConcurrentLoads` / `loadTimeoutSeconds` | `8` / `30` | 同時解析數與單次解析時限（秒） |
| `maxPlaylistTracks` / `searchResults` | `100` / `5` | 播放清單曲數上限與單次搜尋筆數 |
| `cacheEntries` / `cacheTtlSeconds` | `1024` / `300` | 解析快取筆數與存活秒數 |
| `playerUpdateSeconds` / `statsSeconds` | `5` / `60` | 播放進度與 `node.stats` 推送秒數 |
| `eventBuffer` | `256` | 每工作階段 WebSocket 待送事件上限 |
| `opusBitrate` / `opusComplexity` | `192000` / `10` | Opus 目標位元率（`16000..=256000` bit/s）與複雜度（`0..=10`） |

- **`ratelimits.enable`**（預設 `true`）：驗證密碼後依「路由＋工作階段」獨立計算權杖桶，驗證失敗則依來源 IP（IPv6 `/64`）限流並回 `429`。
- **`webhook`**（`enable`、`url`、`stateLatencySeconds`）：背景執行緒非阻塞投遞（Discord 網址送 Embed，其餘送原始事件 JSON），具 4 次退避重試。

## 環境變數對照

- **節點與工具**：`RUSTLINK_ADDRESS`、`RUSTLINK_PASSWORD`、`RUSTLINK_FFMPEG`、`RUSTLINK_TOOLS_DIRECTORY`、`RUSTLINK_AUTO_DOWNLOAD`、`RUSTLINK_IN_PROCESS_AUDIO`、`RUSTLINK_LYRICS_SOURCE`
- **日誌**：`RUSTLINK_LOG_LEVEL`、`RUSTLINK_LOG_FORMAT`、`RUSTLINK_LOG_COLOR`、`RUSTLINK_LOG_DIRECTORY`、`RUSTLINK_LOG_RETENTION_DAYS`
- **YouTube**：`RUSTLINK_YOUTUBE_NATIVE`、`RUSTLINK_YOUTUBE_ENDPOINT`、`RUSTLINK_YOUTUBE_MUSIC_ENDPOINT`、`RUSTLINK_YOUTUBE_CLIENT_VERSION`、`RUSTLINK_YOUTUBE_PO_TOKEN_FILE`、`RUSTLINK_YOUTUBE_REQUIRE_PO_TOKEN`、`RUSTLINK_YOUTUBE_PO_TOKEN`、`RUSTLINK_YOUTUBE_GVS_PO_TOKEN`、`RUSTLINK_YOUTUBE_PLAYER_PO_TOKEN`、`RUSTLINK_YOUTUBE_VISITOR_DATA`、`RUSTLINK_YOUTUBE_PROXY`
- **其他音源**：`RUSTLINK_SOUNDCLOUD_NATIVE`、`RUSTLINK_SOUNDCLOUD_API_ENDPOINT`、`RUSTLINK_SOUNDCLOUD_WEB_ENDPOINT`、`RUSTLINK_SOUNDCLOUD_CLIENT_ID`、`RUSTLINK_SOUNDCLOUD_CLIENT_ID_TTL_SECONDS`、`SPOTIFY_CLIENT_ID`、`SPOTIFY_CLIENT_SECRET`、`SPOTIFY_REFRESH_TOKEN`、`SPOTIFY_MARKET`、`RUSTLINK_DEEZER_API_ENDPOINT`、`RUSTLINK_ALLOW_PRIVATE_NETWORKS`
- **容量、限流與 Webhook**：`RUSTLINK_MAX_PLAYERS`、`RUSTLINK_MAX_SESSIONS`、`RUSTLINK_MAX_CONCURRENT_LOADS`、`RUSTLINK_MAX_PLAYLIST_TRACKS`、`RUSTLINK_SEARCH_RESULTS`、`RUSTLINK_CACHE_ENTRIES`、`RUSTLINK_CACHE_TTL_SECONDS`、`RUSTLINK_LOAD_TIMEOUT_SECONDS`、`RUSTLINK_PLAYER_UPDATE_SECONDS`、`RUSTLINK_STATS_SECONDS`、`RUSTLINK_EVENT_BUFFER`、`RUSTLINK_OPUS_BITRATE`、`RUSTLINK_OPUS_COMPLEXITY`、`RUSTLINK_RATELIMITS_ENABLE`、`RUSTLINK_WEBHOOK_ENABLE`、`RUSTLINK_WEBHOOK_URL`、`RUSTLINK_WEBHOOK_STATE_LATENCY_SECONDS`

布林變數接受 `1/true/yes/on` 與 `0/false/no/off`（空字串忽略）。所有密碼、Token、代理與 Webhook 網址於 `Debug` 輸出中皆自動遮蔽。
