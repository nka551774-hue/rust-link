# 效能量測（歷史紀錄）

本頁記錄 **2026-10-25 首輪效能改造**之基準數據（AMD Ryzen 7 260、Windows x64 GNU、Rust 1.99.0、`--release` thin LTO）。最新實測數據請見 [README 實測效能基準](../README.md#benchmarks)。

## 核心最佳化項目

- **來源快取與合併查詢**：以具容量與 TTL 上限的 `Arc<LoadResult>` LRU 快取取代重複 JSON 解析；`Singleflight` 合併相同查詢的併發上游請求，並複用 Spotify token 與 SoundCloud `client_id`。
- **零配置音訊與加密**：Ogg 與 FFmpeg 重用封包緩衝區；DAVE v1 原地修改封包、RTP 佇列回收緩衝並於金鑰卸載時清零；播放器暫停改為條件變數等待。

## 當時實測對照

| 測試項目 | 改造前 | 改造後 | Rust 堆積配置變化 |
| --- | ---: | ---: | --- |
| 單曲快取命中（1,024 筆） | 6.16 µs / 次 | **0.050 µs / 次** | 共享 `Arc<LoadResult>` |
| 百首歌單快取命中（32 筆） | 284.53 µs / 次 | **0.085 µs / 次** | 共享 `Arc<LoadResult>` |
| DAVE v1 封包加密（320 B） | 395.3 ns / 包 | **213.5 ns / 包** | `2 → 0` 次 / 包 |
| AES-256-GCM RTP（336 B） | 429.0 ns / 包 | **259.8 ns / 包** | `0 → 0` 次 / 包 |
| XChaCha20-Poly1305 RTP | 1,532.0 ns / 包 | **1,537.3 ns / 包** | `0 → 0` 次 / 包 |
| Ogg Opus 200,000 包解封裝 | 11.07 ms | **3.47 ms** | `200,004 → 5` 次 |

## 重現指令

```sh
cargo bench -p rustlink-dave --bench media
cargo bench -p rustlink-voice --bench transport
cargo bench -p rustlink-audio --bench hot_paths
cargo test -p rustlink-source --release benchmark_resolution_cache -- --ignored --nocapture --test-threads=1
```
