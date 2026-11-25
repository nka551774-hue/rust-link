//! Audio engines (`PureFrames` in-process and `FfmpegFrames` fallback) and peekable `Body`.

use std::io::{self, Read};

use rustlink_protocol::{Object, Track};

use crate::error::AudioError;
use crate::ffmpeg::FfmpegFrames;
use crate::filters::{self, FilterSet};
use crate::pure::PureFrames;

/// A track stream whose leading bytes can be inspected before playback consumes them.
pub struct Body {
    head: Vec<u8>,
    position: usize,
    eof: bool,
    inner: Box<dyn Read + Send>,
}

impl Body {
    pub fn new(inner: Box<dyn Read + Send>) -> Self {
        Self { head: Vec::new(), position: 0, eof: false, inner }
    }

    /// Reads up to `count` unconsumed bytes ahead without consuming them.
    pub fn peek(&mut self, count: usize) -> io::Result<()> {
        let target = self.position.saturating_add(count);
        while !self.eof && self.head.len() < target {
            let start = self.head.len();
            self.head.resize(target, 0);
            match self.inner.read(&mut self.head[start..]) {
                Ok(0) => {
                    self.head.truncate(start);
                    self.eof = true;
                }
                Ok(read) => self.head.truncate(start + read),
                Err(error) => {
                    self.head.truncate(start);
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
            }
        }
        Ok(())
    }

    /// The bytes read ahead and not yet consumed.
    pub fn head(&self) -> &[u8] {
        &self.head[self.position..]
    }
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let buffered = self.head();
        if !buffered.is_empty() {
            let count = buffered.len().min(buf.len());
            buf[..count].copy_from_slice(&buffered[..count]);
            self.position += count;
            if self.position == self.head.len() {
                self.head = Vec::new();
                self.position = 0;
            }
            return Ok(count);
        }
        if self.eof {
            return Ok(0);
        }
        let read = self.inner.read(buf)?;
        self.eof = read == 0;
        Ok(read)
    }
}

/// Stream of 20 ms Opus packets.
pub trait Frames: Send {
    /// Writes the next 20 ms Opus packet into `packet`; returns `false` at EOF.
    fn next_frame(&mut self, packet: &mut Vec<u8>) -> Result<bool, AudioError>;

    /// Whether [`Frames::set_filters`] can update non-timescale DSP in place.
    fn supports_live_filters(&self) -> bool {
        false
    }

    /// Updates volume and filters in place when supported; `false` requires a stream restart.
    fn set_filters(&mut self, _set: &FilterSet) -> bool {
        false
    }
}

/// Which engine to use for a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preference {
    Auto,
    Ffmpeg,
}

/// Detected media container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Mp3,
    Wav,
    Flac,
    OggOpus,
    OggVorbis,
    OggFlac,
    Adts,
    Mp4,
    Webm,
    Unknown,
}

/// Identifies the media container from leading stream bytes.
pub fn detect(head: &[u8]) -> Container {
    if head.len() < 4 {
        return Container::Unknown;
    }
    match head {
        [b'O', b'g', b'g', b'S', ..] => {
            match head.get(27 + usize::from(*head.get(26).unwrap_or(&0))..) {
                Some(p) if p.starts_with(b"OpusHead") => Container::OggOpus,
                Some(p) if p.starts_with(b"\x01vorbis") => Container::OggVorbis,
                Some(p) if p.starts_with(b"\x7fFLAC") => Container::OggFlac,
                _ => Container::Unknown,
            }
        }
        [b'f', b'L', b'a', b'C', ..] => Container::Flac,
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'A', b'V', b'E', ..] => Container::Wav,
        [b'I', b'D', b'3', ..] => Container::Mp3,
        [0xff, b1, ..] if b1 & 0xf6 == 0xf0 => Container::Adts,
        [0xff, b1, ..] if b1 & 0xe0 == 0xe0 && b1 & 0x06 != 0 => Container::Mp3,
        [_, _, _, _, b'f', b't', b'y', b'p', ..] => Container::Mp4,
        [0x1a, 0x45, 0xdf, 0xa3, ..] => Container::Webm,
        _ => Container::Unknown,
    }
}

/// Dispatcher configuring in-process and FFmpeg playback engines.
pub struct Engine {
    pub ffmpeg: Option<String>,
    pub in_process: bool,
    pub bitrate: i32,
    pub complexity: i32,
}

/// True when ADTS uses CRC protection, MPEG-2 framing, or a non-AAC-LC profile.
#[inline]
fn adts_needs_ffmpeg(head: &[u8]) -> bool {
    matches!(head, [0xff, b1, b2, ..] if *b1 != 0xf1 || ((*b2 >> 6) & 0x3) != 1)
}

impl Engine {
    pub fn start(
        &self,
        track: &Track,
        mut body: Body,
        position_ms: i64,
        volume: i64,
        filters: &Object,
        preference: Preference,
    ) -> Result<Box<dyn Frames>, AudioError> {
        let set = filters::parse(filters, volume)?;
        if !cfg!(feature = "native-opus")
            || preference == Preference::Ffmpeg
            || !self.in_process
            || !PureFrames::supports_timescale(&set)
        {
            return self.start_ffmpeg(body, position_ms, &set);
        }
        // Up to 282 bytes precede the first Ogg page's codec signature; 512 always covers it.
        body.peek(512)?;
        let container = detect(body.head());
        if container == Container::Unknown
            || (container == Container::Adts && adts_needs_ffmpeg(body.head()))
        {
            return self.start_ffmpeg(body, position_ms, &set);
        }
        PureFrames::start(track, body, position_ms, &set, self.bitrate, self.complexity)
            .map(|frames| Box::new(frames) as Box<dyn Frames>)
    }

    fn start_ffmpeg(
        &self,
        body: Body,
        position_ms: i64,
        set: &FilterSet,
    ) -> Result<Box<dyn Frames>, AudioError> {
        let program = self.ffmpeg.as_deref().ok_or(AudioError::FfmpegMissing)?;
        let frames =
            FfmpegFrames::start(program, body, position_ms, set, self.bitrate, self.complexity)?;
        Ok(Box::new(frames))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StrictEofReader {
        data: Vec<u8>,
        offset: usize,
        saw_eof: bool,
    }

    impl StrictEofReader {
        fn body(data: &[u8]) -> Body {
            Body::new(Box::new(Self { data: data.to_vec(), offset: 0, saw_eof: false }))
        }
    }

    impl Read for StrictEofReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            assert!(!self.saw_eof, "inner reader polled again after returning Ok(0)");
            if self.offset == self.data.len() {
                self.saw_eof = true;
                return Ok(0);
            }
            let n = buf.len().min(self.data.len() - self.offset);
            buf[..n].copy_from_slice(&self.data[self.offset..self.offset + n]);
            self.offset += n;
            Ok(n)
        }
    }

    #[test]
    fn body_does_not_read_past_eof_after_peek_or_drain() {
        let mut peeked = StrictEofReader::body(&[1, 2, 3, 4]);
        peeked.peek(512).unwrap();
        let mut out = Vec::new();
        peeked.read_to_end(&mut out).unwrap();
        assert_eq!(out, vec![1, 2, 3, 4]);
        let mut tail = [0u8; 8];
        assert_eq!(peeked.read(&mut tail).unwrap(), 0);

        let mut direct = StrictEofReader::body(&[9, 8]);
        assert_eq!(direct.read(&mut tail).unwrap(), 2);
        assert_eq!(&tail[..2], &[9, 8]);
        assert_eq!(direct.read(&mut tail).unwrap(), 0);
        assert_eq!(direct.read(&mut tail).unwrap(), 0);
    }

    #[test]
    fn adts_routing_accepts_mpeg4_lc_without_crc_and_routes_other_variants_to_ffmpeg() {
        let aac_lc = [0xff, 0xf1, 0x50, 0x80, 0x00, 0x1f, 0xfc];
        assert_eq!(detect(&aac_lc), Container::Adts);
        assert!(!adts_needs_ffmpeg(&aac_lc));

        let engine = Engine { ffmpeg: None, in_process: true, bitrate: 128_000, complexity: 5 };
        let track = Track::new("adts".into(), Default::default());
        for unsupported in [
            [0xff, 0xf0, 0x50, 0x80],
            [0xff, 0xf9, 0x50, 0x80],
            [0xff, 0xf1, 0x10, 0x80],
            [0xff, 0xf1, 0x90, 0x80],
        ] {
            assert_eq!(detect(&unsupported), Container::Adts);
            assert!(adts_needs_ffmpeg(&unsupported));
            let err = engine
                .start(
                    &track,
                    Body::new(Box::new(io::Cursor::new(unsupported.to_vec()))),
                    0,
                    100,
                    &Object::default(),
                    Preference::Auto,
                )
                .err()
                .expect("unsupported ADTS variant should route directly to start_ffmpeg");
            assert!(matches!(err, AudioError::FfmpegMissing));
        }
    }
}
