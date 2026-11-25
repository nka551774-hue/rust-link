//! Rustlink tokens: `rlt1.` followed by URL-safe, unpadded UTF-8 JSON.
//! Tokens carry metadata, not credentials or cached media URLs. Sources still
//! validate and resolve that metadata before opening a stream.

use std::cell::RefCell;

use crate::types::{Track, TrackInfo};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

pub const MAX_ENCODED_TRACK: usize = 256 << 10;
pub const VERSION: u8 = 1;
const PREFIX: &str = "rlt1.";
const MAX_JSON_BYTES: usize = (MAX_ENCODED_TRACK - PREFIX.len()) * 3 / 4;
const MAX_SCRATCH_CAP: usize = 64 << 10;

thread_local! {
    static CODEC_SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

fn with_scratch<R>(f: impl FnOnce(&mut Vec<u8>) -> R) -> R {
    CODEC_SCRATCH.with(|cell| match cell.try_borrow_mut() {
        Ok(mut buf) => {
            buf.clear();
            let result = f(&mut buf);
            if buf.capacity() > MAX_SCRATCH_CAP {
                *buf = Vec::new();
            }
            result
        }
        Err(_) => f(&mut Vec::new()),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    #[error("invalid Rustlink track token")]
    Invalid,
    #[error("track token exceeds 256 KiB")]
    TextTooLong,
    #[error("unsupported track token version; expected rlt1")]
    UnsupportedVersion,
}

#[inline]
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

fn encode_token(info: &TrackInfo) -> Result<String, CodecError> {
    validate(info)?;
    with_scratch(|json| {
        serde_json::to_writer(&mut *json, info).map_err(|_| CodecError::Invalid)?;
        if json.len() > MAX_JSON_BYTES {
            return Err(CodecError::TextTooLong);
        }
        let mut token = String::with_capacity(PREFIX.len() + (json.len() * 4).div_ceil(3));
        token.push_str(PREFIX);
        URL_SAFE_NO_PAD.encode_string(json.as_slice(), &mut token);
        Ok(token)
    })
}

pub fn encode_track(info: &TrackInfo) -> Result<Track, CodecError> {
    let token = encode_token(info)?;
    Ok(Track::new(token, info.clone()))
}

pub fn encode_track_owned(info: TrackInfo) -> Result<Track, CodecError> {
    let token = encode_token(&info)?;
    Ok(Track::new(token, info))
}

fn decode_info(token: &str) -> Result<TrackInfo, CodecError> {
    if token.len() > MAX_ENCODED_TRACK {
        return Err(CodecError::TextTooLong);
    }
    let Some(payload) = token.strip_prefix(PREFIX) else {
        return if token.is_empty() || token.bytes().any(|b| b.is_ascii_whitespace()) {
            Err(CodecError::Invalid)
        } else {
            Err(CodecError::UnsupportedVersion)
        };
    };
    with_scratch(|json| {
        URL_SAFE_NO_PAD.decode_vec(payload, json).map_err(|_| CodecError::Invalid)?;
        let info: TrackInfo = serde_json::from_slice(json).map_err(|_| CodecError::Invalid)?;
        validate(&info)?;
        Ok(info)
    })
}

pub fn decode_track(token: &str) -> Result<Track, CodecError> {
    let info = decode_info(token)?;
    Ok(Track::new(token.to_owned(), info))
}

pub fn decode_track_owned(token: String) -> Result<Track, CodecError> {
    let info = decode_info(&token)?;
    Ok(Track::new(token, info))
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
        let owned = encode_track_owned(info.clone()).unwrap();
        assert_eq!(track, owned);
        assert!(track.token.starts_with(PREFIX));
        assert_eq!(decode_track(&track.token).unwrap(), track);
        assert_eq!(track.metadata, info);
        assert_eq!(decode_track(&format!(" {}", track.token)), Err(CodecError::Invalid));
        assert_eq!(decode_track(&format!("{}\n", track.token)), Err(CodecError::Invalid));
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
