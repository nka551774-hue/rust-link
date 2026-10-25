# 第三輪 Discord voice／DAVE 審查

起始版本：`3894d7a`。依使用者「直接模擬」指示，驗證使用本機 TCP WebSocket、UDP、真正的傳輸密碼實作、獨立 davey 接收端與 libopus。未連線真實 Discord，未使用 bot token，也沒有把模擬結果當成正式 Discord 聲道驗證。

## 逐檔覆蓋

以下為完整閱讀的範圍，行數記錄於本輪修改及套用 crate 格式後。起始 15 個 tracked files 全部閱讀，另新增的模擬檔也完整審閱。這兩個 crate 沒有 vendor／generated 檔案；`benches/support/legacy_*` 是本專案舊版效能比較樣本，沒有進入正式服務。

| 檔案（相對 repo root） | 完整閱讀範圍 | 檢查重點 |
| --- | --- | --- |
| `crates/dave/Cargo.toml` | 1–29 | MLS／密碼依賴、zeroize、測試 peer 與 bench 邊界 |
| `crates/dave/benches/media.rs` | 1–30 | 比較樣本、輸出一致性與重用 buffer |
| `crates/dave/benches/support/legacy_media.rs` | 1–87 | 舊版 sender 比較實作，不是 runtime |
| `crates/dave/src/lib.rs` | 1–21 | 公共 API／錯誤介面 |
| `crates/dave/src/media.rs` | 1–149 | ratchet、nonce、footer、金鑰清理與溢位拒絕 |
| `crates/dave/src/session.rs` | 1–639 | 協商／transition 順序、MLS roster／commit／welcome／reset |
| `crates/dave/src/tests.rs` | 1–363 | 外部 peer 互通、重放、竄改、成員變更、失敗恢復 |
| `crates/voice/Cargo.toml` | 1–31 | production 與 dev-dependency 邊界 |
| `crates/voice/benches/support/legacy_transport.rs` | 1–93 | 舊版 AES／XChaCha 比較實作 |
| `crates/voice/benches/transport.rs` | 1–51 | 一致性比較，未於本輪重做效能量測 |
| `crates/voice/src/gateway.rs` | 1–484 | 握手、心跳、命令佇列、close、DAVE／UDP 排序 |
| `crates/voice/src/gateway/tests.rs` | 1–279 | 既有完整 DAVE 握手、SSRC discovery、心跳與背壓 |
| `crates/voice/src/lib.rs` | 1–121 | Connection／Factory 契約與 close 資訊 |
| `crates/voice/src/transport.rs` | 1–285 | RTP header／SSRC、兩種 AEAD、nonce 不重用、UDP discovery |
| `crates/voice/src/validate.rs` | 1–181 | Discord endpoint 與原始 snowflake 字串驗證 |
| `crates/voice/src/gateway/simulation.rs` | 1–437 | 新增真 loopback 與雙聲道 Opus 加解密整合 |

## 已確認並修正

1. **DAVE 協商前可啟用 sender。** 原本在 Session Description 前接受 opcode 21 的 protocol 0／transition 0，會先啟用 transport-only sender；之後選擇 DAVE v1 也可能留下該狀態。新增失敗回歸後，21／22／24 必須等 Session Description 設定協商版本。既有允許 external sender 先到的行為保留，沒有更動加密算法。
2. **ID 驗證與 Identify 不一致。** `valid_id` 原本 trim 後接受帶空白的 ID，但實際 Identify 保留原字串。改為直接驗證原字串為 1–20 個 ASCII 數字及非零 u64；有效 ID 格式不變。

新模擬在 AES-GCM 與 XChaCha20-Poly1305 兩種 transport 下，讓合成雙聲道音訊經 Opus 編碼、DAVE、RTP-size 加密、UDP、解密、獨立 DAVE peer 與 Opus 解碼。每種模式驗證 50 個音訊包與兩次停講各 5 個 silence 包，共 60 包、每聲道 57,600 samples（1 秒音訊加 0.2 秒 silence）。逐包解密後的 Opus 與基準相同，解碼 PCM 也完全相同，且兩聲道 RMS 均非靜音。本次各觀察到 9 次心跳 ACK。

其他新增情境涵蓋 speaking 的啟停順序、遠端 4014 close、重複 Session Description、錯誤 heartbeat nonce、local close 冪等性與只回呼一次、以新 transport key 重新建立連線，以及提前降級不能完成握手。

## 可重現驗證

Windows PowerShell 使用 Rust GNU toolchain，先設定：

```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;C:\msys64\ucrt64\bin;$env:PATH"
$env:OPUS_LIB_DIR = 'C:\msys64\ucrt64\lib'
$env:RUSTLINK_TEST_FFMPEG = Join-Path (Get-Location) '.tools\ffmpeg.exe'
```

| 階段 | 命令／本機 log | 結果 |
| --- | --- | --- |
| 修改前基準 | `cargo test -p rustlink-voice -p rustlink-dave --release --locked --no-fail-fast`；`target/round3-discord-baseline.log` | exit 0；voice 19、DAVE 14 pass |
| DAVE red | `cargo test -p rustlink-dave --release --locked transitions_before_session_description_cannot_enable_audio`；`target/round3-dave-transition-red.log` | exit 101；正確因提前接受 opcode 21 失敗 |
| ID red | `cargo test -p rustlink-voice --release --locked snowflakes_cannot_differ_from_the_identify_payload_by_whitespace`；`target/round3-voice-id-red.log` | exit 101；正確因接受 `" 123"` 失敗 |
| 修後與完整模擬 | `cargo test -p rustlink-voice -p rustlink-dave --release --locked --no-fail-fast -- --nocapture`；`target/round3-discord-green.log` | exit 0；voice 24、DAVE 15 pass |

`rustlink-audio` 只新增於 voice 的 dev-dependencies；Cargo.lock 只增加該相依邊，沒有升級套件。全庫最終測試／Clippy／release 建置由主代理統一整合；本文件不以目標 crate 結果代替全庫結果。

## 限制

- Fresh reconnect 是呼叫者重新連線並完整 Identify／discovery／協商。現有 gateway 沒有自動 opcode 7／9 Resume；斷線回報給播放器後須由呼叫者重建，不能宣稱透明重連。
- seek 與播放器世代／暫停狀態由 server crate 管理，本輪 voice 模擬驗證的是命令排序與封包連續性。相關 server 狀態機由主代理覆蓋。
- loopback 與合成音訊能證明協定路徑及音訊傳輸未變更資料；不能代表真實 Discord 網路、聲道限速、丟包恢復或主觀聽感。
- 本輪沒有新的 microbenchmark 或效能倍數宣稱；所有 Cargo 工作由同一代理串行執行。
