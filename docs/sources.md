# Sources

Pass the identifier in the `query` field of `POST /api/v1/tracks/resolve`.

An identifier is a search query, a video id, a catalogue link or a plain URL.
Which one it is decides which service the node talks to, and the choice is made
before any network call.

## Routing

A prefix is checked first, because a search query can contain anything,
including something that looks like a URL.

| Identifier | Source |
| --- | --- |
| `ytsearch:<query>`, `ytmsearch:<query>` | YouTube, and YouTube Music for the second |
| 11 characters of `A-Za-z0-9_-` | a YouTube video id |
| `scsearch:<query>` | SoundCloud search |
| `spsearch:<query>`, `spotify:<...>` | Spotify |
| `dzsearch:<query>`, `dzisrc:<isrc>`, `deezer:<...>` | Deezer |
| `https://youtube.com/...`, `https://youtu.be/...` | YouTube |
| `https://soundcloud.com/...` | SoundCloud |
| `https://open.spotify.com/...` | Spotify |
| `https://deezer.com/...` | Deezer |
| any other `http` or `https` URL | fetched directly |
| anything else | `empty`, because no source can be asked about it |

The host match accepts a subdomain, so `music.youtube.com` is YouTube.
YouTube links may include share parameters and fragments. A valid video ID
takes priority over an attached `list` parameter, so sharing a song from a
playlist loads that song. Use a playlist URL without a video ID or
`youtube:playlist:<id>` to load the playlist. See the
[Discord YouTube guide](youtube.md) for supported link shapes and bot wiring.

## What each source does

**YouTube.** Audio URL resolution tries Android, Android VR and iOS InnerTube
clients in turn. Metadata resolution also tries the next client after a
refusal. During playback, a retryable CDN rejection tries the next native
client before resolving again, within the same request deadline.
YouTube Music search uses the web music client. A
search returns up to `searchResults` tracks, a playlist is followed for up to
`maxPlaylistTracks` tracks over at most six pages, and a video is one track with
its audio stream resolved through the player response. Resolution runs in Rust;
if every native client fails, the source error is returned to the API client.
Disabling `youtubeNative` disables YouTube resolution and playback.

Googlevideo audio is fetched in 128 KiB byte ranges. Each continuation starts
at the next unread byte and validates the returned offset and total size;
normal chunk boundaries neither sleep nor consume the failure retry budget.
This avoids CDN rejections of open-ended or larger range requests. Other HTTP
hosts retain their normal streaming requests. A bounded response without a
known total size is rejected rather than silently truncating playback. This
also applies to partial responses from other hosts; full 200 responses with
an unknown length remain supported for live streams.

Every partial body is checked against its declared byte span, including
chunked transfers. A strong ETag is retained for `If-Range` and a conflicting
ETag ends playback before replacement bytes reach the decoder. Without a
strong ETag, offset and length validation cannot detect every same-size
replacement. Range bodies must use identity encoding. HTTP clients request
identity encoding and do not transparently decompress media responses; see
the [Rust architecture](architecture.md).

Playback requires a direct audio URL from a supported native client. Formats
available only through `signatureCipher` / `cipher` or a proprietary streaming
protocol are not deciphered or reconstructed. Regional, account or provider
restrictions can also reject a request; the API returns the source error.
Native resolution does not guarantee playback of every YouTube video.

**PO tokens.** YouTube asks for a proof-of-origin token for some clients.
`youtubePoToken` supplies a token in the native player request;
`youtubeVisitorData` supplies its visitor data. Rustlink does not generate or
refresh these credentials automatically. A client that demands an unavailable
or expired token may reject playback.

**SoundCloud.** The public API with a client id. An empty `soundcloudClientId`
means the id is scraped from the site and cached for
`soundcloudClientIdTtlSeconds`. Playback uses a progressive stream reported by
the API. Tracks available only as HLS return a source error because the native
pipeline does not assemble HLS segments. Disabling `soundcloudNative` disables
this source.

**Spotify.** Metadata only. A track, album or playlist is resolved
through the web API. Playback searches YouTube and checks title, artist and
duration for a close match. Spotify identifiers require configured client
credentials, and playback also requires `youtubeNative`.

Paste an `open.spotify.com/track/...`, `/album/...` or `/playlist/...` link
directly into the same `query` field used for YouTube. Locale prefixes such as
`/intl-zh-TW/`, `/embed/` links, share parameters, fragments and Discord's
`<...>` link wrapper are supported. Spotify URIs such as
`spotify:track:4cOdK2wGLETKBW3PvgPWqT` also work. Resource IDs are validated
before any request; podcasts, local files and Spotify short redirect links
are not treated as songs.

Set `spotifyClientId` and `spotifyClientSecret` in the node configuration
(or `SPOTIFY_CLIENT_ID` / `SPOTIFY_CLIENT_SECRET`). A private
or editorial playlist can additionally require `spotifyRefreshToken` with
appropriate account permissions. API pagination stays on the Spotify API
origin so a response cannot forward the access token to another host.

Playback does not use Spotify's 30-second preview URL or decrypt Spotify's
audio. It searches YouTube for a full-length recording using title, artist
and duration. Single-character song/artist names are supported. Candidates
whose titles contain recognized labels for unrequested covers, live versions,
remixes, karaoke, speed edits or previews are excluded. If no candidate meets
the matching thresholds, resolution returns an error. Matching is based on
metadata and cannot guarantee the identity of the recording.

### Audio quality and advertisements

YouTube selection prefers an original, non-auto-dubbed audio track and preserves
the original dynamics instead of choosing a dynamically compressed variant
solely for a larger bitrate. Among equivalent variants, the highest reported
audio bitrate wins. Output quality is controlled by `opusBitrate` and
`opusComplexity`; see [configuration](configuration.md). Transcoding cannot
recover detail missing from the source, and Discord delivery depends on the
voice channel's capabilities and network.

Rustlink never schedules pre-roll or mid-roll advertisements. It opens the
selected recording's audio URL, skips YouTube's promoted-result subtrees, and
does not fetch player advertisement placements. Advertisements or sponsorships
already recorded inside the selected video remain part of that recording;
Rustlink does not detect and edit them out.

**Deezer.** Metadata is resolved through the public API, including ISRC lookup
with `dzisrc:`. Playback uses the same YouTube matching path as Spotify;
Rustlink does not fetch Deezer audio or preview streams.

**Plain HTTP.** The URL is fetched and its media bytes are probed. A recognised
audio stream can be played directly. HTML pages and playlist manifests such as
M3U, PLS and XSPF are not expanded into media URLs; a response without a
supported audio container returns a source error.

Other websites have no generic extractor. A plain URL must expose media that
the HTTP resolver and audio pipeline can read.

## Caching and limits

A resolved load is cached for `cacheTtlSeconds`, up to `cacheEntries` entries
and a 32 MiB accounting budget, least recently used first. The cache retains
typed shared results, so hits do not parse JSON or copy every track. An empty
answer is cached like any other; errors are not retained after the current
resolution finishes, so a later request can retry.

Clients asking for the same identifier at the same time share one resolution
and its result, including errors and results too large to cache. Errors are
shared only with current waiters, so a later request can retry.

At most `maxConcurrentLoads` loads run at once. Capacity waits, waits for an
existing resolution and provider requests share the `loadTimeoutSeconds`
budget; retries and catalogue pages do not receive a fresh timeout each time.

## Private networks

`allowPrivateNetworks` is off by default. The check runs in the resolver, which
is the last place before a connection is made: a name that resolved to a private
or special-use address since the URL was accepted is still caught. This is what
keeps a client from pointing the node at the network it runs in.

## Media helpers

`ffmpeg` names the optional codec helper. A bare name means a file inside
`toolsDirectory` and is never looked up on `PATH`, so a node always runs the
build it was given rather than whatever the machine happens to have installed.
An explicit path is a deliberate choice and is never replaced by an install.

With `autoDownload` on, a missing helper is installed into `toolsDirectory`:

| Helper | Where it comes from |
| --- | --- |
| FFmpeg on Windows | the BtbN GPL archive for this architecture, verified against its `checksums.sha256` |
| FFmpeg on Linux and macOS | martin-riedl.de, the release build first and the snapshot second |

A body shorter than 1 MiB is treated as an error page or a truncated transfer
rather than a helper, and is discarded.
