# 接手審查與音質驗證

日期：2026-10-25（Asia/Taipei）。基準：`54a7094e3623e9c64e8eba4530f91d49d51663f9`。

本輪重點是 YouTube／Spotify 連結、高音質、廣告處理與長時間播放的可靠性。四位代理分工閱讀與複審；共用 Cargo 輸出由單一代理統一建置，沒有以多專案同時建置期間的耗時宣稱效能提升。

## 結構與審查範圍

| 範圍 | 責任與驗證邊界 |
| --- | --- |
| `protocol`、`client` | track token／API 型別、REST／WebSocket 客戶端；檢查大小限制、URL／ID、驗證標頭及 PATCH 的 null 語意。 |
| `source` | 音源路由、Spotify／YouTube catalogue、HTTP／DNS 防護、有界 LRU 與並行解析。用 loopback API 驗證連結、分頁、媒體選取與讀取。 |
| `audio` | 原生解碼、重採樣、DSP、Opus、Ogg 與 FFmpeg。驗證非 48 kHz 來源、首尾音訊、編碼延遲、左右聲道及長串流記憶體界限。 |
| `config` | 新設定的高音質預設與既有明確設定／環境變數保留。 |
| `server` | 播放器狀態、工作階段、停止、容量／限流、webhook；透過真實 HTTP 客戶端與假的音訊／語音依賴驗證生命週期。 |
| `voice`、`dave` | gateway／UDP／RTP、DAVE 轉換、金鑰與 nonce、佇列及關閉；使用本機 Discord 模擬與獨立 DAVE 接收端，未使用真實 bot 憑證。 |
| `lyrics` | LRC／LRCLIB／YouTube Music 時間與文字解析；補極端整數、負時間與多段文字回歸。 |
| `node`、啟動／安裝／部署檔 | CLI、媒體工具、日誌、Linux 安裝器與 Windows 啟動；檢查設定保留及字串替換，Linux／Docker 的實際部署另列限制。 |

## 來源與播放行為

- Spotify 支援歌曲、專輯、播放清單、`intl-*` 地區前綴、embed、分享 query、fragment 與 Discord `<...>` 包裝；拒絕錯誤 ID、額外路徑及偽造主機。分頁回傳的 URL 必須留在相同 API origin 與版本路徑，避免外送 Bearer token。
- Spotify 僅供應 metadata；完整音源透過 YouTube 的歌名、歌手、長度比對取得。不使用短版 preview，也不解密 Spotify 音訊。單字母與單中文字名稱可以匹配，未要求的 cover、live、remix、karaoke、速度修改版與 preview 不會勝過原曲。
- YouTube 保留原始音軌與動態，再比較 bitrate。搜尋與播放清單跳過廣告 renderer 子樹；播放器只開啟選定歌曲的音訊 URL，不抓取 player 廣告項目。整合測試以三次預期 HTTP 請求驗證搜尋、選曲與完整 fixture 讀取。
- 不插入片頭／中途廣告；原影片已錄入的工商或口播仍屬原始音訊，沒有實作自動剪輯。
- 網路防護補上 IPv4 this-network、多播、退役 6to4 與 IPv6 特殊用途位址；修正將整個 `2001::/16` 誤當 Teredo，正常全球 IPv6 能連線。兩項回歸均先重現失敗再修復。

## 音訊與服務可靠性

- 新預設為 48 kHz 雙聲道、192 kbit/s Opus、complexity 10；既有明確指定的 128 kbit/s 或環境覆寫不會被改寫。原生／FFmpeg 的編碼選項明確設定，libopus 控制失敗會回報。
- 修復非 48 kHz 來源在套用固定 48 kHz 濾鏡前沒有轉換採樣率的問題；重採樣補齊首尾，Ogg EOS granule 裁去尾端 padding，原生 Opus 保留編碼器延遲中的最後音訊。
- Ogg 裁尾涵蓋末頁多個封包與非零起始 granule，並拒絕倒退、非法或截斷時間資料。每次只暫存一頁壓縮封包，PCM 維持單包最多 120 ms；11 項新增回歸包含跨頁封包及讀取界限。
- `FilterSet::default()` 原本把 gain 設為零，直接使用 FFmpeg 路徑會靜音；改為與空濾鏡解析一致的中性設定。真實 FFmpeg 測試檢查聲音能量及聲道分離，避免只數封包漏掉靜音。
- 重採樣歷史緩衝的相對／累計位置混用會隨播放時間增加記憶體；改以目前緩衝座標回收，新增分塊輸出一致性與記憶體界限測試。
- 極慢 timescale 優先排出已有輸出，避免持續預讀整曲；400 倍延長播放的測試輸出 80 秒，來源讀取仍不超過 128 KiB。大 seek 位置改用寬整數計算，避免溢位跳回開頭。
- 播放統計改讀即時語音連線，避免 `playing_players` 一直為零；相同音量／濾鏡與不適用的 seek 不重新開啟音源；`replace=false` 保留既有曲目時仍套用 pause。
- 背景工作先註冊通知再查持久關閉旗標，修復先 close、後排程時遺失停止通知並持續持有 Server 的問題。
- 歌詞時間使用非負檢查與 checked arithmetic；YouTube Music 合併所有文字 run，避免只顯示第一段。LRC 展開限制為 10,000 行／8 MiB，避免多個時間戳放大記憶體；webhook 錯誤不再記錄完整含 token URL。
- Linux 安裝器對 sed 替換值進行 literal escaping，密碼中的 `&`／`|` 與路徑中的 `&` 不再改壞設定。已驗證 Bash 語法與合成設定替換，沒有執行特權安裝。

## 驗證記錄

環境：Windows x64、Rust 1.99.0 GNU、MSYS2 UCRT64 libopus，以及本機實際 FFmpeg。以下均針對本輪最終程式碼執行：

| 驗證 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過。 |
| `cargo test --workspace --release --locked --no-fail-fast` | 369 passed、0 failed、3 ignored；包含 source 109 項與新增 Ogg 11 項。 |
| `cargo clippy --workspace --all-targets --release --locked -- -D warnings` | 通過。 |
| 設定 `RUSTLINK_TEST_FFMPEG` 後執行 pipeline 的 2 項 ignored 測試 | 2 passed，驗證時長、非靜音、聲道隔離、佇列背壓與釋放子程序。 |
| `cargo build --release --locked --bin rustlink`、`--version`、`--check` | 通過，正式版執行檔可啟動。 |
| 正式執行檔 HTTP smoke | health、未授權拒絕、DAVE 能力、真實 44.1 kHz WAV 解析／inspect、session／player PATCH、`replace=false`、統計、清理及 metrics 均通過。 |
| 真實 YouTube 分享連結 | `dQw4w9WgXcQ` 附 `list` 與 fragment，回傳 HTTP 200、正確曲名與 213,000 ms 時長。 |
| 真實 YouTube 音訊 | 經正式 source／server Playback／audio Engine 讀取同一影片前 3 秒，產出 150 個 Opus 封包；反解碼共 144,000 samples/channel，左右 PCM16 RMS 約 3,504／3,444，確認非靜音。使用預設 192,000 bit/s、complexity 10，未連接 Discord。 |
| Spotify 未設憑證 | 明確回報需要 `spotifyClientId` 的錯誤；真實帳戶 API 未驗證。 |
| 啟動設定 | 日誌確認預設 `opusBitrate=192000`；測試前後既有 `config.yaml` SHA-256 相同。 |
| 安裝腳本／差異檢查 | Bash 語法、literal 替換及 `git diff --check` 通過。 |

總計 **371 項不同測試通過**，另完成正式執行檔 API 與真實音訊 smoke；唯一未執行的 ignored 項目是 cache microbenchmark，本輪不宣稱新的吞吐量或效能倍數。測試輸出留於本機忽略的 `target/validation-final-*.log`、`target/validation-final-summary.json`、`target/validation-runtime-smoke.json` 與 `target/validation-live-audio.log`。

## 實際限制

本機沒有設定 Spotify API 憑證或 Discord bot 即時語音憑證；Spotify metadata／選曲與 Discord 語音以本機模擬服務驗證。真實 Spotify API 的帳戶權限、YouTube 地區／PO token／平台限制，以及 Discord 聲道能力與網路品質仍會影響實際使用。

192 kbit/s 是有損編碼的品質取捨，無法增加原始音源缺失的細節。測試包含音訊長度、能量、聲道隔離及濾鏡行為，沒有宣稱主觀聽感、無損輸出或真實 Discord 端到端品質已驗證。未在原生 Linux／Docker 執行部署；舊效能數字仍保留在 `performance.md` 並明確標為歷史量測。
