# 第三輪審查：Discord voice 與 DAVE（歷史）

起始版本 `3894d7a`。依使用者要求「直接模擬」，驗證使用本機 TCP WebSocket、UDP、真正的傳輸密碼實作、獨立的 `davey` 接收端與 libopus；沒有連線真實 Discord、沒有用 bot token，也沒有把模擬結果當成正式的 Discord 聲道驗證。

## 範圍

完整審閱 `crates/dave`（`media.rs`、`session.rs`、`tests.rs`、benches）與 `crates/voice`（`gateway.rs`、`transport.rs`、`validate.rs`、`lib.rs`、測試與新增的 `simulation.rs`）。兩個 crate 沒有 vendor 或產生的檔案；`benches/support/legacy_*` 是舊版效能比較樣本，不在正式服務內。

## 已確認並修正

1. **DAVE 協商前可啟用 sender。** 原本在 Session Description 之前接受 opcode 21 的 protocol 0／transition 0，會先啟用 transport-only sender，之後選擇 DAVE v1 也可能留下該狀態。新增失敗回歸後，opcode 21／22／24 必須等 Session Description 設定協商版本；external sender 可以先到的既有行為保留，加密演算法沒有改動。
2. **ID 驗證與 Identify 不一致。** `valid_id` 原本 trim 後接受帶空白的 ID，但實際 Identify 保留原字串。改為直接驗證原字串是 1–20 個 ASCII 數字、非零的 u64。

## 模擬

在 AES-GCM 與 XChaCha20-Poly1305 兩種傳輸下，讓合成的雙聲道音訊依序經過 Opus 編碼、DAVE、RTP-size 加密、UDP、解密、獨立的 DAVE 對端與 Opus 解碼。每種模式驗證 50 個音訊封包與兩次停講各 5 個靜音封包，共 60 包、每聲道 57,600 samples（1 秒音訊加 0.2 秒靜音）。逐包解密後的 Opus 與基準相同，解碼的 PCM 也完全相同，兩聲道的 RMS 都不是靜音；各觀察到 9 次心跳 ACK。

其他情境：speaking 的啟停順序、遠端 4014 close、重複的 Session Description、錯誤的 heartbeat nonce、local close 的冪等性與只回呼一次、以新的傳輸金鑰重新建立連線，以及提前降級不能完成握手。

## 重現（Windows PowerShell，GNU toolchain）

```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;C:\msys64\ucrt64\bin;$env:PATH"
$env:OPUS_LIB_DIR = 'C:\msys64\ucrt64\lib'
$env:RUSTLINK_TEST_FFMPEG = Join-Path (Get-Location) '.tools\ffmpeg.exe'
cargo test -p rustlink-voice -p rustlink-dave --release --locked --no-fail-fast
```

當時的結果：修改前 voice 19、DAVE 14 項通過；兩個新回歸分別因「提前接受 opcode 21」與「接受 `" 123"`」而正確失敗；修復後 voice 24、DAVE 15 項通過。`rustlink-audio` 只新增在 voice 的 dev-dependencies，`Cargo.lock` 只多一條相依邊，沒有升級套件。

## 限制

- 重新連線是呼叫者重新連線並完整 Identify、discovery 與協商；gateway 沒有自動的 opcode 7／9 Resume，斷線回報給播放器後須由呼叫者重建，不能宣稱透明重連。
- seek 與播放器世代、暫停狀態由 server crate 管理；這輪的語音模擬驗證的是命令排序與封包連續性。
- loopback 與合成音訊證明協定路徑與音訊傳輸沒有改變資料，不能代表真實 Discord 的網路、聲道限速、丟包恢復或主觀聽感。
- 這輪沒有新的 microbenchmark 或效能倍數宣稱。
