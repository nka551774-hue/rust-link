# RustLink 接手全庫逐檔審查

- 基準分支：`main`（開始時與 `origin/main` ahead/behind = 0/0）
- 範圍：所有 Git tracked 檔、既有 untracked 來源／文件，以及本報告。
- 原則：每一檔完整閱讀；lockfile、授權文字與生成／部署資料依檔案類型核對來源、manifest 關係、格式與雜湊，不把搜尋或局部抽查記成完整審查。
- 安全：只做 loopback／本機整合，不登入真實 Discord、不使用帳號憑證、不送 webhook。
- 狀態：112/112 已完成；`[x]` 表示依該檔案類型完整閱讀或核對。發現、修正與驗證寫在清單後。

## 清單（112 項）

- [x] `.dockerignore` — 完整閱讀／依檔案類型核對；未發現需另行修改的可重現缺陷
- [x] `.gitattributes` — 完整閱讀／依檔案類型核對；未發現需另行修改的可重現缺陷
- [x] `.github/workflows/ci.yml` — 完整閱讀；核對 fmt/clippy/test、FFmpeg 與平台矩陣
- [x] `.github/workflows/release.yml` — 完整閱讀；核對 release targets、封裝與產物流程
- [x] `.gitignore` — 完整閱讀／依檔案類型核對；未發現需另行修改的可重現缺陷
- [x] `AGENTS.md` — 完整閱讀；與目前實作、操作界線及相鄰文件交叉核對
- [x] `Cargo.lock` — 以產生檔類型核對；SHA-256 `6ACC35774EB1E02E2A1371D1D14C4E6C15EBB49AC71C47A5DF73379619670D2A`，351 個 registry package 均有 64-hex checksum，`cargo metadata --locked` 成功
- [x] `Cargo.toml` — 完整核對 manifest、feature、target 與 workspace 依賴關係
- [x] `Dockerfile` — 完整閱讀；設定／部署預設、安全界線與實作欄位交叉核對
- [x] `LICENSE` — 完整閱讀 GPL-3.0-or-later 原文並與 manifest license 宣告核對
- [x] `NOTICE` — 完整閱讀並核對 Discord/DAVE、OpenMLS、davey 與歷史來源歸屬
- [x] `README.md` — 完整閱讀；與目前實作、操作界線及相鄰文件交叉核對
- [x] `benchmarks/measure.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `compose.yaml` — 完整閱讀；設定／部署預設、安全界線與實作欄位交叉核對
- [x] `config.example.yaml` — 完整閱讀；設定／部署預設、安全界線與實作欄位交叉核對
- [x] `crates/audio/Cargo.toml` — 完整核對 manifest、feature、target 與 workspace 依賴關係
- [x] `crates/audio/benches/hot_paths.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/audio/benches/support/legacy_ogg.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/audio/build.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/audio/src/decode.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/audio/src/dsp.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/audio/src/engine.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/audio/src/error.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/audio/src/ffmpeg.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/audio/src/filters.rs` — 完整逐行審查；修正 equalizer 缺少必填 `band` 時靜默落到 band 0，新增拒絕回歸
- [x] `crates/audio/src/lib.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/audio/src/ogg.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/audio/src/opus.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/audio/src/opus_decode.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/audio/src/pure.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/audio/src/resample.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/audio/tests/ogg_granule.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/audio/tests/pipeline.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/client/Cargo.toml` — 完整核對 manifest、feature、target 與 workspace 依賴關係
- [x] `crates/client/examples/bot.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/client/src/lib.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/config/Cargo.toml` — 完整核對 manifest、feature、target 與 workspace 依賴關係
- [x] `crates/config/src/env.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/config/src/lib.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/config/src/validate.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/dave/Cargo.toml` — 完整核對 manifest、feature、target 與 workspace 依賴關係
- [x] `crates/dave/benches/media.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/dave/benches/support/legacy_media.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/dave/src/lib.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/dave/src/media.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/dave/src/session.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/dave/src/tests.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/lyrics/Cargo.toml` — 完整核對 manifest、feature、target 與 workspace 依賴關係
- [x] `crates/lyrics/src/lib.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/lyrics/src/lrc.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/lyrics/src/lrclib.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/lyrics/src/ytmusic.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/node/Cargo.toml` — 完整核對 manifest、feature、target 與 workspace 依賴關係
- [x] `crates/node/src/linux.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/node/src/logging.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/node/src/main.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/node/src/tools.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/protocol/Cargo.toml` — 完整核對 manifest、feature、target 與 workspace 依賴關係
- [x] `crates/protocol/src/codec.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/protocol/src/lib.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/protocol/src/types.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/server/Cargo.toml` — 完整核對 manifest、feature、target 與 workspace 依賴關係
- [x] `crates/server/src/cpu.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/server/src/lib.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/server/src/playback.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/server/src/player.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/server/src/ratelimit.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/server/src/server.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/server/src/session.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/server/src/webhook.rs` — 完整逐行審查；queue full/worker stopped 改走 fatal 契約，採保守 UTF-16 預算並加 emoji 回歸
- [x] `crates/server/tests/api.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/source/Cargo.toml` — 完整核對 manifest、feature、target 與 workspace 依賴關係
- [x] `crates/source/src/cache.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/source/src/catalog.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/source/src/deadline.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/source/src/flight.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/source/src/innertube.rs` — 完整逐行審查；拒絕負值／NaN／無限大媒體時長，新增回歸
- [x] `crates/source/src/lib.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/source/src/matching.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/source/src/media.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/source/src/netguard.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/source/src/probe.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/source/src/test_http.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/source/tests/media_continuation.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/source/tests/probe_metadata.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/voice/Cargo.toml` — 完整核對 manifest、feature、target 與 workspace 依賴關係
- [x] `crates/voice/benches/support/legacy_transport.rs` — 完整逐行審查；benchmark 輸入改接受 canonical mode，保留舊實作作歷史基線
- [x] `crates/voice/benches/transport.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/voice/src/gateway.rs` — 完整逐行審查；依 Hello 小數 interval 排程，拒絕捨入為零，維持 ACK 失敗關閉
- [x] `crates/voice/src/gateway/simulation.rs` — 完整逐行審查；固定規格 literal 驗證 XChaCha negotiation、Speaking-before-audio、DAVE/UDP 與重連 key
- [x] `crates/voice/src/gateway/tests.rs` — 完整逐行審查；新增小數／零捨入 interval 及首個 heartbeat 時序回歸
- [x] `crates/voice/src/lib.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `crates/voice/src/transport.rs` — 完整逐行審查；修正 canonical XChaCha mode 字串並明確拒絕舊拼字
- [x] `crates/voice/src/validate.rs` — 完整逐行審查；介面、錯誤路徑、界限、並行／資源生命週期與既有測試一併核對
- [x] `deploy/rustlink.service` — 完整閱讀；設定／部署預設、安全界線與實作欄位交叉核對
- [x] `docs/api.md` — 完整閱讀；與目前實作、操作界線及相鄰文件交叉核對
- [x] `docs/architecture.md` — 完整閱讀；與目前實作、操作界線及相鄰文件交叉核對
- [x] `docs/configuration.md` — 完整閱讀；與目前實作、操作界線及相鄰文件交叉核對
- [x] `docs/optimization-audit.md` — 完整閱讀；與目前實作、操作界線及相鄰文件交叉核對
- [x] `docs/performance.md` — 完整閱讀；與目前實作、操作界線及相鄰文件交叉核對
- [x] `docs/reviews/round3-discord.md` — 完整閱讀；作為既有未追蹤 round3 證據交叉核對，不當成當輪重新實測
- [x] `docs/reviews/takeover-full-review.md` — 本報告內容、112 項清單、來源對照、風險與 gate 紀錄完成後全文回讀
- [x] `docs/sources.md` — 完整閱讀；與目前實作、操作界線及相鄰文件交叉核對
- [x] `docs/third-party.md` — 完整閱讀；與目前實作、操作界線及相鄰文件交叉核對
- [x] `docs/voice.md` — 完整閱讀並與官方 voice/DAVE 規格及實作對照；更新 mode、heartbeat、seq、Speaking、Identify 重連界線
- [x] `docs/vps.md` — 完整閱讀；與目前實作、操作界線及相鄰文件交叉核對
- [x] `docs/youtube.md` — 完整閱讀；與目前實作、操作界線及相鄰文件交叉核對
- [x] `install.sh` — 完整逐行審查；僅做語法與安全 smoke，不執行特權安裝
- [x] `rustfmt.toml` — 完整閱讀／依檔案類型核對；未發現需另行修改的可重現缺陷
- [x] `start.bat` — 完整逐行審查；PowerShell 參數／exit code 轉送納入安全 smoke
- [x] `start.ps1` — 完整逐行審查；Windows GNU/MSYS2 bootstrap 與參數轉送納入安全 smoke
- [x] `start.sh` — 完整逐行審查；語法及安全 `--version` launch smoke 納入 gate

## 發現與修正

### Discord voice／DAVE 規格對照

- 主要規格：[Discord Voice Connections](https://docs.discord.com/developers/topics/voice-connections)、[Transport Encryption Modes](https://docs.discord.com/developers/topics/voice-connections#transport-encryption-modes)、[Discord DAVE protocol](https://github.com/discord/dave-protocol/blob/main/protocol.md)。另以 [discord.py voice_client.py](https://github.com/Rapptz/discord.py/blob/master/discord/voice_client.py) 與 [discord.js VoiceReceiver.ts](https://github.com/discordjs/discord.js/blob/main/packages/voice/src/receive/VoiceReceiver.ts) 交叉核對 wire mode literal。
- 修正重大相容性錯字：原本 `aead_xchacha20poly1305_rtpsize` 少了 `_`；production parse/name、文件與 benchmark setup 現使用規格的 `aead_xchacha20_poly1305_rtpsize`。測試不再以 `mode.name()` 同時餵給假 server/client，而以固定規格 literal 建立 Ready、驗證 Select Protocol、送 Session Description；只有 XChaCha 可用時也必須完成連線，舊拼字明確拒絕。
- Hello 的 `heartbeat_interval` 現接受有限正數小數毫秒，拒絕 0、負值、非數字，以及 `Duration` 捨入為零的極小正數；首個 heartbeat 在完整 interval 後送出。這遵從 Hello cadence 並減少額外控制流量，不把舊的立即 heartbeat 描述為協定違規。取捨是未 ACK 最慢到下一個 server interval 才被偵測；明確 WebSocket close 仍立即處理。
- JSON message `seq` 按規格保持 optional `u16`，server-to-client DAVE binary 前綴則讀取 big-endian `u16`；未新增 Voice Resume。斷線後目前由 bot 重新提供 voice state，走完整 Identify，建立新 transport key／nonce 空間，而不是 opcode 7 Resume 或緩衝事件重播。
- 首個 Opus packet 前會先把 `Speaking(true)` 排入相同有界佇列；loopback 以 wire 順序驗證。nonce 每次嘗試傳送前單調消耗、同一 transport 禁止重裝 key、32-bit 用盡即失敗；完整 Identify 重連使用新 transport key。DAVE 僅在合法轉換點切 sender，轉換前無 session description 會拒絕，generation／nonce 邊界已有回歸。

### 其他可重現缺陷

- Webhook 的公開契約是 queue full 為 fatal，但原本只印 stderr 並漏事件；queue full 與 worker disconnected 現皆呼叫 fatal callback。Discord embed 同時套每欄及 6000 總文字預算；UTF-16 單位只是對 emoji 較保守的安全預算（官方未在本輪找到明確 scalar/UTF-16 計數證據），不宣稱 Discord 必然用 UTF-16。補 emoji、50-player、超長 metadata、queue failure 及 `i64::MAX` 時長回歸。
- Equalizer 項目缺少必填 `band` 時原本靜默當 band 0；現拒絕該 payload。高頻 biquad octave bandwidth 補上 bilinear correction，並以原生／真實 FFmpeg ignored test 比對。
- InnerTube renderer 的負值、NaN 或 infinity 時長原本可進入 track metadata；現拒絕為未知時長並加回歸。
- Native decoder 不再把來源的 `UnexpectedEof` 誤當 Symphonia 正常 EOF；FFmpeg input thread 也保留來源 read error，即使 child 對截斷輸入回成功仍回報播放失敗。Vorbis 零-frame priming packet 不再進入會索引空 buffer 的 interleaver；Ogg 只有實際辨識為 Opus 才走 Opus reader。
- `lyricsSource: ' NoNe '` 經驗證後原本仍可能啟用網路 provider；runtime 判斷現同步 trim。LRCLIB featured-credit 清理改用 ASCII case，避免 Unicode lowercase 改變 byte offset 後切錯或 panic。
- ID3v2.4 frame size 現按 synchsafe 解析，UTF-16 BOM 文字移除結尾 NUL；長中文 title、後續 artist 與大小端回歸均涵蓋。Discord snowflake 不再 trim 後接受，避免驗證內容與 Identify wire payload 不同。

### 審查後保留的界線／風險

- 未登入真實 Discord、未使用 bot/voice 憑證，也未實際投遞 webhook；voice、DAVE、UDP、reconnect 與 webhook 皆以 loopback／本機 fixture 驗證。因此不能宣稱真實 guild、區域路由或上游服務端到端已驗證。
- 沒有強 ETag 的 HTTP media 只能核對 offset、片段長度與總長，仍無法辨認同大小的上游替換；文件已明列。Ogg reader 依 framing、granule 與 decoder 驗證，但未在 reader 層計算頁面 CRC／連續序號，損壞資料的提早診斷仍有限。
- Windows FFmpeg 自動下載會核對同來源 checksum 清單；Linux/macOS provider 目前依 HTTPS 與檔案下限，沒有獨立固定 digest/signature。這是既有 helper supply-chain 界線，本輪不在未指定發佈政策下改來源或 pin 版本。
- 未做效能變更或新效能宣稱；歷史 benchmark 保持明確標示為舊基線。共用 `target` 的 Cargo gate 全程序列執行。

## 驗證紀錄

原始輸出與每個 exit code 留在 Git 忽略的 `logs/takeover/`。所有 Cargo 工作都在此
repository 內序列執行；沒有和 benchmark 或另一個 Cargo build 重疊。

| Gate | 最終結果 | 原始 log |
| --- | --- | --- |
| targeted voice/server lib tests | server 34 + voice 26 passed，0 failed | `targeted-voice-webhook-rerun.log` |
| `cargo fmt --all` + `cargo fmt --all --check` | 兩者 exit 0 | `fmt-final.log` |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | exit 0 | `clippy-workspace-final.log` |
| `cargo test --workspace --locked --no-fail-fast` | **405 passed、0 failed、6 ignored**；其中 5 項是 FFmpeg、1 項是 cache benchmark | `test-workspace.log` |
| 五項 ignored FFmpeg integration tests | **5 passed、0 failed**；真實 `.tools/ffmpeg.exe`，涵蓋 failed input、queue/child、EQ、stereo profile、Vorbis/FLAC native path | `test-ffmpeg-ignored.log` |
| `cargo build --workspace --release --locked` | exit 0；`target/release/rustlink.exe` SHA-256 `AE207251A9D326115B7B17EC509450F9C9EFB938A03C41B58C941B9D245C3001` | `build-release-workspace.log` |
| shell syntax | Git Bash 與 MSYS2 Bash 各自對 `install.sh`、`start.sh` 執行 `-n`，4/4 exit 0 | `bash-syntax.log` |
| 安全 launch smoke | release binary、`start.ps1`、`start.bat`、Git Bash `start.sh`、MSYS2 Bash `start.sh` 均以 `--version` 回覆 `Rustlink 0.5.0 (Rustlink API 1)`；未啟動 server | `launch-version-smoke.log`、`launch-version-smoke-msys-rerun.log` |
| installer non-mutating smoke | 兩套 Bash 的 `install.sh --help` 皆 exit 0；未安裝套件／服務 | `install-help-smoke.log` |
| lockfile | 351 registry packages / 351 checksums；10 workspace members；`cargo metadata --locked` exit 0 | `cargo-lock-review.log` |
| ledger | `git ls-files` + untracked inventory 112、ledger 112、missing 0、extra 0 | `review-ledger-check.log` |
| whitespace/status | `git diff --check` exit 0（另有兩個既有 CRLF→LF Git 提示，非 whitespace error） | `git-final-check.log` |

中途 gate 亦原樣保留：第一次 targeted compile 因新增 webhook test 少匯入 `Mutex` 而
exit 101，補匯入後重跑全綠；第一次 Clippy 以 `chunks_exact_to_as_chunks` 擋下新
simulation，改用 `as_chunks::<2>()` 後重跑全綠。第一次 MSYS2 launch smoke 的呼叫端
PATH 未含 MSYS `/usr/bin`，因找不到 `dirname` 而 exit 1；補上明確 MSYS path 後同一
source script 重跑 exit 0。這三項都不是被忽略的失敗，對應 first/final logs 均保留。

未執行 cache microbenchmark，未做效能宣稱；未用真實 Discord／Spotify 憑證，亦未
向實際 webhook 投遞資料。
