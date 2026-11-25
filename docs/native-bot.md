# 原生 Rust Discord 機器人

`rustlink-discord-bot` 在純 Rust 內實作 Discord Gateway v10、REST v10 斜線指令與語音事件配對，將 `VoiceState` 交由 Rustlink 節點播放，完全不依賴外部腳本或直譯器。

## 核心元件與範例

| 元件 / 範例 | 說明 |
| --- | --- |
| `Gateway` | 主 Gateway v10：`connect`、`connect_with_messages`（遇 `4014` 免特權自動降級）、`resume`（`OP 6`）、`reconnect`（有界退避與續傳／重登切換）與 `OP 4` 語音狀態 |
| `DiscordRest` | REST v10：發送訊息、註冊伺服器斜線指令、互動延遲回覆與編輯（內建 429 `retry_after` 退避與 Token 遮蔽） |
| `VoiceStatePairer` | 依伺服器配對機器人自身的 `VOICE_STATE_UPDATE` 與 `VOICE_SERVER_UPDATE`（支援亂序、區域遷移與重新分配） |
| `normalize_query` / `extract_message_query` / `extract_url_timestamp_ms` | 擷取分享文字與裸連結、轉換關鍵字為 `ytsearch:`，並解析 YouTube `?t=`／`#t=` 起播毫秒 |
| `examples/play.rs` | 最小登入並加入指定頻道播放單一查詢 |
| `examples/slash.rs` | 單檔 `/play`、`@Bot <查詢>` 與頻道裸連結自動播放機器人 |
| `examples/music/` | 生產級多伺服器佇列音樂機器人（8 個斜線指令 + `.env` 組態 + `SIGINT`/`SIGTERM` 優雅退場） |

## 部署與啟動 `music` 機器人

除了在開發環境直接執行外，專案支援三種一鍵同時部署節點與 `music` 機器人的方式：

```sh
# 1. 本機一鍵啟動（自動安裝缺少的工具鏈並編譯節點＋機器人）
./quickstart/start.sh                                         # Linux / macOS（Windows 雙擊 quickstart\start.bat）

# 2. Linux VPS systemd 雙服務一鍵安裝（rustlink + rustlink-bot）
sudo DISCORD_BOT_TOKEN=... ./install.sh --with-bot

# 3. Docker Compose 一鍵啟動節點＋機器人
RUSTLINK_PASSWORD=... DISCORD_BOT_TOKEN=... docker compose --profile bot up -d --build

# 4. 單獨以 Cargo 啟動機器人（讀取 .env 或指定路徑，環境變數優先）
cp .env.example .env
cargo run -p rustlink-discord-bot --example music
```

### 設定變數

| 變數 | 預設 | 說明 |
| --- | --- | --- |
| `DISCORD_BOT_TOKEN` | 必填 | Discord Bot Token；若已開啟 Message Content Intent 支援頻道裸連結自動播放，未開啟（`4014`）自動退回 `/play` 與 `@Bot` 模式 |
| `RUSTLINK_PASSWORD` | 必填 | 對應節點 `config.yaml` 的 `password` |
| `RUSTLINK_URL` | `http://127.0.0.1:2333` | Rustlink 節點位址 |
| `DISCORD_GUILD_IDS` | 空 | 僅在指定伺服器（逗號分隔）註冊指令；留空代表所有伺服器 |
| `BOT_DEFAULT_VOLUME` | `100` | 初始音量 `0..=200` |
| `BOT_MAX_QUEUE` | `100` | 單一伺服器待播佇列上限 `1..=1000` |
| `BOT_IDLE_LEAVE_SECONDS` | `120` | 佇列播畢閒置幾秒後自動離開語音頻道（`0` 為不自動離開） |

### 斜線指令與行為細節

| 指令 | 行為 |
| --- | --- |
| `/play query` | 支援網址、影片 ID 或搜尋關鍵字（播放清單整批入列，單曲 YouTube `?t=` 連結自動定位起播） |
| `/skip`、`/pause`、`/resume`、`/stop` | 控制播放（要求使用者與機器人位於同一語音頻道；`/stop` 清空佇列並離開頻道） |
| `/queue`、`/nowplaying` | 顯示目前曲目、進度與後續待播清單 |
| `/volume level` | 調整音量 `0..=200`（要求同一語音頻道） |

- **精準推進佇列**：每首曲目附帶遞增 `play_id`，僅在收到相符 `play_id` 且原因為 `finished` 或 `load_failed` 的 `track.ended` 時推進下一首；連續 3 首載入失敗自動停止佇列防止洗版。
- **語音狀態自癒**：啟動時自動清除上次殘留的語音頻道狀態；重連退回 `IDENTIFY` 收到新 `READY` 時自動為播放中的伺服器重送 `OP 4`；收到 `SIGINT` 或 `SIGTERM`（systemd / Docker 停止）時先停播離開語音頻道再銷毀工作階段。
- **憑證隔離**：Bot Token 以 `Zeroizing` 保存且僅用於主 Gateway 與 Discord REST；傳給節點 `set_voice` 的僅限單一伺服器的短效 `voice.token`。
