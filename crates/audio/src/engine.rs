//! The engines, and the body they read from.
//!
//! Two engines can play a track. The in-process one decodes, filters and
//! encodes Opus inside the node; the FFmpeg one hands the whole job to a child
//! process. They are chosen per track, because the in-process pipeline covers
//! the common containers and FFmpeg covers everything else.
//!
//! Both are synchronous and take an already-open body. Opening a body is
//! network work, which belongs to the source layer, and keeping it out of here
//! means an engine can be tested by handing it a byte slice.

use std::io::{self, Read};

use rustlink_protocol::{Object, Track};

use crate::error::AudioError;
use crate::ffmpeg::FfmpegFrames;
use crate::filters::{self, FilterSet};
use crate::pure::PureFrames;

/// A track's bytes, with the first few readable twice.
///
/// The dispatcher has to look at the start of a stream to know what it is, and
/// whichever engine takes it must still see those bytes, so the head is read
/// into a buffer and served back before the rest of the stream.
pub struct Body {
    head: Vec<u8>,
    position: usize,
    inner: Box<dyn Read + Send>,
}

impl Body {
    pub fn new(inner: Box<dyn Read + Send>) -> Self {
        Body { head: Vec::new(), position: 0, inner }
    }

    /// Reads up to `count` bytes ahead without consuming them.
    pub fn peek(&mut self, count: usize) -> io::Result<()> {
        while self.head.len() < count {
            // The bytes are read straight into the buffer that keeps them, so
            // peeking does not allocate a chunk every time it needs more.
            let start = self.head.len();
            self.head.resize(count, 0);
            match self.inner.read(&mut self.head[start..]) {
                Ok(0) => {
                    self.head.truncate(start);
                    break;
                }
                Ok(read) => self.head.truncate(start + read),
                Err(error) => {
                    self.head.truncate(start);
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// The bytes that have been read ahead and not yet consumed.
    pub fn head(&self) -> &[u8] {
        &self.head[self.position..]
    }
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.position < self.head.len() {
            let count = (self.head.len() - self.position).min(buf.len());
            buf[..count].copy_from_slice(&self.head[self.position..self.position + count]);
            self.position += count;
            return Ok(count);
        }
        self.inner.read(buf)
    }
}

/// One Opus packet, which is one 20 ms frame.
pub trait Frames: Send {
    /// Replaces `packet` with the next one and reports whether there was one.
    ///
    /// The buffer belongs to the caller, which is what keeps a player that runs
    /// for hours from allocating once per twenty milliseconds. `false` means the
    /// track has ended.
    fn next_frame(&mut self, packet: &mut Vec<u8>) -> Result<bool, AudioError>;
}

/// Which engine to use for a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preference {
    /// Use the in-process pipeline where it applies, and fall back when it does
    /// not.
    Auto,
    /// Use FFmpeg, which is what a caller retries with after a decline.
    Ffmpeg,
}

/// A container, as far as choosing an engine is concerned.
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

/// Works out the container from the leading bytes.
pub fn detect(head: &[u8]) -> Container {
    if head.len() < 4 {
        return Container::Unknown;
    }
    if head.starts_with(b"OggS") {
        // The first page's payload starts after the header and its segment
        // table, and the codec names itself there.
        let segments = head.get(26).copied().unwrap_or(0) as usize;
        let payload = 27 + segments;
        return match head.get(payload..) {
            Some(rest) if rest.starts_with(b"OpusHead") => Container::OggOpus,
            Some([0x01, b'v', b'o', b'r', b'b', b'i', b's', ..]) => Container::OggVorbis,
            Some(rest) if rest.starts_with(b"\x7fFLAC") => Container::OggFlac,
            _ => Container::Unknown,
        };
    }
    if head.starts_with(b"fLaC") {
        return Container::Flac;
    }
    if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WAVE") {
        return Container::Wav;
    }
    if head.starts_with(b"ID3") {
        return Container::Mp3;
    }
    if head[0] == 0xff {
        match head[1] & 0xf0 {
            // ADTS starts with a sync word and no layer.
            0xf0 if head[1] & 0x06 == 0x00 => return Container::Adts,
            _ => {}
        }
        if head[1] & 0xe0 == 0xe0 && head[1] & 0x06 != 0 {
            return Container::Mp3;
        }
    }
    if head.len() >= 8 && &head[4..8] == b"ftyp" {
        return Container::Mp4;
    }
    if head.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return Container::Webm;
    }
    Container::Unknown
}

/// The engine, which knows how to play a track and not where it came from.
pub struct Engine {
    /// The FFmpeg executable, or `None` when there is none to run.
    pub ffmpeg: Option<String>,
    /// Whether the in-process pipeline may be used.
    pub in_process: bool,
    /// The Opus bitrate to encode at.
    pub bitrate: i32,
    /// How hard the Opus encoder works, on its own scale of zero to ten.
    pub complexity: i32,
}

impl Engine {
    /// Starts playback, returning the frames to send.
    pub fn start(
        &self,
        track: &Track,
        body: Body,
        position_ms: i64,
        volume: i64,
        filters: &Object,
        preference: Preference,
    ) -> Result<Box<dyn Frames>, AudioError> {
        let set = filters::parse(filters, volume)?;
        if preference == Preference::Ffmpeg
            || !self.in_process
            || !PureFrames::supports_timescale(&set)
        {
            return self.start_ffmpeg(body, position_ms, &set);
        }
        let mut body = body;
        body.peek(64)?;
        if detect(body.head()) == Container::Unknown {
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
        let Some(program) = self.ffmpeg.as_deref() else {
            return Err(AudioError::FfmpegMissing);
        };
        let frames =
            FfmpegFrames::start(program, body, position_ms, set, self.bitrate, self.complexity)?;
        Ok(Box::new(frames))
    }
}
