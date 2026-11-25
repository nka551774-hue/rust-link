# 審查紀錄：接手審查與音質驗證（歷史）

本頁記錄 **2026-10-25** 兩輪全庫審查（基準提交 `54a7094e` 與 `cc996267`）。後續的 YouTube 用戶端演進、512 KiB 預讀與單人頻道 DAVE 見 [YouTube](youtube.md) 與 [DAVE 與語音](voice.md)。

## 第一輪：音源、音訊與服務可靠性

- **Spotify 與 YouTube**：支援地區前綴、embed、分享參數與 `<...>` 包覆，限制分頁維持同一 API origin；Spotify 以歌名、歌手、時長比對 YouTube 音源並排除未要求的翻唱／現場／預覽標記。網路防護修正 `2001::/16` Teredo 誤判並補齊 IPv4/IPv6 特殊位址攔截。
- **音訊管線**：預設改為 48 kHz 雙聲道、192 kbit/s Opus、複雜度 10；修正非 48 kHz 來源在濾鏡前的重取樣順序、Ogg EOS granule 裁尾、`FilterSet::default()` 靜音問題、重取樣緩衝回收與極慢速 `timescale` 背壓。
- **服務與歌詞**：修正 `playing_players` 即時統計、關閉旗標競態、LRC 展開上限（10,000 行／8 MiB）、Webhook 錯誤網址遮蔽與 `install.sh` 的 `sed` 轉義。

## 第二輪：HTTP 串流與容器探測完整性

| 問題 | 修復內容 |
| --- | --- |
| `206` 區間與續傳一致性 | 嚴格驗證起訖位移、已知總長與 `Content-Length`；保留強 `ETag` 送 `If-Range`，拒絕未知總長的 `206` |
| `ureq` 自動 gzip 解壓破壞位移 | 工作區統一停用自動解壓，強制 `Accept-Encoding: identity` |
| 容器時長探測誤差 | 依 FLAC 24-bit 區塊長度遍歷、相容 MP4 FullBox 與 QuickTime `hdlr`、Ogg 需具 EOS 並扣除 Opus pre-skip |
| 暫停與換曲競態 | `speaking(false)` 與播放器狀態共用同一把鎖排序，語音佇列採非阻塞 `try_send` |
| 無效語音端點 | 拆舊連線前先驗證 hostname label 與非零連接埠 |

## 當時驗證數據

環境：Windows x64、Rust 1.99.0 GNU、MSYS2 UCRT64 `libopus`、本機 FFmpeg。

| 驗證項目 | 第一輪 | 第二輪 |
| --- | --- | --- |
| `cargo fmt` / `clippy -D warnings` | 通過 | 通過 |
| `cargo test --workspace --release --locked` | 369 passed、3 ignored | 386 passed、3 ignored |
| `RUSTLINK_TEST_FFMPEG` 整合測試 | 2 passed | 2 passed |
| 真實 YouTube 串流驗證 | 前 3 秒 150 個非靜音 Opus 封包 | 前 30 秒 1,500 包、跨越 5 個以上分段 |
