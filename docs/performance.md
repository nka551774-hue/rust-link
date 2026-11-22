# 效能量測（歷史紀錄）

這一頁的時間與配置次數，是 **2026-10-25 第一次效能改造**的量測，不是後來 192 kbit/s 預設、音訊完整性修正，以及 2026-11 的 YouTube 用戶端與語音改動之後的重新量測。目前不宣稱新的整體加速比例。正確性與整合測試的紀錄見[審查紀錄](optimization-audit.md)。

改善集中在會反覆執行的解析、快取、音訊封包與加密流程。Opus 仍用原本的位元率與複雜度；DAVE 的金鑰切換、認證與 nonce 防重用規則不變。

## 已實作的改善

- **來源快取**：改為有容量與 TTL 限制的具型別 LRU。命中以 O(1) 更新順序、共享 `Arc<LoadResult>`，不再重新解析 JSON；API 直接序列化結果，播放器只複製需要的單首曲目。記憶體預算同時估算字串、容器與附加資料的配置容量。
- **並行解析**：相同查詢共用一次工作；成功、錯誤與太大而不快取的結果都交給正在等的呼叫者。等待、重試與分頁共用解析時限，工作中斷也會喚醒等待者。
- **來源連線**：Spotify token 與 SoundCloud client ID 隨服務保留，不必每次建 catalogue 都重抓。修正 Spotify 認證、減少 SoundCloud 重複請求。長音軌採每次 I/O 的閒置時限；續傳必須符合 `206` 與正確的 `Content-Range` 起點。
- **音訊**：Ogg 與 FFmpeg 重複使用封包緩衝區，頁面資料分批整理；降混係數只在聲道配置改變時更新。48 kHz 等速播放略過重採樣，完整音框直接處理。FFmpeg 佇列上限 8 個 20 ms 封包，暫停時對上游施加背壓。
- **語音加密**：DAVE 與 AES RTP 傳輸在金鑰建立時準備好加密器，DAVE 直接修改既有的封包，語音佇列回收緩衝區。AES 金鑰排程與認證金鑰在丟棄時清零。XChaCha 保留每個封包建立輕量加密器的路徑，沒有採用量測中較慢的快取版本。
- **暫停**：播放器等待條件變化，不再固定喚醒輪詢。

同時修正了 Ogg 首包遺失、pre-skip 與輸出增益、合法的 120 ms Opus 輸入封包、截斷資料、pitch／rate 方向，以及高速伸縮跨區塊的越界問題；超出原生伸縮範圍的合法組合直接交給 FFmpeg，避免靜默改變使用者指定的速度。

## 量測環境

Windows x64 GNU、AMD Ryzen 7 260（8 核心／16 執行緒）、Rust 1.99.0、release 加 thin LTO。量測期間沒有其他 Rust 建置；數值仍受系統排程、CPU 頻率與背景程式影響。

### 快取

相同的 mutex 與輸入，3 批取中位數，每批先暖機 100 次。只量取得結果，**不含 HTTP JSON 序列化、網路或播放**。

| 工作 | 舊版 JSON 快取 | 新版 owned | 新版 shared |
| --- | ---: | ---: | ---: |
| 1024 筆、每筆 1 首；每批 50,000 次 | 6.1602 μs／次 | 0.2703 μs／次 | 0.0498 μs／次 |
| 32 筆、每筆 100 首；每批 10,000 次 | 284.5252 μs／次 | 30.7008 μs／次 | 0.0845 μs／次 |

### 加密

同一個行程裡比較保留的舊演算法與目前的程式，兩邊用相同的相依與清零功能。每輪 200,000 次，7 輪取中位數，配置次數另計，排除第一次金鑰建立與緩衝區暖機。

| 工作 | 舊路徑 | 新路徑 | Rust 配置／重配置 |
| --- | ---: | ---: | ---: |
| DAVE，320 位元組 Opus | 395.3 ns／封包 | 213.5 ns／封包 | 2 → 0／封包 |
| AES-256-GCM RTP，336 位元組 payload | 429.0 ns／封包 | 259.8 ns／封包 | 0 → 0／封包 |
| XChaCha20-Poly1305 RTP，相同 payload | 1532.0 ns／封包 | 1537.3 ns／封包 | 0 → 0／封包 |

XChaCha 的差異在量測波動之內，不宣稱加速。這些測試也不含 gateway 排程、UDP 傳送或 Discord 延遲。

### Ogg 讀取

固定的 Ogg 資料：每頁 50 個、每個 160 位元組的封包。專案保留舊 reader 演算法，計時前先驗證舊版、新版 owned、新版 reuse 三者對 200,000 個封包與完整 checksum 的結果相同。同一行程三輪，每輪 7 次取中位數：舊版 13.546／11.072／11.005 ms，新版 reuse 3.580／3.472／3.221 ms；中位數 **11.072 → 3.472 ms**。Rust 配置與重配置由 **200,004 次降到 5 次**，新版串流路徑不再逐封包配置。

另外，修改前保存的基準執行檔與新版交錯量測，中位數 8.707 → 3.411 ms（約少 61%）。保留舊演算法重新編譯的計時與原本的執行檔不同，因此不把任何一個比例當成適用所有主機或完整播放的保證。

### 編碼

10 秒、48 kHz 立體聲 WAV 轉 Opus，維持 128,000 bit/s、複雜度 10：舊版三輪 87.057／87.748／88.873 ms，新版 83.407／84.299／91.844 ms。範圍重疊，**不宣稱完整編碼有穩定的加速**；Rust 配置次數 451 → 441（不含 libopus 的 C heap）。

## 重跑

需要先安裝 Rust 與系統 libopus；Windows 用與啟動腳本相同的 GNU／MSYS2 環境。避免與其他建置同時量測。

```sh
cargo bench -p rustlink-dave --bench media
cargo bench -p rustlink-voice --bench transport
cargo bench -p rustlink-audio --bench hot_paths
cargo test -p rustlink-source --release benchmark_resolution_cache -- --ignored --nocapture --test-threads=1

cargo fmt --all --check
cargo clippy --workspace --all-targets --release --locked -- -D warnings
cargo test --workspace --release --locked
```

設定 `RUSTLINK_TEST_FFMPEG` 為已安裝的 FFmpeg 路徑，可另外驗證佇列滿載後續播與停止子行程：

```sh
cargo test -p rustlink-audio --release ffmpeg_queue_resumes_after_backpressure_and_drop_closes_the_child -- --ignored
```

完整的 Discord 連線仍需要機器人的即時語音憑證；本機 DAVE 測試的範圍見 [DAVE 與語音](voice.md)，音源能力與限制見[音源](sources.md)。
