# 接手全庫審查（歷史）

基準：`main`（逐行審閱當時全數 112 個追蹤檔案與規範對照）。

## 關鍵規格對照與缺陷修正

- **Discord Voice 與 DAVE**：修正 `aead_xchacha20_poly1305_rtpsize` 缺少底線之協定名稱；接受有限正小數毫秒 `heartbeat_interval` 並於完整週期後送出首個心跳；JSON `seq` 與 DAVE 二進位前綴統一為 `u16`；確保 `Speaking(true)` 先於首個 Opus 封包入列，且單調消耗 32-bit RTP nonce。
- **Webhook 與濾鏡**：修正 Webhook 佇列滿時未觸發致命回呼之問題，並對 Discord Embed 套用欄位與總長上限；拒絕缺少必填 `band` 的 Equalizer 請求並補上高頻雙二階濾波器的 bilinear bandwidth 修正。
- **音源、解碼與歌詞**：將 InnerTube 負值／NaN／Infinity 時長視為未知；區分來源 `UnexpectedEof` 與正常結尾；跳過 Vorbis 零音框 priming 封包；`lyricsSource` 比對前執行 `trim`，LRCLIB 改用 ASCII 大小寫偏移清理，ID3v2.4 依 synchsafe 解析。

## 當時驗證結果

`cargo fmt --all --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings`、`cargo test --workspace --locked --no-fail-fast`（**405 passed、0 failed、6 ignored**）、5 項真實 FFmpeg 整合測試、`bash -n` 腳本檢查與跨平台 `--version` 啟動檢查全數通過。
