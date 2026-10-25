# Configuration

A node reads `config.yaml`, `config.yml` or `config.json` from the working
directory, or the file named by `--config` / `-c`. Field names are matched strictly: a
misspelled option stops startup and names the field, because a typo that is
silently ignored is harder to find than a startup error.

A node that finds no file, and no password in the environment, writes
`config.yaml` with a generated password rather than refusing to start, which is
what makes the binary runnable by double-clicking it.

Environment variables are applied on top of the file, so a container can be
configured without mounting one.

## Node

| Option | Default | What it does |
| --- | --- | --- |
| `address` | `0.0.0.0:2333` | Where the node listens. Every interface by default, which is what a bot on another machine needs; `127.0.0.1:2333` is for a node behind a proxy that terminates TLS. |
| `password` | *(empty)* | The password every client sends as `Authorization: Bearer <password>`. Startup fails without one. |

## Media tools

| Option | Default | What it does |
| --- | --- | --- |
| `ffmpeg` | `ffmpeg` | The FFmpeg executable: a bare name, a path, or `none`. A bare name means a file inside `toolsDirectory` and is never looked up on `PATH`, so every node runs the build it was given. `none` switches FFmpeg off entirely. |
| `toolsDirectory` | `.tools` | Where downloaded helpers live. |
| `autoDownload` | `false` | Install a helper that is neither in `toolsDirectory` nor named by an explicit path. |
| `inProcessAudio` | `true` | Decode, filter and encode in process. Turning it off sends everything through FFmpeg, which only costs performance. |
| `lyricsSource` | `lrclib` | `lrclib` asks the database and then YouTube Music for timed lines; `none` switches lyrics off and stops the node calling anything. |

FFmpeg is only needed for the containers the in-process pipeline declines, so a
node without it still plays the supported formats. The startup line reports
whether FFmpeg is available.

## Logging

| Option | Default | What it does |
| --- | --- | --- |
| `logLevel` | `info` | `debug`, `info`, `warn` or `error`. |
| `logFormat` | `text` | `text` for a person, `json` for a collector. The wordmark is left out of a JSON stream. |
| `logColor` | `auto` | `auto` colours a terminal and leaves a redirected stream alone, `always` and `never` override that. `NO_COLOR` in the environment is honoured. |
| `logDirectory` | `logs` | Daily files named `rustlink-YYYY-MM-DD.log`, reopened when the date changes. Empty keeps the console as the only output. Records in a file are never coloured. |
| `logRetentionDays` | `14` | How many days of those files are kept: anything older is deleted at startup and when the date changes, so a node left running does not fill the disk. `0` keeps every file, for a node whose logs are rotated and collected elsewhere. Only files this node writes are touched. |

## Sources

| Option | Default | What it does |
| --- | --- | --- |
| `youtubeNative` | `true` | Resolve YouTube with the native InnerTube clients. |
| `youtubeEndpoint` | `https://youtubei.googleapis.com` | The InnerTube endpoint. |
| `youtubeMusicEndpoint` | `https://music.youtube.com` | The endpoint the YouTube Music client and the lyrics catalogue use. |
| `youtubeClientVersion` | *(empty)* | Override the Android client version and its matching User-Agent. Empty currently uses `20.10.38`; Android VR and iOS retain their own versions. |
| `youtubePoToken` | *(empty)* | A proof-of-origin token, if an operator has one. |
| `youtubeVisitorData` | *(empty)* | Visitor data to send with it. |
| `soundcloudNative` | `true` | Resolve SoundCloud with the public API. |
| `soundcloudApiEndpoint` | `https://api-v2.soundcloud.com` | The API endpoint. |
| `soundcloudWebEndpoint` | `https://soundcloud.com` | The site a client id is scraped from. |
| `soundcloudClientId` | *(empty)* | A client id, or empty to scrape and cache one. |
| `soundcloudClientIdTtlSeconds` | `21600` | How long a scraped client id is kept. |
| `spotifyClientId` | *(empty)* | A Spotify application client id. Without it, `spsearch:` is refused rather than silently answered with something else. |
| `spotifyClientSecret` | *(empty)* | The matching secret. |
| `spotifyRefreshToken` | *(empty)* | A user refresh token, which is what makes a listener's own playlists resolvable. |
| `spotifyMarket` | `US` | The market country code sent with catalogue requests. |
| `deezerApiEndpoint` | `https://api.deezer.com` | The Deezer API endpoint. |
| `allowPrivateNetworks` | `false` | Allow a source on a private or special-use address. Off keeps a client from pointing the node at the network it runs in. |

## Filters

`filters` maps a filter name to whether clients may use it. A name that is
absent stays enabled; listing
one as `false` makes a player update that asks for it fail with
`Following filters are disabled in the config: ...`. The names are `volume`,
`equalizer`, `karaoke`, `timescale`, `tremolo`, `vibrato`, `rotation`,
`distortion`, `channelMix` and `lowPass`.

## Capacity

| Option | Default | What it does |
| --- | --- | --- |
| `maxPlayers` | `100` | Players across the whole node. |
| `maxSessions` | `100` | Sessions across the whole node. |
| `maxConcurrentLoads` | `8` | Loads resolved at once; waiting for capacity consumes the same `loadTimeoutSeconds` budget as the provider requests. |
| `maxPlaylistTracks` | `100` | Tracks taken from one playlist or album. |
| `searchResults` | `5` | Results in one search answer. |
| `cacheEntries` | `1024` | Resolved loads kept. |
| `cacheTtlSeconds` | `300` | How long one is reused. |
| `loadTimeoutSeconds` | `30` | How long a load may take before it is given up on. |
| `playerUpdateSeconds` | `5` | How often a session reports the position of its players. |
| `statsSeconds` | `60` | How often the machine is sampled for `/api/v1/stats`. |
| `eventBuffer` | `256` | Events that may wait for a socket. A client that stops reading fills it and is then closed, because a node that drops events silently leaves a client showing something that is not happening. |
| `opusBitrate` | `128000` | Opus bitrate in bits per second. |
| `opusComplexity` | `10` | How hard the Opus encoder works, on its own scale of zero to ten. Lowering it spends less CPU on every frame of every track, which is the knob for a node that carries more players than it has cores; the in-process pipeline and the FFmpeg fallback both follow it. |

## Rate limits

`ratelimits.enable` turns per-route token buckets on or off. Each route has its
own bucket, so a client that hammers one route does not spend the allowance of
another. The limiter runs before authentication, so a throttled request is
refused before the password check and a rejected one still spends a token.

## Webhook

| Option | Default | What it does |
| --- | --- | --- |
| `webhook.enable` | `false` | Post node events to an endpoint. |
| `webhook.url` | *(empty)* | A Discord webhook URL receives an embed; any other endpoint receives the event itself as JSON. |
| `webhook.stateLatencySeconds` | `5` | How long the node waits between whole-node player state reports. Zero leaves the endpoint carrying only events. |

Delivery runs on one background thread, so a slow endpoint never blocks the
player that produced the event. A delivery that cannot be completed stops the
node: a configured endpoint is part of the deployment, not an optional extra.

## Environment variables

Every option has one, named `RUSTLINK_` followed by the option in
`SCREAMING_SNAKE_CASE`: `RUSTLINK_ADDRESS`, `RUSTLINK_PASSWORD`,
`RUSTLINK_FFMPEG`, `RUSTLINK_TOOLS_DIRECTORY`, `RUSTLINK_AUTO_DOWNLOAD`,
`RUSTLINK_IN_PROCESS_AUDIO`, `RUSTLINK_LYRICS_SOURCE`, `RUSTLINK_LOG_LEVEL`,
`RUSTLINK_LOG_FORMAT`, `RUSTLINK_LOG_COLOR`, `RUSTLINK_LOG_DIRECTORY`,
`RUSTLINK_LOG_RETENTION_DAYS`,
`RUSTLINK_YOUTUBE_NATIVE`,
`RUSTLINK_YOUTUBE_ENDPOINT`, `RUSTLINK_YOUTUBE_MUSIC_ENDPOINT`,
`RUSTLINK_YOUTUBE_CLIENT_VERSION`, `RUSTLINK_YOUTUBE_PO_TOKEN`,
`RUSTLINK_YOUTUBE_VISITOR_DATA`, `RUSTLINK_SOUNDCLOUD`,
`RUSTLINK_SOUNDCLOUD_API_ENDPOINT`, `RUSTLINK_SOUNDCLOUD_WEB_ENDPOINT`,
`RUSTLINK_SOUNDCLOUD_CLIENT_ID`, `RUSTLINK_DEEZER_API_ENDPOINT`,
`RUSTLINK_PRIVATE_NETWORKS`, `RUSTLINK_MAX_PLAYERS`, `RUSTLINK_MAX_SESSIONS`,
`RUSTLINK_MAX_CONCURRENT_LOADS`, `RUSTLINK_MAX_PLAYLIST_TRACKS`,
`RUSTLINK_SEARCH_RESULTS`, `RUSTLINK_CACHE_ENTRIES`, `RUSTLINK_CACHE_TTL_SECONDS`,
`RUSTLINK_LOAD_TIMEOUT_SECONDS`, `RUSTLINK_PLAYER_UPDATE_SECONDS`,
`RUSTLINK_STATS_SECONDS`, `RUSTLINK_EVENT_BUFFER`, `RUSTLINK_OPUS_BITRATE`,
`RUSTLINK_OPUS_COMPLEXITY`, `RUSTLINK_RATELIMITS_ENABLE`,
`RUSTLINK_WEBHOOK_ENABLED`, `RUSTLINK_WEBHOOK_URL`
and `RUSTLINK_WEBHOOK_STATE_LATENCY_SECONDS`.

Spotify settings also accept `SPOTIFY_CLIENT_ID`, `SPOTIFY_CLIENT_SECRET`,
`SPOTIFY_REFRESH_TOKEN` and `SPOTIFY_MARKET`. These override the corresponding
configuration fields, just like the `RUSTLINK_*` variables above.
