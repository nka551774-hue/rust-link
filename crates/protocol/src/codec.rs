//! Rustlink tokens: `rlt1.` followed by URL-safe, unpadded UTF-8 JSON.
//! Tokens carry metadata, not credentials or cached media URLs. Sources still
//! validate and resolve that metadata before opening a stream.

use crate::types::{Track, TrackInfo};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

pub const MAX_ENCODED_TRACK: usize = 256 << 10;
pub const VERSION: u8 = 1;
const PREFIX: &str = "rlt1.";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    #[error("invalid Rustlink track token")]
    Invalid,
    #[error("track token exceeds 256 KiB")]
    TextTooLong,
    #[error("unsupported track token version; expected rlt1")]
    UnsupportedVersion,
}

fn validate(info: &TrackInfo) -> Result<(), CodecError> {
    if info.identifier.trim().is_empty()
        || info.source_name.trim().is_empty()
        || info.length < 0
        || info.position < 0
    {
        return Err(CodecError::Invalid);
    }
    Ok(())
}

pub fn encode_track(info: &TrackInfo) -> Result<Track, CodecError> {
    validate(info)?;
    let json = serde_json::to_vec(info).map_err(|_| CodecError::Invalid)?;
    if json.len() > (MAX_ENCODED_TRACK - PREFIX.len()) / 4 * 3 {
        return Err(CodecError::TextTooLong);
    }
    Ok(Track::new(format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(json)), info.clone()))
}

pub fn decode_track(token: &str) -> Result<Track, CodecError> {
    if token.len() > MAX_ENCODED_TRACK {
        return Err(CodecError::TextTooLong);
    }
    let payload = token.strip_prefix(PREFIX).ok_or(CodecError::UnsupportedVersion)?;
    let json = URL_SAFE_NO_PAD.decode(payload).map_err(|_| CodecError::Invalid)?;
    let info: TrackInfo = serde_json::from_slice(&json).map_err(|_| CodecError::Invalid)?;
    validate(&info)?;
    Ok(Track::new(token.to_owned(), info))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> TrackInfo {
        TrackInfo {
            identifier: "track-123".into(),
            source_name: "youtube".into(),
            title: "音樂 🎶\0 UTF-8".into(),
            author: "演奏者".into(),
            length: 123_456,
            position: 123,
            is_seekable: true,
            uri: Some("https://www.youtube.com/watch?v=track-123".into()),
            ..Default::default()
        }
    }
    #[test]
    fn unicode_metadata_roundtrips() {
        let info = sample();
        let track = encode_track(&info).unwrap();
        assert!(track.token.starts_with(PREFIX));
        assert_eq!(decode_track(&track.token).unwrap(), track);
        assert_eq!(track.metadata, info);
    }
    #[test]
    fn rejects_legacy_unknown_malformed_and_oversized_tokens() {
        for token in ["QAAAgwMABHNvbmc=", "rlt2.e30", "rlt1.!", "rlt1.e30"] {
            assert!(decode_track(token).is_err(), "{token}");
        }
        assert_eq!(decode_track(&"a".repeat(MAX_ENCODED_TRACK + 1)), Err(CodecError::TextTooLong));
        let mut info = sample();
        info.title = "a".repeat(MAX_ENCODED_TRACK);
        assert_eq!(encode_track(&info), Err(CodecError::TextTooLong));
    }
    #[test]
    fn rejects_invalid_metadata_on_both_paths() {
        let mut info = sample();
        info.length = -1;
        assert!(encode_track(&info).is_err());
        let token =
            format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(serde_json::to_vec(&info).unwrap()));
        assert!(decode_track(&token).is_err());
    }
}
