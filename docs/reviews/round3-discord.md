# 第三輪審查：Discord Voice 與 DAVE（歷史）

基準提交 `3894d7a`。以本機 TCP WebSocket、UDP、真實傳輸加密、獨立 `davey` 對端與 `libopus` 離線模擬完整語音與雙層加密鏈路。

## 發現與修正

1. **禁止未協商提前降級**：修正原本在收到 `Session Description` 設定協定版本前就接受 Opcode 21（`protocol 0 / transition 0`）啟用純傳輸發送器的問題；Opcode 21／22／24 現必須等待 `Session Description` 完成協商。
2. **嚴格 Snowflake 驗證**：`valid_id` 不再 `trim` 空白，直接驗證原始字串為 1–20 位 ASCII 數字且為非零 `u64`。

## 本機全鏈路模擬與回歸

於 `aead_aes256_gcm_rtpsize` 與 `aead_xchacha20_poly1305_rtpsize` 兩種模式下，驗證雙聲道合成音訊經 Opus 編碼 → DAVE v1 → RTP 加密 → UDP → 雙層解密 → `davey` 與 Opus 解碼（50 個音訊封包 + 兩段各 5 個靜音封包、9 次心跳 ACK），解密還原之 Opus 與 PCM 完全一致。另涵蓋 `Speaking` 排序、`4014` 關閉、錯誤心跳 nonce、冪等關閉與金鑰重建測試（`voice` 24 項、`dave` 15 項全數通過）。
