//! HLS media playlists read as one continuous byte stream (SoundCloud now serves only HLS).

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read};
use std::time::Duration;

use crate::media::{MediaBody, MediaRange};
use crate::{catalog, netguard, SourceError};

const SEGMENT_ATTEMPTS: usize = 3;

/// The init section (if any) and media segment URLs of a media playlist, in play order.
pub(crate) fn segments(playlist_url: &str, playlist: &str) -> Result<Vec<String>, SourceError> {
    let base = url::Url::parse(playlist_url).map_err(|e| SourceError::Url(e.to_string()))?;
    let resolve = |uri: &str| -> Result<String, SourceError> {
        let url = base.join(uri.trim()).map_err(|e| SourceError::Url(e.to_string()))?;
        netguard::validate_url(url.as_str())?;
        Ok(url.into())
    };
    if !playlist.trim_start().starts_with("#EXTM3U") {
        return Err(SourceError::Http("the HLS playlist is not an M3U8 file".into()));
    }
    let mut out = Vec::new();
    for line in playlist.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if line.starts_with("#EXT-X-STREAM-INF") {
            return Err(SourceError::Http("multivariant HLS playlists are not supported".into()));
        }
        if let Some(key) = line.strip_prefix("#EXT-X-KEY:") {
            if !key.contains("METHOD=NONE") {
                return Err(SourceError::Http("encrypted HLS streams cannot be played".into()));
            }
        } else if let Some(map) = line.strip_prefix("#EXT-X-MAP:") {
            let uri = map
                .split_once("URI=\"")
                .and_then(|(_, rest)| rest.split_once('"'))
                .map(|(uri, _)| uri)
                .ok_or_else(|| SourceError::Http("an HLS init section has no URI".into()))?;
            out.push(resolve(uri)?);
        } else if !line.starts_with('#') {
            out.push(resolve(line)?);
        }
    }
    if out.is_empty() {
        return Err(SourceError::Http("the HLS playlist names no segments".into()));
    }
    Ok(out)
}

/// Segments fetched one after another, so the decoder sees a single file.
// shortcut: segment URLs come from one playlist fetch and may expire on multi-hour mixes;
// re-fetch the playlist on 403 if that ever shows up.
pub(crate) struct Body {
    agent: ureq::Agent,
    pending: VecDeque<String>,
    current: Option<MediaBody>,
}

impl Body {
    pub(crate) fn open(agent: &ureq::Agent, playlist_url: &str) -> Result<Self, SourceError> {
        let playlist = catalog::get_text(agent, playlist_url)?;
        let pending = segments(playlist_url, &playlist)?.into();
        let mut body = Body { agent: agent.clone(), pending, current: None };
        body.advance()?;
        Ok(body)
    }

    /// Opens the next segment; `false` once the playlist is used up.
    fn advance(&mut self) -> Result<bool, SourceError> {
        self.current = None;
        let Some(url) = self.pending.pop_front() else { return Ok(false) };
        let mut last = None;
        for attempt in 0..SEGMENT_ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(300));
            }
            let range = MediaRange::OpenEnded;
            match MediaBody::open(self.agent.clone(), url.as_str(), BTreeMap::new(), range) {
                Ok(segment) => {
                    self.current = Some(segment);
                    return Ok(true);
                }
                Err(e) if e.retryable() => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| SourceError::Http("an HLS segment could not be opened".into())))
    }
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if let Some(segment) = &mut self.current {
                let n = segment.read(buf)?;
                if n > 0 {
                    return Ok(n);
                }
            }
            if !self.advance().map_err(io::Error::other)? {
                return Ok(0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_media_playlist_lists_its_init_section_then_its_segments() {
        let playlist = "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:10.0,\n\
                        seg/0.m4s?sig=a\n#EXTINF:9.5,\nhttps://cdn.example/1.m4s\n#EXT-X-ENDLIST\n";
        assert_eq!(
            segments("https://hls.example/media/playlist.m3u8?x=1", playlist).unwrap(),
            [
                "https://hls.example/media/init.mp4",
                "https://hls.example/media/seg/0.m4s?sig=a",
                "https://cdn.example/1.m4s",
            ]
        );
    }

    #[test]
    fn playlists_that_cannot_be_played_as_one_file_are_refused() {
        let url = "https://hls.example/p.m3u8";
        for bad in [
            "not a playlist",
            "#EXTM3U\n#EXT-X-ENDLIST\n",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nlow.m3u8\n",
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n#EXTINF:1,\na.ts\n",
            "#EXTM3U\n#EXTINF:1,\nhttps://user:pass@cdn.example/a.ts\n",
        ] {
            assert!(segments(url, bad).is_err(), "{bad}");
        }
        assert!(segments(url, "#EXTM3U\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:1,\na.ts\n").is_ok());
    }
}
