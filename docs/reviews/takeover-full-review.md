# 接手全庫審查（歷史）

基準：`main`（開始時與 `origin/main` 相同）。範圍是當時所有 tracked 檔案、既有未追蹤的來源與文件。安全原則：只做 loopback 與本機整合，不登入真實 Discord、不用帳號憑證、不送 webhook。

當時為 112 個檔案逐一建立了清單（原始碼逐行審查；lockfile、授權與部署資料依檔案類型核對來源、manifest 關係、格式與雜湊），全數完成。這份紀錄只保留發現、修正與限制；逐檔清單與本機 log 路徑已不再列出，因為倉庫的檔案組成之後已經改變。

## Discord voice 與 DAVE：對照規格

對照 [Discord Voice Connections](https://docs.discord.com/developers/topics/voice-connections)、其中的 Transport Encryption Modes、[DAVE protocol](https://github.com/discord/dave-protocol/blob/main/protocol.md)，並以 discord.py 與 discord.js 的實作交叉核對 wire mode 字串。

- **重大相容性錯字**：`aead_xchacha20poly1305_rtpsize` 少了一個 `_`。正式的解析與名稱、文件與 benchmark 設定現在都使用規格的 `aead_xchacha20_poly1305_rtpsize`。測試不再把 `mode.name()` 同時餵給假的伺服器與客戶端，改以固定的規格字串建立 Ready、驗證 Select Protocol、送出 Session Description；只有 XChaCha 可用時也必須完成連線，舊拼字明確被拒絕。
- **Hello 的 `heartbeat_interval`**：接受有限的正數小數毫秒，拒絕 0、負值、非數字，以及被 `Duration` 捨入為零的極小正數；第一個 heartbeat 在完整的 interval 後才送。取捨是未 ACK 最慢要到下一個伺服器 interval 才會被偵測；明確的 WebSocket close 仍立即處理。
- **sequence**：JSON 的 `seq` 依規格保持 optional `u16`，伺服器送來的 DAVE binary 前綴讀 big-endian `u16`。沒有新增 Voice Resume：斷線後由機器人重新提供語音狀態，走完整 Identify，建立新的傳輸金鑰與 nonce 空間。
- **Speaking 與 nonce**：第一個 Opus 封包之前，`Speaking(true)` 先排進同一個有界佇列，loopback 以 wire 順序驗證。nonce 每次嘗試傳送前單調消耗、同一個傳輸禁止重裝 key、32 位元用盡即失敗。DAVE 只在合法的轉換點切換 sender，沒有 session description 就收到轉換會被拒絕，generation 與 nonce 邊界都有回歸。

## 其他可重現的缺陷

- **Webhook**：公開契約是「queue full 屬於致命」，但原本只印 stderr 並漏掉事件；queue full 與 worker disconnected 現在都呼叫 fatal callback。Discord embed 同時套用每欄與 6000 的總文字預算；以 UTF-16 單位計是對 emoji 較保守的預算（這輪沒找到官方對計數方式的明確說明，不宣稱 Discord 實際用 UTF-16）。
- **Equalizer**：缺少必填的 `band` 時原本靜默當成 band 0，現在拒絕該 payload；高頻 biquad 的 octave bandwidth 補上 bilinear correction，並以原生與真實 FFmpeg 的 ignored 測試比對。
- **InnerTube**：renderer 的負值、NaN 或 infinity 時長原本可以進入曲目中繼資料，現在被當作未知時長。
- **解碼**：不再把來源的 `UnexpectedEof` 誤當成 Symphonia 的正常 EOF；FFmpeg 輸入執行緒保留來源的讀取錯誤，即使子行程對截斷的輸入回報成功也會回報播放失敗；Vorbis 零 frame 的 priming 封包不再進入會索引空緩衝的 interleaver；只有實際辨識為 Opus 的 Ogg 才走 Opus reader。
- **歌詞**：`lyricsSource: ' NoNe '` 通過驗證後原本仍可能啟用網路 provider，執行期判斷現在同樣 trim；LRCLIB 的 featured-credit 清理改用 ASCII 大小寫，避免 Unicode 轉小寫改變 byte offset 而切錯或 panic；ID3v2.4 frame size 依 synchsafe 解析，UTF-16 BOM 文字去掉結尾 NUL；Discord snowflake 不再 trim 後才接受。

## 保留的界線與風險

- 沒有登入真實 Discord、沒有 bot 或語音憑證，也沒有實際投遞 webhook：語音、DAVE、UDP、重連與 webhook 都以 loopback 或本機 fixture 驗證，所以不能宣稱真實 guild、區域路由或上游服務的端到端已驗證。
- 沒有強 ETag 的 HTTP 媒體只能核對位移、片段長度與總長，仍無法辨認同大小的上游替換。Ogg reader 依 framing、granule 與解碼器驗證，但沒有在 reader 層計算頁面 CRC 或連續序號，損壞資料的提早診斷有限。
- Windows 的 FFmpeg 自動下載會核對同來源的 checksum 清單；Linux 與 macOS 的 provider 依賴 HTTPS 與檔案大小下限，沒有獨立固定的 digest 或簽章。這是既有輔助工具的供應鏈界線，在沒有指定發佈政策下，不更動來源或固定版本。
- 沒有效能變更，也沒有新的效能宣稱；歷史 benchmark 維持標示為舊的基線。

## 當時的驗證

| Gate | 結果 |
| --- | --- |
| server 與 voice 的 targeted 測試 | server 34、voice 26 通過 |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | 通過 |
| `cargo test --workspace --locked --no-fail-fast` | **405 passed、0 failed、6 ignored**（5 項 FFmpeg、1 項 cache benchmark） |
| 5 項 ignored 的 FFmpeg 整合測試 | 5 passed：真實 FFmpeg，涵蓋失敗輸入、佇列與子行程、EQ、立體聲設定、Vorbis／FLAC 原生路徑 |
| `cargo build --workspace --release --locked` | 通過 |
| shell 語法 | `install.sh`、`start.sh` 以兩套 Bash 各自 `-n`，全部通過 |
| 啟動 smoke | release 執行檔、`start.ps1`、`start.bat`、兩套 Bash 的 `start.sh` 都以 `--version` 回覆 `Rustlink 0.5.0 (Rustlink API 1)`，未啟動伺服器 |
| 安裝程式 | 兩套 Bash 的 `install.sh --help` 通過，未安裝任何套件或服務 |

過程中的失敗也如實保留：第一次 targeted 編譯因新增的 webhook 測試少匯入 `Mutex` 失敗，補上後重跑全綠；第一次 Clippy 以 `chunks_exact_to_as_chunks` 擋下新的 simulation，改用 `as_chunks::<2>()` 後重跑全綠；第一次 MSYS2 啟動 smoke 因呼叫端的 PATH 沒有 MSYS `/usr/bin`、找不到 `dirname` 而失敗，補上明確路徑後，同一份腳本重跑通過。這三項都不是被忽略的失敗。

未執行 cache microbenchmark，沒有效能宣稱；沒有使用真實 Discord 或 Spotify 憑證，也沒有向真實的 webhook 投遞資料。
