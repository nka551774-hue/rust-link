# YouTube 專章

Rustlink 以 100% 純 Rust 原生 InnerTube 協定解析與串流 YouTube / YouTube Music，不執行播放器 JavaScript、不呼叫 `yt-dlp`，亦不將暫存檔寫入硬碟。搭配內建的 [`music`](../crates/discord-bot/examples/music/main.rs) 或 [`slash.rs`](../crates/discord-bot/examples/slash.rs) 即可在 Discord 透過 `/play`、`@Bot` 或頻道直接貼連結播放。

## 支援的輸入格式

| 輸入形式 | 解析行為 |
| --- | --- |
| `https://www.youtube.com/watch?v=dQw4w9WgXcQ`（含 `m.`、`music.`、`youtube-nocookie.com`） | 單一影片（即使同時附帶 `&list=` 亦優先載入該單曲） |
| `https://youtu.be/dQw4w9WgXcQ?si=...`、`/shorts/...`、`/live/...`、`/embed/...`、`/v/...`、`/attribution_link?u=...` | 單一影片或直播 |
| `https://www.youtube.com/playlist?list=PL...`、`watch?list=...`、`youtube:playlist:<ID>`（含 `RD...` 合輯） | 播放清單 |
| `dQw4w9WgXcQ`（11 字元 ID） | 單一影片 |
| `ytsearch:<關鍵字>`、`ytmsearch:<關鍵字>` | YouTube / YouTube Music 搜尋結果 |

- **分享文字與時間戳擷取**：`rustlink-discord-bot` 的 `normalize_query` 與 `extract_message_query` 會自動從手機分享文字、`||...||`、`<...>` 或省略 `https://` 的 `youtu.be/...` 抽出純淨網址；`extract_url_timestamp_ms` 則解析 `?t=90`、`?t=1m30s`、`&start=43`、`#t=2m` 並帶入 `PlayerUpdate.position` 精準起播（不污染共用快取中的 `rlt1.` 權杖）。

## 多用戶端自適應備援與零轉碼加速

播放單曲時依健康度與音質優先序嘗試下列原生 InnerTube 用戶端（遭驗證或 403 的用戶端會自動進入短期指數退避冷卻）：

| 順序 | 用戶端 | 特性與防護 |
| --- | --- | --- |
| 1 | `VISIONOS 0.1` | 自動透過 `/youtubei/v1/visitor_id`（與 `/sw.js_data` 後備）引導並快取匿名 `visitorData`（與歌詞模組共用），優先提供原生 **48 kHz WebM Opus（`itag 251`）**；若快取憑證失效會自動重取一次 |
| 2 | `IOS_PRODUCER 0.147.945` | 免登入、免 PO Token 穩定取得完整長度 AAC-LC（`itag 140`）高速備援 |
| 3 | `ANDROID_VR 1.60.19` | 精簡欄位備援用戶端 |
| 4 | `ANDROID` | 可由 `youtubeClientVersion` 覆寫；未設 `youtubePoToken` 且大於 1 MiB 時先做尾端 1-byte 預檢，攔截 ~900 KiB（約一分鐘）處 403 截斷 |

搜尋與播放清單優先走 `ANDROID` 並以 `VISIONOS` 備援；`RD...` 合輯若 `/browse` 為空自動切換 `/next` 解析。選軌優先非自動配音原音與未動態壓縮音軌，並依無損 FLAC → 48 kHz Opus（`itag 251`）→ AAC-LC 排序。

### 低延遲串流關鍵機制

1. **預解析串流快取**：`resolve` 取得的 `streamingData` 依影片 ID 與 `?expire=`（預留 120 秒安全邊界，淘汰時自動清零記憶體簽章網址）快取，緊接著 `play` 或 `seek` 直接重用 CDN 串流，省去重複 `/player` 往返。
2. **512 KiB 驗證分段 + 2 MiB 背景預讀（`PrefetchReader`）**：背景執行緒預先將 googlevideo 512 KiB 區間讀入 64 KiB 循環緩衝池（上限 2 MiB，穩態零配置），徹底隔離網路抖動。
3. **48 kHz Opus 零轉碼直通**：音量 100% 且無濾鏡時，`itag 251` 經 `opus_packet_parse` 校驗後直接送往 Discord RTP（每音框僅 **445.2 ns**）；中途啟用音量或濾鏡時，透過最近兩包完成零相位預熱並無縫切入 32-tap Sinc 重取樣與 24-bit `opus_encode_float` 管線。
4. **壓縮封包快轉（Fast Compressed Seek）**：`seek` 直接跳過目標前的壓縮封包，僅解碼收斂預熱視窗（Opus 80 ms `SeekPreRoll`、AAC-LC 8 個音框）。

## PO Token 專屬設定檔模式（`potoken.yaml`，預設強制啟用）

> [!NOTE]
> **全鏈路強制 PO Token 與免重啟自動熱載入**：啟動腳本、預設 `config.yaml` 與 `install.sh` 皆預設啟用 `youtubePoTokenFile: potoken.yaml` 與 `youtubeRequirePoToken: true`。所有 YouTube / YouTube Music 請求（`/player`、`/search`、`/browse`、`/next`、歌詞與 `googlevideo.com` 音訊網址 `&pot=`）一律強制走 `poToken` 驗證（優先採用原生支援 `poToken` 與 **48 kHz WebM Opus `itag 251`** 的 `VISIONOS` 與 `ANDROID`），未設定時拒絕匿名直連以避免觸發風控。檔案存檔後節點會**自動偵測變更並即時熱更新、清空舊快取**，全程無須重啟節點或中斷語音通話。

### 1. 從瀏覽器取得 `poToken` 與 `visitorData`（30 秒步驟）
1. 在自己的瀏覽器開啟無痕視窗前往 `https://music.youtube.com` 或 `https://www.youtube.com` 隨意播放一首歌。
2. 按 `F12` 開啟開發者工具 -> **Network（網路）** -> 篩選 `v1/player` -> 點開任一筆請求的 **Payload（載荷）**。
3. 複製以下兩個欄位填入專案根目錄（或 `/opt/rustlink/`）的 **`potoken.yaml`**（範本見 [`potoken.example.yaml`](../potoken.example.yaml)）：
   ```yaml
   poToken: "<serviceIntegrityDimensions.poToken>"
   visitorData: "<context.client.visitorData>"
   ```
   - 亦支援將整個 `v1/player` Request Payload JSON 直接貼入 `potoken.json`、使用 `{"poToken":"...","visitorData":"..."}` JSON，或 `<visitorData>:<poToken>` 合併字串。

### 2. 部署與免重啟更新方式
- **直接編輯 `potoken.yaml`（推薦）**：
  - 本機 `start.bat` / `start.sh` 與 `quickstart/start.bat` / `quickstart/start.sh` 皆自動建立並監聽專案根目錄下的 `potoken.yaml`。
  - Linux VPS `sudo ./install.sh --with-bot` 自動建立 `/opt/rustlink/potoken.yaml`（權限 `0640`；若原始碼目錄已有 `potoken.yaml` 會自動帶入）。
  - 權杖數小時後過期時，直接修改 `potoken.yaml` 存檔，下一次解析或切歌即自動套用新權杖。
- **REST API 線上熱更新（`PATCH /api/v1/youtube`）**：適合唯讀容器或遠端自動化腳本推送：
  ```sh
  curl -X PATCH http://127.0.0.1:2333/api/v1/youtube \
    -H "Authorization: Bearer <RUSTLINK_PASSWORD>" \
    -H "Content-Type: application/json" \
    -d '{"po_token":"<PO_TOKEN>","visitor_data":"<VISITOR_DATA>"}'
  ```
- **搭配 `youtubeProxy`**：支援 `http(s)://`、`socks4(a)://`、`socks5(h)://`（如本機 Cloudflare WARP `socks5://127.0.0.1:40000`），僅路由 YouTube API 與音訊流量，不影響其他音源與 Discord UDP。
- **驗證指令**：`cargo test -p rustlink-source --test live_youtube -- --ignored --nocapture`（自動讀取根目錄 `potoken.yaml` 或 `RUSTLINK_YOUTUBE_*` 環境變數實測）。
