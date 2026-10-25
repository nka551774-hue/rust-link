# Rustlink 0.5.0 效能與驗證

這次改善集中在反覆執行的解析、快取、音訊封包與加密流程。Opus 仍使用原本的 bitrate／complexity 設定；DAVE 的金鑰切換、認證與 nonce 防重用規則維持不變。

## 已實作的改善

- **來源快取**：改為有容量與 TTL 限制的 typed LRU。命中以 O(1) 更新順序、共享 `Arc<LoadResult>`，不再重新解析 JSON；API 直接序列化結果，播放器只複製需要播放的單曲。記憶體預算同時估算字串、容器與附加資料的配置容量。
- **並行解析**：相同查詢共用一次工作；成功、錯誤及過大而不快取的結果都會交給正在等候的呼叫者。等待、重試與分頁共用解析期限，工作中斷也會喚醒等待者。
- **來源連線**：Spotify token 與 SoundCloud client ID 隨服務保留，避免每次建立 catalogue 時重抓。修正 Spotify 認證，減少 SoundCloud 重複請求。長音軌採每次 I/O 的閒置期限；續傳必須符合 `206` 與正確的 `Content-Range` 起點。
- **音訊**：Ogg 與 FFmpeg 重複使用封包緩衝區，頁面資料分批整理；降混係數只在聲道配置改變時更新。48 kHz 等速播放略過重採樣，完整音框直接處理。FFmpeg 佇列上限為 8 個 20 ms 封包，暫停時會向上游施加背壓。
- **語音加密**：DAVE 與 AES RTP 傳輸在金鑰建立時準備加密器，DAVE 直接修改已有封包，語音佇列回收緩衝區。AES 金鑰排程與認證金鑰在丟棄時清零。XChaCha 保留每封包建立輕量加密器的路徑，沒有套用量測中較慢的快取版本。
- **暫停**：播放器等待條件變化，不再固定喚醒輪詢。

同時修正 Ogg 首包遺失、pre-skip／輸出增益、合法的 120 ms Opus 輸入封包、截斷資料、pitch／rate 方向，以及高速伸縮跨區塊的越界問題。超出原生伸縮範圍的合法組合直接交給 FFmpeg，避免靜默改變使用者指定的速度。

## 本機量測

2026-10-25，Windows x64 GNU、AMD Ryzen 7 260（8 核心／16 執行緒）、Rust 1.99.0、release／thin LTO。量測期間沒有其他 Rust 建置；數值仍受系統排程、CPU 頻率與背景程式影響。

快取測試使用相同 mutex 與輸入，採 3 批中位數，每批預熱 100 次。它只量測取得結果，**不包含 HTTP JSON 序列化、網路或播放**。

| 快取工作 | 舊版 JSON 快取 | 新版 owned 結果 | 新版 shared 結果 |
| --- | ---: | ---: | ---: |
| 1024 entries、每筆 1 track；50,000 次／批 | 6.1602 μs／次 | 0.2703 μs／次 | 0.0498 μs／次 |
| 32 entries、每筆 100 tracks；10,000 次／批 | 284.5252 μs／次 | 30.7008 μs／次 | 0.0845 μs／次 |

加密基準在同一個程序比較保留的舊演算法與目前程式，兩邊使用相同依賴與清零功能。每輪 200,000 次，採 7 輪中位數，配置次數另行計數，排除首次金鑰建立與緩衝區預熱。

| 工作 | 舊路徑 | 新路徑 | Rust 配置／重配置 |
| --- | ---: | ---: | ---: |
| DAVE，320-byte Opus | 395.3 ns／封包 | 213.5 ns／封包 | 2 → 0／封包 |
| AES-256-GCM RTP，336-byte payload | 429.0 ns／封包 | 259.8 ns／封包 | 0 → 0／封包 |
| XChaCha20-Poly1305 RTP，相同 payload | 1532.0 ns／封包 | 1537.3 ns／封包 | 0 → 0／封包 |

XChaCha 的差異落在量測波動內，不宣稱加速。這些測試也不包含 gateway 排程、UDP 傳送或 Discord 延遲。

音訊基準使用每頁 50 個、每個 160 bytes 的固定 Ogg 封包。專案保留舊 reader 演算法，計時前驗證舊版、新版 owned、新版 reuse 三者的 200,000 個封包與完整 checksum 相同。同程序的三輪結果，每輪取 7 次中位數：舊版 13.546／11.072／11.005 ms，新版 reuse 3.580／3.472／3.221 ms；中位數為 **11.072 → 3.472 ms**。Rust 配置／重配置由 **200,004 次降為 5 次**；新版串流路徑不再逐封包配置。

另外，修改前保存的基準執行檔與新版交錯測量，中位數為 8.707 → 3.411 ms（約減少 61%）。保留舊演算法重新編譯與原先執行檔的計時不同，因此不把任一比例視為所有主機或完整播放的保證。

10 秒、48 kHz stereo WAV → Opus，維持 128,000 bit/s、complexity 10：舊版三輪為 87.057／87.748／88.873 ms，新版為 83.407／84.299／91.844 ms。範圍重疊，**不宣稱完整編碼穩定加速**；Rust 配置次數為 451 → 441。配置計數不包含 libopus 的 C heap。

## 重跑與檢查

需先安裝 Rust 與系統 libopus；Windows 使用與啟動腳本相同的 GNU／MSYS2 環境。避免和其他建置同時量測。

```sh
cargo bench -p rustlink-dave --bench media
cargo bench -p rustlink-voice --bench transport
cargo bench -p rustlink-audio --bench hot_paths
cargo test -p rustlink-source --release benchmark_resolution_cache -- --ignored --nocapture --test-threads=1

cargo fmt --all --check
cargo clippy --workspace --all-targets --release --locked -- -D warnings
cargo test --workspace --release --locked
```

設定 `RUSTLINK_TEST_FFMPEG` 為已安裝的 FFmpeg 路徑，可另外驗證佇列滿載後續播與停止子程序：

```sh
cargo test -p rustlink-audio --release ffmpeg_queue_resumes_after_backpressure_and_drop_closes_the_child -- --ignored
```

完整 Discord 實際連線仍須 bot 的即時語音憑證；本機 DAVE 測試的範圍見 [voice.md](voice.md)。音源能力與限制見 [sources.md](sources.md)。

本次驗證記錄：完整 workspace 的 306 個測試通過；最後續傳修正新增 2 個來源測試後，來源 crate 80 個測試及其 Clippy 再次通過。另有真實 FFmpeg 背壓／停止測試、全 workspace Clippy、格式檢查、最終 release 的 HTTP API／設定保留測試，以及來源版與封裝版 BAT／MSYS2 Bash 啟動測試通過。兩個主要 release 路徑的執行檔皆已更新為 0.5.0。未執行真實 Discord、原生 Linux 或 Docker 建置測試。
