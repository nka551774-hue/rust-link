# 審查紀錄：接手審查與音質驗證（歷史）

這是 **2026-10-25** 兩輪審查的紀錄，基準提交 `54a7094e`（第一輪）與 `cc996267`（第二輪）。內容描述的是當時的狀態；之後的改動（例如 YouTube 用戶端順序、`youtubeProxy`、單人頻道的 DAVE）見 [YouTube](youtube.md) 與 [DAVE 與語音](voice.md)。

## 範圍

| 範圍 | 審查與驗證 |
| --- | --- |
| `protocol`、`client` | 曲目 token 與 API 型別、REST／WebSocket 客戶端：大小限制、網址與 ID、驗證標頭、PATCH 的 null 語意 |
| `source` | 路由、Spotify／YouTube catalogue、HTTP／DNS 防護、有界 LRU、並行解析；用 loopback API 驗證連結、分頁、媒體選取與讀取 |
| `audio` | 原生解碼、重採樣、DSP、Opus、Ogg、FFmpeg：非 48 kHz 來源、首尾音訊、編碼延遲、左右聲道、長串流的記憶體界限 |
| `config` | 新設定的高音質預設，以及既有明確設定與環境變數的保留 |
| `server` | 播放器狀態、工作階段、停止、容量與限流、webhook；用真實 HTTP 客戶端與假的音訊／語音依賴驗證生命週期 |
| `voice`、`dave` | gateway／UDP／RTP、DAVE 轉換、金鑰與 nonce、佇列與關閉；用本機 Discord 模擬與獨立的 DAVE 接收端 |
| `lyrics` | LRC、LRCLIB、YouTube Music 的時間與文字解析 |
| `node`、啟動與安裝 | CLI、媒體工具、日誌、Linux 安裝器與 Windows 啟動 |

## 來源與播放行為

- Spotify 支援歌曲、專輯、播放清單、`intl-*` 地區前綴、embed、分享參數、fragment 與 Discord 的 `<…>` 包覆；拒絕錯誤 ID、額外路徑與偽造主機。分頁網址必須留在同一個 API origin 與版本路徑，避免送出 Bearer token。
- Spotify 只提供中繼資料，完整音源透過 YouTube 以歌名、歌手、長度比對取得。不使用預覽，也不解密 Spotify 音訊。單字母與單一中文字的名稱可以比對；含有未要求的 cover、live、remix、karaoke、變速或預覽標記的標題會被排除。比對無法保證錄音身分完全相同。
- YouTube 保留原始音軌與動態再比位元率。搜尋與播放清單跳過廣告 renderer 的子樹；播放器只開啟選定歌曲的音訊網址，不取得 player 廣告項目。不插入片頭或中途廣告，原影片已錄入的內容不剪。
- 網路防護補上 IPv4 this-network、多播、已退役的 6to4 與 IPv6 特殊用途位址；修正把整個 `2001::/16` 誤當成 Teredo，正常的全球 IPv6 位址現在可以連線。兩項回歸都先重現失敗再修復。

## 音訊與服務可靠性

- 新預設 48 kHz 雙聲道、192 kbit/s Opus、複雜度 10；既有明確指定的 128 kbit/s 或環境覆寫不被改寫。編碼選項明確設定，libopus 控制失敗會回報。
- 修復非 48 kHz 來源在套用固定 48 kHz 濾鏡前沒有轉換取樣率的問題；重採樣補齊首尾，Ogg EOS granule 裁掉尾端 padding，原生 Opus 保留編碼器延遲中的最後音訊。
- Ogg 裁尾涵蓋末頁多個封包與非零的起始 granule，拒絕倒退、非法或截斷的時間資料；每次只暫存一頁壓縮封包，PCM 單包最多 120 ms。
- `FilterSet::default()` 原本把 gain 設為零，直接走 FFmpeg 路徑會靜音；改為與空濾鏡一致的中性設定，並用真實 FFmpeg 檢查聲音能量與聲道分離，避免只數封包而漏掉靜音。
- 重採樣歷史緩衝的相對與累計位置混用，會隨播放時間增加記憶體；改以目前緩衝座標回收，並加入分塊輸出一致性與記憶體界限測試。
- 極慢的 timescale 優先排出已有的輸出，不持續預讀整首；400 倍延長播放的測試輸出 80 秒，來源讀取仍不超過 128 KiB。大的 seek 位置改用寬整數計算，避免溢位跳回開頭。
- 播放統計改讀即時語音連線，避免 `playing_players` 一直為零；相同音量或濾鏡與不適用的 seek 不重新開啟音源；`replace=false` 保留既有曲目時仍套用 pause。
- 背景工作先註冊通知再查持久的關閉旗標，修復先 close、後排程時遺失停止通知而持續持有 Server。
- 歌詞時間使用非負檢查與 checked arithmetic；YouTube Music 合併所有文字 run，不只顯示第一段；LRC 展開限制 10,000 行／8 MiB；webhook 錯誤不再記錄含 token 的完整網址。
- Linux 安裝器對 `sed` 的替換值做 literal escaping，密碼中的 `&`、`|` 與路徑中的 `&` 不會改壞設定。

## 第二輪：HTTP 串流完整性（2026-10-25）

| 問題 | 修復 |
| --- | --- |
| 206 宣告的區間、實際內容與續傳版本可能不一致 | 驗證 start／end／已知 total 與 Content-Length；chunked 內容同樣受區間限制，超量位元組不交給解碼器，短 body 可續傳。保留強 ETag，後續送 `If-Range`，拒絕明確不同的 ETag；總長未知的 206 拒絕，未知長度的完整 200 串流仍支援 |
| `ureq` 預設自動 gzip 解壓會先拿掉編碼與長度標頭，破壞位移驗證 | workspace 統一停用自動解壓，請求明確使用 identity |
| FLAC padding 後標籤遺失、MP4 meta 與未知時長處理錯誤、Ogg 前綴被當成完整時長 | 依 FLAC 24-bit block 長度遍歷；相容 MP4 FullBox 與 QuickTime `hdlr`；未知 duration 保留為未知；Ogg 需完整內容與 EOS 才計時，Opus 扣除 pre-skip |
| 舊播放 pump 的 pause 靜音可能晚於換曲，把新曲設成不發聲 | `speaking(false)` 與播放器狀態由同一把鎖排序，傳輸採非阻塞的 `try_send` |
| 無效的 voice endpoint 可能在既有連線被拆掉後才失敗 | 提前驗證 hostname label 字元、長度、連字號與 port 0；這是格式與狀態保護，不宣稱修復跨主機 SSRF |

複審另外指出：弱 ETag 與強度不明的 Last-Modified 不能當強 `If-Range` 身分證明；QuickTime meta 不一定帶 FullBox 前綴。兩者都已修正並加入回歸。

## 驗證（當時）

環境：Windows x64、Rust 1.99.0 GNU、MSYS2 UCRT64 libopus、本機真實 FFmpeg。

| 驗證 | 第一輪 | 第二輪 |
| --- | --- | --- |
| `cargo fmt --all --check` | 通過 | 通過 |
| `cargo test --workspace --release --locked --no-fail-fast` | 369 passed、3 ignored | 386 passed、3 ignored |
| `cargo clippy --workspace --all-targets --release --locked -- -D warnings` | 通過 | 通過 |
| 設定 `RUSTLINK_TEST_FFMPEG` 的 ignored 測試 | 2 passed | 2 passed |
| 正式執行檔的 HTTP smoke | 通過 | 通過（含 WAV／FLAC／M4A 解析） |
| 真實 YouTube 分享連結 | `dQw4w9WgXcQ` 附 `list` 與 fragment，HTTP 200、曲名與 213,000 ms 時長正確 | 同左 |
| 真實 YouTube 音訊 | 前 3 秒：150 個 Opus 封包、非靜音 | 前 30 秒：1500 包、每聲道 1,440,000 samples，單次讀取跨越至少 5 個 128 KiB 區段 |
| Spotify 未設憑證 | 明確回報需要 `spotifyClientId` | 同左 |

第二輪沒有新的 benchmark，也不宣稱所有格式、所有來源或真實 Discord 端到端已驗證。

## 當時的限制

- 沒有 Spotify API 憑證，也沒有 Discord 即時語音憑證：Spotify 比對與 Discord 語音用本機模擬服務驗證。真實 Spotify 帳號權限、YouTube 的地區與 PO token 限制，以及 Discord 聲道能力與網路品質仍會影響實際使用。
- 192 kbit/s 是有損編碼的取捨，不能補回原始音源缺少的細節。測試包含長度、能量、聲道隔離與濾鏡行為，沒有宣稱主觀聽感、無損輸出或真實 Discord 端到端品質。
- 沒有強 ETag 的來源只能核對位移與長度，無法偵測同大小的上游替換。
- 當時沒有在原生 Linux 或 Docker 執行部署。
