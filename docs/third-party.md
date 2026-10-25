# Third-party services

This node is a client of services it does not own. Nothing here is affiliated
with, endorsed by or supported by any of them, and an operator is the one who
answers to their terms.

## Discord

The node speaks the voice gateway with the credentials a bot already has. It
does not connect to the Discord API otherwise: the bot that owns the session
sends the voice state, and the node acts on it. Nothing is stored beyond the
token for as long as the connection lives.

## YouTube

Track metadata and audio are read from YouTube through its internal API using
Rust clients. This interface can change without notice. An operator who runs
a node at scale is the one who has to consider what YouTube's terms say about it.

Visitor data and proof-of-origin tokens, when supplied in the configuration,
are sent to the configured YouTube endpoints. Rustlink does not generate them.

## SoundCloud

Metadata and audio come from the public API. A client id that the configuration
does not supply is scraped from the site and cached, which is what a browser
does; a supplied one is used as it is.

## Spotify

Only metadata is read, through the web API with an application client id and,
optionally, a user refresh token. Audio is not fetched from Spotify, because it
is not fetchable. Each entry is matched to a video on another service by
searching for its title and artist, so what a listener hears is that service's
copy. A node without a client id refuses Spotify identifiers instead of guessing.

## Deezer

Metadata comes from the public API. Playback uses a YouTube match selected by
title, artist and duration; Deezer audio and preview streams are not fetched.

## LRCLIB

Lyrics are read from `lrclib.net`, a free and public database. The node sends the
track title, artist, album and duration, and caches what comes back for as long
as the entry lives.

## YouTube Music

When the database has nothing for a track, timed lyrics are read from YouTube
Music through the same InnerTube client the source layer uses, so the catalogue
request carries the same visitor data.

## FFmpeg

FFmpeg is a separate program with its own licences, installed by the operator
or by `autoDownload`. The builds this node installs are GPL builds with libopus,
because the encoder needs it; the archive also holds `ffprobe`, which this node
does not use. Its licences and terms are those of its upstream distributors.

## What the node sends

Nothing about a node or its listeners leaves it except the requests above. There
is no telemetry, no update check and no central service: a webhook is only
called when the operator configures one, and its URL is the only place events
go.
