//! Container probing and audio decoding into interleaved stereo float PCM.

use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Mutex;

use symphonia::core::audio::{AudioBufferRef, Channels, SampleBuffer, Signal, SignalSpec};
use symphonia::core::codecs::{
    CodecParameters, Decoder as SymphoniaDecoder, DecoderOptions, CODEC_TYPE_AAC, CODEC_TYPE_NULL,
    CODEC_TYPE_OPUS,
};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::engine::{detect, Body, Container};
use crate::error::AudioError;
use crate::ogg::OggOpusReader;
use crate::opus_decode;

struct Unseekable {
    inner: Mutex<Body>,
}

impl Read for Unseekable {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let body = self.inner.get_mut().map_err(|_| io::Error::other("the source is poisoned"))?;
        body.read(buf).map_err(|error| {
            // Symphonia treats UnexpectedEof as normal EOF; preserve explicit source failures.
            if error.kind() == io::ErrorKind::UnexpectedEof {
                io::Error::other(error)
            } else {
                error
            }
        })
    }
}

impl Seek for Unseekable {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "the source cannot seek"))
    }
}

impl MediaSource for Unseekable {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

/// RFC 7845 section 4.6 80 ms (`3840` samples @ 48 kHz) Opus seek convergence window.
const OPUS_SEEK_PREROLL_FRAMES: u64 = 3_840;
/// Pre-roll window (~8 AAC-LC packets) decoded before target seek sample for MDCT overlap state.
const AAC_SEEK_PREROLL_FRAMES: u64 = 8_192;

pub(crate) enum PassthroughStep {
    Packet,
    Eof,
    FallbackToTranscode,
}

pub(crate) struct PassthroughIo<'a> {
    pub packet_index: u64,
    pub out_packet: &'a mut Vec<u8>,
    pub older: Option<&'a [u8]>,
    pub latest: Option<&'a [u8]>,
    pub k_tail: &'a mut Vec<f32>,
    pub next_pcm: &'a mut Vec<f32>,
}

/// Decodes a byte stream into interleaved stereo float samples at the source rate.
pub struct Decoder {
    inner: Inner,
    rate: u32,
}

enum Inner {
    Symphonia {
        format: Box<dyn FormatReader>,
        decoder: Box<dyn SymphoniaDecoder>,
        track_id: u32,
        spec: SignalSpec,
        mixer: Downmix,
        samples: SampleBuffer<f32>,
        can_skip_packets: bool,
        primed: bool,
        needs_reset: bool,
    },
    OggOpus(Box<OggDecoder>),
    ContainerOpus(Box<ContainerOpusDecoder>),
}

#[inline]
fn guard_unwind<T>(
    msg: &'static str,
    f: impl FnOnce() -> Result<T, AudioError>,
) -> Result<T, AudioError> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| Err(AudioError::Decode(msg.into())))
}

#[inline]
fn map_symphonia_error(err: SymphoniaError) -> AudioError {
    match err {
        SymphoniaError::IoError(e) if e.kind() != io::ErrorKind::UnexpectedEof => AudioError::Io(e),
        other => AudioError::Decode(other.to_string()),
    }
}

#[inline]
fn param_channels(params: &CodecParameters) -> Option<Channels> {
    params.channels.or_else(|| params.channel_layout.map(|l| l.into_channels()))
}

fn make_codec_decoder(params: &CodecParameters) -> Result<Box<dyn SymphoniaDecoder>, AudioError> {
    guard_unwind("the codec parameters are malformed", || {
        symphonia::default::get_codecs()
            .make(params, &DecoderOptions::default())
            .map_err(map_symphonia_error)
    })
}

#[derive(Clone, Copy, Default)]
struct Trim {
    start: usize,
    max_end: Option<usize>,
    end: usize,
}

struct OpusStream {
    decoder: opus_decode::Decoder,
    initial_skip: u32,
    skip: u32,
    gain: f32,
    gain_is_unity: bool,
    needs_reset: bool,
}

impl OpusStream {
    fn gain_factor(output_gain: i16) -> f32 {
        if output_gain == 0 {
            1.0
        } else {
            10.0f32.powf(f32::from(output_gain) / (20.0 * 256.0))
        }
    }

    fn new(skip: u32, output_gain: i16) -> Result<Self, AudioError> {
        Ok(Self {
            decoder: opus_decode::Decoder::new().map_err(|e| AudioError::Decode(e.to_string()))?,
            initial_skip: skip,
            skip,
            gain: Self::gain_factor(output_gain),
            gain_is_unity: output_gain == 0,
            needs_reset: false,
        })
    }

    fn configure(&mut self, skip: u32, output_gain: i16) {
        self.initial_skip = skip;
        self.skip = skip;
        self.gain_is_unity = output_gain == 0;
        self.gain = Self::gain_factor(output_gain);
    }

    #[inline]
    fn can_passthrough(&self) -> bool {
        self.gain_is_unity && self.initial_skip <= 480
    }

    #[inline]
    fn try_skip(&mut self, skip: &mut u64, drop_pre: usize, usable: usize) -> bool {
        if *skip > (usable as u64).saturating_add(OPUS_SEEK_PREROLL_FRAMES) {
            self.skip -= drop_pre as u32;
            *skip -= usable as u64;
            self.needs_reset = true;
            true
        } else {
            false
        }
    }

    fn warm_from_history(
        &mut self,
        older: Option<&[u8]>,
        latest: Option<&[u8]>,
        k_tail_out: &mut Vec<f32>,
    ) -> Result<(), AudioError> {
        self.skip = 0;
        self.needs_reset = false;
        self.decoder.reset();
        k_tail_out.clear();
        if let Some((pkt, expected)) =
            older.and_then(|p| Some((p, opus_decode::packet_samples(p)?)))
        {
            let _ = self.decode_packet(pkt, expected, Trim::default(), k_tail_out)?;
            k_tail_out.clear();
        }
        if let Some((pkt, expected)) =
            latest.and_then(|p| Some((p, opus_decode::packet_samples(p)?)))
        {
            let _ = self.decode_packet(pkt, expected, Trim::default(), k_tail_out)?;
        }
        Ok(())
    }

    fn step_passthrough(
        &mut self,
        raw: &[u8],
        expected: usize,
        trim_ok: bool,
        trim: Trim,
        io: PassthroughIo<'_>,
    ) -> Result<PassthroughStep, AudioError> {
        if self.can_passthrough() && trim_ok && opus_decode::is_valid_20ms_packet(raw) {
            self.skip -= (self.skip as usize).min(expected) as u32;
            io.out_packet.clear();
            io.out_packet.extend_from_slice(raw);
            return Ok(PassthroughStep::Packet);
        }
        io.next_pcm.clear();
        if io.packet_index > 0 {
            self.warm_from_history(io.older, io.latest, io.k_tail)?;
        } else {
            io.k_tail.clear();
        }
        let _ = self.decode_packet(raw, expected, trim, io.next_pcm)?;
        Ok(PassthroughStep::FallbackToTranscode)
    }

    #[inline]
    fn decode_packet(
        &mut self,
        packet: &[u8],
        expected: usize,
        trim: Trim,
        out: &mut Vec<f32>,
    ) -> Result<bool, AudioError> {
        if self.needs_reset {
            self.decoder.reset();
            self.needs_reset = false;
        }
        let base = out.len();
        let frames = match self.decoder.decode_float_append(packet, out) {
            Ok(frames) => frames,
            Err(_) => {
                out.truncate(base);
                self.decoder
                    .conceal_float_append(expected, out)
                    .map_err(|e| AudioError::Decode(e.to_string()))?
            }
        };
        let drop_pre = (self.skip as usize).min(frames);
        self.skip -= drop_pre as u32;
        let end = trim.max_end.map_or(frames, |keep| keep.min(frames)).saturating_sub(trim.end);
        let start = drop_pre.max(trim.start).min(end);
        if start == end {
            out.truncate(base);
            return Ok(false);
        }
        out.truncate(base + end * 2);
        if start > 0 {
            out.drain(base..base + start * 2);
        }
        let gain = self.gain;
        if gain == 1.0 {
            for s in &mut out[base..] {
                *s = crate::dsp::clean_f32(*s);
            }
        } else {
            for s in &mut out[base..] {
                *s = crate::dsp::clean_f32(*s * gain);
            }
        }
        Ok(true)
    }
}

struct ContainerOpusDecoder {
    format: Box<dyn FormatReader>,
    opus: OpusStream,
    track_id: u32,
}

fn parse_container_opus_params(params: &CodecParameters) -> Result<(u32, i16), AudioError> {
    let channels = param_channels(params).map_or(2, |c| c.count());
    if !(1..=2).contains(&channels) {
        return Err(AudioError::Decode("unsupported Opus channel count".into()));
    }
    let extra = params
        .extra_data
        .as_deref()
        .ok_or_else(|| AudioError::Decode("missing Opus header in container".into()))?;
    let (be, body) = match extra.strip_prefix(b"OpusHead") {
        // Symphonia `isomp4` prepends `b"OpusHead"` onto big-endian ISO-BMFF `dOps` (version 0).
        Some([0, rest @ ..]) => (true, rest),
        // Matroska/WebM CodecPrivate carries little-endian RFC 7845 `OpusHead` (version 1..=15).
        Some([1..=15, rest @ ..]) => (false, rest),
        Some(_) => return Err(AudioError::Decode("unsupported OpusHead in container".into())),
        None => match extra {
            [0, rest @ ..] => (true, rest),
            _ => return Err(AudioError::Decode("unsupported Opus header in container".into())),
        },
    };
    let &[ch, s0, s1, _, _, _, _, g0, g1, family, ..] = body else {
        return Err(AudioError::Decode("unsupported OpusHead in container".into()));
    };
    if !(1..=2).contains(&ch) || family != 0 {
        return Err(AudioError::Decode("unsupported Opus channel mapping".into()));
    }
    let (head_skip, output_gain) = if be {
        (u16::from_be_bytes([s0, s1]), i16::from_be_bytes([g0, g1]))
    } else {
        (u16::from_le_bytes([s0, s1]), i16::from_le_bytes([g0, g1]))
    };
    let skip = match params.delay.unwrap_or(0) {
        0 => u32::from(head_skip),
        delay => delay,
    };
    Ok((skip, output_gain))
}

impl ContainerOpusDecoder {
    fn from_track(
        format: Box<dyn FormatReader>,
        track_id: u32,
        params: &CodecParameters,
    ) -> Result<Self, AudioError> {
        let (skip, output_gain) = parse_container_opus_params(params)?;
        Ok(Self { format, opus: OpusStream::new(skip, output_gain)?, track_id })
    }

    fn reset_track(&mut self) -> Result<(), AudioError> {
        let track = select_audio_track(self.format.as_ref())?;
        if track.codec_params.codec != CODEC_TYPE_OPUS {
            return Err(AudioError::Decode("chained container codec changed".into()));
        }
        let (pre_skip, output_gain) = parse_container_opus_params(&track.codec_params)?;
        let new_id = track.id;
        self.opus = OpusStream::new(pre_skip, output_gain)?;
        self.track_id = new_id;
        Ok(())
    }

    fn decode_with_skip(&mut self, skip: &mut u64, out: &mut Vec<f32>) -> Result<bool, AudioError> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(false);
                }
                Err(SymphoniaError::ResetRequired) => {
                    self.reset_track()?;
                    continue;
                }
                Err(e) => return Err(map_symphonia_error(e)),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let buf = packet.buf();
            let Some(expected) = opus_decode::packet_samples(buf) else { continue };
            let trim = Trim {
                start: packet.trim_start() as usize,
                max_end: None,
                end: packet.trim_end() as usize,
            };
            let drop_pre = (self.opus.skip as usize).min(expected);
            let end = expected.saturating_sub(trim.end);
            let usable = end - drop_pre.max(trim.start).min(end);
            if self.opus.try_skip(skip, drop_pre, usable) {
                continue;
            }
            if self.opus.decode_packet(buf, expected, trim, out)? {
                return Ok(true);
            }
        }
    }

    fn try_passthrough_next(
        &mut self,
        io: PassthroughIo<'_>,
    ) -> Result<PassthroughStep, AudioError> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(PassthroughStep::Eof);
                }
                Err(SymphoniaError::ResetRequired) => {
                    self.reset_track()?;
                    if io.packet_index > 0 {
                        io.k_tail.clear();
                        io.next_pcm.clear();
                        let mut zero = 0;
                        let _ = self.decode_with_skip(&mut zero, io.next_pcm)?;
                        return Ok(PassthroughStep::FallbackToTranscode);
                    }
                    continue;
                }
                Err(e) => return Err(map_symphonia_error(e)),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let buf = packet.buf();
            let Some(expected) = opus_decode::packet_samples(buf) else { continue };
            let trim = Trim {
                start: packet.trim_start() as usize,
                max_end: None,
                end: packet.trim_end() as usize,
            };
            let trim_ok = trim.end == 0
                && if io.packet_index == 0 { trim.start <= 480 } else { trim.start == 0 };
            return self.opus.step_passthrough(buf, expected, trim_ok, trim, io);
        }
    }
}

struct OggPacket {
    offset: u32,
    length: u32,
    frames: u16,
    keep: u16,
}

struct OggDecoder {
    reader: OggOpusReader<Body>,
    opus: OpusStream,
    page: Vec<u8>,
    packets: VecDeque<OggPacket>,
    previous_granule: Option<u64>,
    ended: bool,
}

impl OggDecoder {
    fn new(body: Body) -> Result<Self, AudioError> {
        let mut decoder = Self {
            reader: OggOpusReader::new(body),
            opus: OpusStream::new(0, 0)?,
            page: Vec::new(),
            packets: VecDeque::new(),
            previous_granule: None,
            ended: false,
        };
        if !decoder.read_page()? {
            return Err(AudioError::Decode("the stream holds no audio".into()));
        }
        Ok(decoder)
    }

    fn read_page(&mut self) -> Result<bool, AudioError> {
        if self.ended {
            return Ok(false);
        }
        self.page.clear();
        self.packets.clear();
        let mut total = 0u64;
        loop {
            let offset = self.page.len();
            if !self.reader.append_next_packet(&mut self.page)? {
                return if self.packets.is_empty() {
                    Ok(false)
                } else {
                    Err(AudioError::Decode("missing ogg page granule".into()))
                };
            }
            let raw_packet = &self.page[offset..];
            let frames = opus_decode::packet_samples(raw_packet)
                .ok_or_else(|| AudioError::Decode("invalid opus packet duration".into()))?;
            total += frames as u64;
            self.packets.push_back(OggPacket {
                offset: offset as u32,
                length: raw_packet.len() as u32,
                frames: frames as u16,
                keep: frames as u16,
            });
            let Some((granule, end_of_stream)) = self.reader.packet_page_end() else {
                if self.packets.len() >= 255 {
                    return Err(AudioError::Decode("too many packets on an ogg page".into()));
                }
                continue;
            };
            if granule == u64::MAX {
                return Err(AudioError::Decode("missing ogg audio granule".into()));
            }
            let keep = match self.previous_granule {
                None => {
                    self.opus.configure(self.reader.pre_skip(), self.reader.output_gain());
                    if end_of_stream {
                        granule.min(total)
                    } else if granule < total {
                        return Err(AudioError::Decode("invalid initial ogg granule".into()));
                    } else {
                        total
                    }
                }
                Some(previous) => {
                    let delta = granule
                        .checked_sub(previous)
                        .ok_or_else(|| AudioError::Decode("ogg granule moved backwards".into()))?;
                    if delta > total || (!end_of_stream && delta != total) {
                        return Err(AudioError::Decode(
                            "ogg granule disagrees with packet duration".into(),
                        ));
                    }
                    delta
                }
            };
            if end_of_stream && keep < u64::from(self.opus.skip) {
                return Err(AudioError::Decode("ogg end precedes pre-skip".into()));
            }
            let mut remaining = keep as usize;
            for packet in &mut self.packets {
                let keep_frames = remaining.min(usize::from(packet.frames));
                packet.keep = keep_frames as u16;
                remaining -= keep_frames;
            }
            self.previous_granule = Some(granule);
            self.ended = end_of_stream;
            return Ok(true);
        }
    }

    fn next_loaded_packet(&mut self) -> Result<Option<OggPacket>, AudioError> {
        loop {
            if self.packets.is_empty() && !self.read_page()? {
                return Ok(None);
            }
            let packet = self.packets.pop_front().expect("a nonempty audio page was loaded");
            if !(self.ended && packet.keep == 0) {
                return Ok(Some(packet));
            }
        }
    }

    fn decode_with_skip(&mut self, skip: &mut u64, out: &mut Vec<f32>) -> Result<bool, AudioError> {
        while let Some(packet) = self.next_loaded_packet()? {
            let frames = usize::from(packet.frames);
            let keep = usize::from(packet.keep).min(frames);
            let drop_pre = (self.opus.skip as usize).min(frames);
            if self.opus.try_skip(skip, drop_pre, keep.saturating_sub(drop_pre)) {
                continue;
            }
            let start_byte = packet.offset as usize;
            let raw = &self.page[start_byte..start_byte + packet.length as usize];
            let trim = Trim { start: 0, max_end: Some(keep), end: 0 };
            if self.opus.decode_packet(raw, frames, trim, out)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn try_passthrough_next(
        &mut self,
        io: PassthroughIo<'_>,
    ) -> Result<PassthroughStep, AudioError> {
        let Some(packet) = self.next_loaded_packet()? else {
            return Ok(PassthroughStep::Eof);
        };
        let frames = usize::from(packet.frames);
        let keep = usize::from(packet.keep).min(frames);
        let start_byte = packet.offset as usize;
        let raw = &self.page[start_byte..start_byte + packet.length as usize];
        let trim = Trim { start: 0, max_end: Some(keep), end: 0 };
        self.opus.step_passthrough(raw, frames, keep == frames, trim, io)
    }
}

fn select_audio_track(
    format: &dyn FormatReader,
) -> Result<&symphonia::core::formats::Track, AudioError> {
    format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .or_else(|| format.default_track())
        .ok_or_else(|| AudioError::Decode("the stream holds no playable track".into()))
}

fn parse_aac_lc_rate(
    extra: Option<&[u8]>,
    container_rate: Option<u32>,
) -> Result<Option<u32>, AudioError> {
    const AAC_RATES: [u32; 13] = [
        96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
        8_000, 7_350,
    ];
    let Some(extra) = extra else { return Ok(None) };
    if extra.len() < 2 {
        return Err(AudioError::Decode("truncated AAC AudioSpecificConfig".into()));
    }
    let mut pos = 0usize;
    let read_bits = |pos: &mut usize, count: usize| -> Option<u32> {
        if *pos + count > extra.len() * 8 {
            return None;
        }
        let mut val = 0u32;
        for _ in 0..count {
            val = (val << 1) | u32::from((extra[*pos >> 3] >> (7 - (*pos & 7))) & 1);
            *pos += 1;
        }
        Some(val)
    };
    let trunc = || AudioError::Decode("truncated AAC config".into());
    let mut aot = read_bits(&mut pos, 5).ok_or_else(trunc)?;
    if aot == 31 {
        aot = 32 + read_bits(&mut pos, 6).ok_or_else(trunc)?;
    }
    if aot == 5 || aot == 29 {
        return Err(AudioError::Decode("HE-AAC (SBR/PS) requires ffmpeg".into()));
    }
    let asc_rate = match read_bits(&mut pos, 4).ok_or_else(trunc)? {
        15 => read_bits(&mut pos, 24)
            .filter(|&r| r > 0)
            .ok_or_else(|| AudioError::Decode("invalid AAC sample rate".into()))?,
        idx => *AAC_RATES
            .get(idx as usize)
            .ok_or_else(|| AudioError::Decode("unsupported AAC sample rate".into()))?,
    };
    let chan_cfg = read_bits(&mut pos, 4).ok_or_else(trunc)?;
    if aot == 2 && chan_cfg != 0 {
        let _short = read_bits(&mut pos, 1);
        if read_bits(&mut pos, 1) == Some(1) {
            let _ = read_bits(&mut pos, 14);
        }
        if read_bits(&mut pos, 1) == Some(0)
            && extra.len() * 8 - pos >= 16
            && read_bits(&mut pos, 11) == Some(0x2b7)
        {
            let mut ext_aot = read_bits(&mut pos, 5).unwrap_or(0);
            if ext_aot == 31 {
                ext_aot = 32 + read_bits(&mut pos, 6).unwrap_or(0);
            }
            if (ext_aot == 5 || ext_aot == 29) && read_bits(&mut pos, 1) == Some(1) {
                return Err(AudioError::Decode("HE-AAC (SBR/PS) requires ffmpeg".into()));
            }
        }
    }
    if container_rate == Some(asc_rate.saturating_mul(2)) {
        return Err(AudioError::Decode("implicit HE-AAC (SBR) requires ffmpeg".into()));
    }
    Ok(Some(asc_rate))
}

fn symphonia_rate_and_skip(
    params: &CodecParameters,
    default_rate: u32,
) -> Result<(u32, bool), AudioError> {
    let asc_rate = if params.codec == CODEC_TYPE_AAC {
        parse_aac_lc_rate(params.extra_data.as_deref(), params.sample_rate)?
    } else {
        None
    };
    let rate = asc_rate.or(params.sample_rate).unwrap_or(default_rate);
    if rate == 0 {
        return Err(AudioError::Decode("the stream has a zero sample rate".into()));
    }
    let can_skip = params.codec == CODEC_TYPE_AAC
        && params.time_base == Some(symphonia::core::units::TimeBase::new(1, rate));
    Ok((rate, can_skip))
}

impl Decoder {
    pub fn new(mut body: Body) -> Result<Self, AudioError> {
        body.peek(512)?;
        let container = detect(body.head());
        if container == Container::OggOpus {
            return Ok(Self {
                inner: Inner::OggOpus(Box::new(OggDecoder::new(body)?)),
                rate: 48_000,
            });
        }

        let source = Unseekable { inner: Mutex::new(body) };
        let stream = MediaSourceStream::new(Box::new(source), Default::default());
        let probed = guard_unwind("the stream header is malformed", || {
            symphonia::default::get_probe()
                .format(
                    &Hint::new(),
                    stream,
                    &FormatOptions { enable_gapless: true, ..Default::default() },
                    &MetadataOptions::default(),
                )
                .map_err(map_symphonia_error)
        })?;
        let format = probed.format;
        if container == Container::Mp4 && format.tracks().len() > 1 {
            return Err(AudioError::Decode(
                "multi-track MP4 on unseekable stream requires ffmpeg".into(),
            ));
        }
        let track = select_audio_track(format.as_ref())?;
        let track_id = track.id;
        if track.codec_params.codec == CODEC_TYPE_OPUS {
            let params = track.codec_params.clone();
            let opus = ContainerOpusDecoder::from_track(format, track_id, &params)?;
            return Ok(Self { inner: Inner::ContainerOpus(Box::new(opus)), rate: 48_000 });
        }
        let (rate, can_skip_packets) = symphonia_rate_and_skip(&track.codec_params, 48_000)?;
        let channels = param_channels(&track.codec_params)
            .unwrap_or(Channels::FRONT_LEFT | Channels::FRONT_RIGHT);
        if channels.count() == 0 {
            return Err(AudioError::Decode("the stream has zero audio channels".into()));
        }
        let decoder = make_codec_decoder(&track.codec_params)?;
        let spec = SignalSpec::new(rate, channels);
        Ok(Self {
            inner: Inner::Symphonia {
                format,
                decoder,
                track_id,
                spec,
                mixer: Downmix::new(channels),
                samples: SampleBuffer::new(0, spec),
                can_skip_packets,
                primed: false,
                needs_reset: false,
            },
            rate,
        })
    }

    #[inline]
    pub fn rate(&self) -> u32 {
        self.rate
    }

    #[inline]
    pub fn supports_opus_passthrough(&self) -> bool {
        match &self.inner {
            Inner::OggOpus(d) => d.opus.can_passthrough(),
            Inner::ContainerOpus(d) => d.opus.can_passthrough(),
            Inner::Symphonia { .. } => false,
        }
    }

    pub(crate) fn warm_from_passthrough(
        &mut self,
        older_hist: Option<&[u8]>,
        latest_hist: Option<&[u8]>,
        k_tail_out: &mut Vec<f32>,
    ) -> Result<(), AudioError> {
        match &mut self.inner {
            Inner::OggOpus(d) => d.opus.warm_from_history(older_hist, latest_hist, k_tail_out),
            Inner::ContainerOpus(d) => {
                d.opus.warm_from_history(older_hist, latest_hist, k_tail_out)
            }
            Inner::Symphonia { .. } => {
                k_tail_out.clear();
                Ok(())
            }
        }
    }

    pub(crate) fn try_passthrough_next(
        &mut self,
        io: PassthroughIo<'_>,
    ) -> Result<PassthroughStep, AudioError> {
        guard_unwind("the decoder panicked on the stream", || match &mut self.inner {
            Inner::OggOpus(d) => d.try_passthrough_next(io),
            Inner::ContainerOpus(d) => d.try_passthrough_next(io),
            Inner::Symphonia { .. } => Ok(PassthroughStep::FallbackToTranscode),
        })
    }

    pub fn decode(&mut self, out: &mut Vec<f32>) -> Result<bool, AudioError> {
        let mut zero = 0;
        self.decode_with_skip(&mut zero, out)
    }

    pub fn decode_with_skip(
        &mut self,
        skip: &mut u64,
        out: &mut Vec<f32>,
    ) -> Result<bool, AudioError> {
        guard_unwind("the decoder panicked on the stream", || self.decode_next_with_skip(skip, out))
    }

    fn decode_next_with_skip(
        &mut self,
        skip: &mut u64,
        out: &mut Vec<f32>,
    ) -> Result<bool, AudioError> {
        match &mut self.inner {
            Inner::OggOpus(decoder) => decoder.decode_with_skip(skip, out),
            Inner::ContainerOpus(decoder) => decoder.decode_with_skip(skip, out),
            Inner::Symphonia {
                format,
                decoder,
                track_id,
                spec,
                mixer,
                samples,
                can_skip_packets,
                primed,
                needs_reset,
            } => loop {
                let packet = match format.next_packet() {
                    Ok(packet) => packet,
                    Err(SymphoniaError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                        return Ok(false);
                    }
                    Err(SymphoniaError::ResetRequired) => {
                        let track = select_audio_track(format.as_ref())?;
                        let (new_rate, new_can_skip) =
                            symphonia_rate_and_skip(&track.codec_params, self.rate)?;
                        if new_rate != self.rate {
                            return Err(AudioError::Decode(
                                "source sample rate changed during playback".into(),
                            ));
                        }
                        let new_id = track.id;
                        let new_channels =
                            param_channels(&track.codec_params).unwrap_or(spec.channels);
                        if new_channels.count() == 0 {
                            return Err(AudioError::Decode(
                                "the stream has zero audio channels".into(),
                            ));
                        }
                        *can_skip_packets = new_can_skip;
                        *primed = false;
                        *needs_reset = false;
                        *decoder = make_codec_decoder(&track.codec_params)?;
                        *track_id = new_id;
                        if new_channels != spec.channels {
                            *spec = SignalSpec::new(self.rate, new_channels);
                            *mixer = Downmix::new(new_channels);
                            *samples = SampleBuffer::new(0, *spec);
                        }
                        continue;
                    }
                    Err(e) => return Err(map_symphonia_error(e)),
                };
                if packet.track_id() != *track_id {
                    continue;
                }
                if *primed
                    && *can_skip_packets
                    && packet.trim_start() == 0
                    && packet.trim_end() == 0
                    && packet.dur() > 0
                    && *skip > packet.dur().saturating_add(AAC_SEEK_PREROLL_FRAMES)
                {
                    *skip -= packet.dur();
                    *needs_reset = true;
                    continue;
                }
                if *needs_reset {
                    decoder.reset();
                    *needs_reset = false;
                }
                let audio = match decoder.decode(&packet) {
                    Ok(audio) => audio,
                    Err(SymphoniaError::DecodeError(_)) => continue,
                    Err(e) => return Err(map_symphonia_error(e)),
                };
                if audio.frames() == 0 {
                    continue;
                }
                *primed = true;
                let decoded_spec = *audio.spec();
                if decoded_spec.rate != self.rate {
                    return Err(AudioError::Decode(
                        "source sample rate changed during playback".into(),
                    ));
                }
                if decoded_spec.channels.count() == 0 {
                    return Err(AudioError::Decode("the stream has zero audio channels".into()));
                }
                let channels_changed = decoded_spec.channels != spec.channels;
                if channels_changed {
                    *spec = decoded_spec;
                    *mixer = Downmix::new(spec.channels);
                }
                if try_decode_direct(mixer, &audio, out) {
                    return Ok(true);
                }
                let needed = audio.frames() * decoded_spec.channels.count();
                if channels_changed || samples.capacity() < needed {
                    *samples = SampleBuffer::new((audio.frames() * 2) as u64, decoded_spec);
                }
                samples.copy_interleaved_ref(audio);
                mixer.process(samples.samples(), out);
                return Ok(true);
            },
        }
    }
}

#[inline]
fn append_stereo<T: Copy>(
    left: &[T],
    right: &[T],
    out: &mut Vec<f32>,
    mut map: impl FnMut(T) -> f32,
) {
    let frames = left.len().min(right.len());
    let base = out.len();
    out.reserve(frames * 2);
    let dst = &mut out.spare_capacity_mut().as_chunks_mut::<2>().0[..frames];
    for ((slot, &l), &r) in dst.iter_mut().zip(&left[..frames]).zip(&right[..frames]) {
        slot[0].write(map(l));
        slot[1].write(map(r));
    }
    // SAFETY: `frames` stereo pairs were reserved and fully initialized above.
    unsafe { out.set_len(base + frames * 2) };
}

#[inline]
fn append_mono<T: Copy>(mono: &[T], out: &mut Vec<f32>, mut map: impl FnMut(T) -> f32) {
    let frames = mono.len();
    let base = out.len();
    out.reserve(frames * 2);
    let dst = &mut out.spare_capacity_mut().as_chunks_mut::<2>().0[..frames];
    for (slot, &m) in dst.iter_mut().zip(mono) {
        let s = map(m);
        slot[0].write(s);
        slot[1].write(s);
    }
    // SAFETY: `frames` stereo pairs were reserved and fully initialized above.
    unsafe { out.set_len(base + frames * 2) };
}

fn try_decode_direct(mixer: &Downmix, audio: &AudioBufferRef<'_>, out: &mut Vec<f32>) -> bool {
    const S16_SCALE: f32 = 1.0 / 32_768.0;
    const S32_SCALE: f32 = 1.0 / 2_147_483_648.0;
    match (mixer, audio) {
        (Downmix::Stereo, AudioBufferRef::F32(buf)) => {
            append_stereo(buf.chan(0), buf.chan(1), out, crate::dsp::clean_f32);
        }
        (Downmix::Stereo, AudioBufferRef::S16(buf)) => {
            append_stereo(buf.chan(0), buf.chan(1), out, |s| f32::from(s) * S16_SCALE);
        }
        (Downmix::Stereo, AudioBufferRef::S32(buf)) => {
            append_stereo(buf.chan(0), buf.chan(1), out, |s| s as f32 * S32_SCALE);
        }
        (Downmix::Mono, AudioBufferRef::F32(buf)) => {
            append_mono(buf.chan(0), out, crate::dsp::clean_f32);
        }
        (Downmix::Mono, AudioBufferRef::S16(buf)) => {
            append_mono(buf.chan(0), out, |s| f32::from(s) * S16_SCALE);
        }
        (Downmix::Mono, AudioBufferRef::S32(buf)) => {
            append_mono(buf.chan(0), out, |s| s as f32 * S32_SCALE);
        }
        _ => return false,
    }
    true
}

enum Downmix {
    Empty,
    Mono,
    Stereo,
    FallbackMono { count: usize, inv_count: f32 },
    Matrix { count: usize, weights: Vec<[f32; 2]>, inv_left: f32, inv_right: f32 },
}

impl Downmix {
    fn new(channels: Channels) -> Self {
        let count = channels.count();
        match count {
            0 => return Self::Empty,
            1 => return Self::Mono,
            2 if channels == (Channels::FRONT_LEFT | Channels::FRONT_RIGHT) => return Self::Stereo,
            _ => {}
        }
        let mut weights = Vec::with_capacity(count);
        let (mut left_sum, mut right_sum) = (0.0f32, 0.0f32);
        for channel in channels.iter() {
            let [lw, rw] = match channel {
                Channels::TOP_CENTRE | Channels::TOP_REAR_CENTRE | Channels::REAR_CENTRE => {
                    [0.5, 0.5]
                }
                Channels::FRONT_CENTRE
                | Channels::TOP_FRONT_CENTRE
                | Channels::FRONT_CENTRE_HIGH => [0.707, 0.707],
                Channels::FRONT_LEFT
                | Channels::FRONT_LEFT_CENTRE
                | Channels::TOP_FRONT_LEFT
                | Channels::FRONT_LEFT_WIDE
                | Channels::FRONT_LEFT_HIGH => [1.0, 0.0],
                Channels::FRONT_RIGHT
                | Channels::FRONT_RIGHT_CENTRE
                | Channels::TOP_FRONT_RIGHT
                | Channels::FRONT_RIGHT_WIDE
                | Channels::FRONT_RIGHT_HIGH => [0.0, 1.0],
                Channels::REAR_LEFT
                | Channels::SIDE_LEFT
                | Channels::TOP_REAR_LEFT
                | Channels::REAR_LEFT_CENTRE => [0.707, 0.0],
                Channels::REAR_RIGHT
                | Channels::SIDE_RIGHT
                | Channels::TOP_REAR_RIGHT
                | Channels::REAR_RIGHT_CENTRE => [0.0, 0.707],
                _ => [0.0, 0.0],
            };
            weights.push([lw, rw]);
            left_sum += lw;
            right_sum += rw;
        }
        if left_sum == 0.0 && right_sum == 0.0 {
            return Self::FallbackMono { count, inv_count: 1.0 / count as f32 };
        }
        Self::Matrix {
            count,
            weights,
            inv_left: 1.0 / left_sum.max(1.0),
            inv_right: 1.0 / right_sum.max(1.0),
        }
    }

    fn process(&self, samples: &[f32], out: &mut Vec<f32>) {
        match self {
            Self::Empty => {}
            Self::Mono => append_mono(samples, out, crate::dsp::clean_f32),
            Self::Stereo => {
                let len = samples.as_chunks::<2>().0.len() * 2;
                out.extend(samples[..len].iter().copied().map(crate::dsp::clean_f32));
            }
            Self::FallbackMono { count, inv_count } => {
                let chunks = samples.chunks_exact(*count);
                out.reserve(chunks.len() * 2);
                for frame in chunks {
                    let m = crate::dsp::clean_f32(
                        frame.iter().copied().map(crate::dsp::clean_f32).sum::<f32>() * inv_count,
                    );
                    out.extend_from_slice(&[m, m]);
                }
            }
            Self::Matrix { count, weights, inv_left, inv_right } => {
                let chunks = samples.chunks_exact(*count);
                out.reserve(chunks.len() * 2);
                for frame in chunks {
                    let (mut l, mut r) = (0.0f32, 0.0f32);
                    for (&sample, &[lw, rw]) in frame.iter().zip(weights) {
                        let s = crate::dsp::clean_f32(sample);
                        l += s * lw;
                        r += s * rw;
                    }
                    out.extend_from_slice(&[
                        crate::dsp::clean_f32(l * inv_left),
                        crate::dsp::clean_f32(r * inv_right),
                    ]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn downmix(samples: &[f32], channels: Channels) -> Vec<f32> {
        let mut out = Vec::new();
        Downmix::new(channels).process(samples, &mut out);
        out
    }

    #[test]
    fn mono_is_copied_to_both_sides() {
        for channel in [
            Channels::FRONT_LEFT,
            Channels::FRONT_CENTRE,
            Channels::TOP_CENTRE,
            Channels::TOP_FRONT_CENTRE,
            Channels::TOP_REAR_CENTRE,
            Channels::REAR_CENTRE,
        ] {
            assert_eq!(downmix(&[1.0, 0.5], channel), vec![1.0, 1.0, 0.5, 0.5]);
        }
    }

    #[test]
    fn stereo_is_passed_through() {
        assert_eq!(
            downmix(&[1.0, 0.0, 0.0, 1.0], Channels::FRONT_LEFT | Channels::FRONT_RIGHT),
            vec![1.0, 0.0, 0.0, 1.0]
        );
    }

    #[test]
    fn a_nonstereo_two_channel_layout_is_not_passed_through_as_left_right() {
        let out = downmix(&[1.0, 0.75], Channels::FRONT_CENTRE | Channels::LFE1);
        assert!((out[0] - 0.707).abs() < 1e-6 && (out[1] - 0.707).abs() < 1e-6);
    }

    #[test]
    fn left_and_right_centre_channels_are_not_silenced() {
        let out = downmix(&[1.0, 0.75], Channels::FRONT_LEFT_CENTRE | Channels::FRONT_RIGHT_CENTRE);
        assert!((out[0] - 1.0).abs() < 1e-6 && (out[1] - 0.75).abs() < 1e-6);
    }

    #[test]
    fn top_front_left_and_right_channels_are_not_silenced() {
        let out = downmix(&[1.0, 0.75], Channels::TOP_FRONT_LEFT | Channels::TOP_FRONT_RIGHT);
        assert!((out[0] - 1.0).abs() < 1e-6 && (out[1] - 0.75).abs() < 1e-6);
    }

    #[test]
    fn multichannel_overhead_centre_uses_the_half_level_stereo_matrix() {
        assert_eq!(downmix(&[0.0, 1.0], Channels::TOP_CENTRE | Channels::LFE1), vec![0.5, 0.5]);
    }

    #[test]
    fn multichannel_rear_centre_is_half_level_but_front_height_centre_is_not() {
        assert_eq!(downmix(&[0.0, 1.0], Channels::LFE1 | Channels::REAR_CENTRE), vec![0.5, 0.5]);
        assert_eq!(
            downmix(&[0.0, 1.0], Channels::LFE1 | Channels::TOP_FRONT_CENTRE),
            vec![0.707, 0.707]
        );
    }

    #[test]
    fn a_layout_without_known_positions_falls_back_to_audible_mono() {
        let out = downmix(&[1.0, 0.5], Channels::LFE1 | Channels::LFE2);
        assert!((out[0] - 0.75).abs() < 1e-6 && (out[1] - 0.75).abs() < 1e-6);
    }

    #[test]
    fn surround_is_folded_with_the_centre_in_both_sides() {
        let layout =
            Channels::FRONT_LEFT | Channels::FRONT_RIGHT | Channels::FRONT_CENTRE | Channels::LFE1;
        let out = downmix(&[1.0, 0.0, 1.0, 1.0], layout);
        let gain = 1.0 + 0.707;
        assert!((out[0] - (1.0 + 0.707) / gain).abs() < 1e-6);
        assert!((out[1] - 0.707 / gain).abs() < 1e-6);
    }

    #[test]
    fn a_surround_signal_at_full_scale_does_not_clip() {
        let layout = Channels::FRONT_LEFT
            | Channels::FRONT_RIGHT
            | Channels::FRONT_CENTRE
            | Channels::REAR_LEFT
            | Channels::REAR_RIGHT;
        let out = downmix(&[1.0f32; 20], layout);
        assert!(out.iter().all(|&s| s <= 1.0));
    }

    #[test]
    fn symphonia_isomp4_dops_big_endian_vs_rfc7845_little_endian() {
        let mut mp4_extra = b"OpusHead".to_vec();
        mp4_extra.extend_from_slice(&[0, 2, 0x01, 0x38, 0x00, 0x00, 0xbb, 0x80, 0x01, 0x00, 0]);
        let mut mp4_params = CodecParameters::new();
        mp4_params.with_extra_data(mp4_extra.into_boxed_slice());
        assert_eq!(parse_container_opus_params(&mp4_params).unwrap(), (312, 256));

        let mut webm_extra = b"OpusHead".to_vec();
        webm_extra.extend_from_slice(&[1, 2, 0x38, 0x01, 0x80, 0xbb, 0x00, 0x00, 0x00, 0x01, 0]);
        let mut webm_params = CodecParameters::new();
        webm_params.with_extra_data(webm_extra.into_boxed_slice());
        assert_eq!(parse_container_opus_params(&webm_params).unwrap(), (312, 256));
    }

    #[test]
    fn container_opus_params_require_header_and_honor_channel_layout() {
        use symphonia::core::audio::Layout;

        assert!(parse_container_opus_params(&CodecParameters::new()).is_err());

        let mut webm_extra = b"OpusHead".to_vec();
        webm_extra.extend_from_slice(&[1, 2, 0x38, 0x01, 0x80, 0xbb, 0x00, 0x00, 0x00, 0x00, 0]);
        let mut surround = CodecParameters::new();
        surround
            .with_channel_layout(Layout::FivePointOne)
            .with_extra_data(webm_extra.into_boxed_slice());
        assert!(parse_container_opus_params(&surround).is_err());

        let mut mono = CodecParameters::new();
        mono.with_channel_layout(Layout::Mono);
        assert_eq!(param_channels(&mono), Some(Channels::FRONT_LEFT));
    }

    fn mp4_box(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + payload.len());
        out.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
        out.extend_from_slice(tag);
        out.extend_from_slice(payload);
        out
    }

    fn minimal_mp4(tracks: u32) -> Vec<u8> {
        let mut out = mp4_box(b"ftyp", b"isom\0\0\0\0");
        let mut mvhd = [0u8; 100];
        mvhd[12..16].copy_from_slice(&48_000u32.to_be_bytes());
        let mut moov_body = mp4_box(b"mvhd", &mvhd);
        for id in 1..=tracks {
            let mut tkhd = [0u8; 84];
            tkhd[12..16].copy_from_slice(&id.to_be_bytes());
            let mut mdhd = [0u8; 24];
            mdhd[12..16].copy_from_slice(&48_000u32.to_be_bytes());
            let mut hdlr = [0u8; 25];
            hdlr[8..12].copy_from_slice(b"soun");
            let mut sowt = [0u8; 28];
            sowt[6..8].copy_from_slice(&1u16.to_be_bytes());
            sowt[16..18].copy_from_slice(&2u16.to_be_bytes());
            sowt[18..20].copy_from_slice(&16u16.to_be_bytes());
            sowt[24..28].copy_from_slice(&(48_000u32 << 16).to_be_bytes());
            let mut stsd_payload = vec![0, 0, 0, 0, 0, 0, 0, 1];
            stsd_payload.extend_from_slice(&mp4_box(b"sowt", &sowt));
            let empty_table = [0u8; 8];
            let mut stsz = [0u8; 12];
            stsz[4..8].copy_from_slice(&4u32.to_be_bytes());
            let mut stbl_body = mp4_box(b"stsd", &stsd_payload);
            for tag in [b"stts", b"stsc"] {
                stbl_body.extend_from_slice(&mp4_box(tag, &empty_table));
            }
            stbl_body.extend_from_slice(&mp4_box(b"stsz", &stsz));
            stbl_body.extend_from_slice(&mp4_box(b"stco", &empty_table));
            let mut mdia_body = mp4_box(b"mdhd", &mdhd);
            mdia_body.extend_from_slice(&mp4_box(b"hdlr", &hdlr));
            mdia_body.extend_from_slice(&mp4_box(b"minf", &mp4_box(b"stbl", &stbl_body)));
            let mut trak_body = mp4_box(b"tkhd", &tkhd);
            trak_body.extend_from_slice(&mp4_box(b"mdia", &mdia_body));
            moov_body.extend_from_slice(&mp4_box(b"trak", &trak_body));
        }
        out.extend_from_slice(&mp4_box(b"moov", &moov_body));
        out.extend_from_slice(&mp4_box(b"mdat", &[0u8; 16]));
        out
    }

    #[test]
    fn multi_track_mp4_is_declined_while_single_track_mp4_opens() {
        let single = Decoder::new(Body::new(Box::new(io::Cursor::new(minimal_mp4(1))))).unwrap();
        assert_eq!(single.rate(), 48_000);

        let err = Decoder::new(Body::new(Box::new(io::Cursor::new(minimal_mp4(2)))))
            .err()
            .expect("multi-track MP4 on unseekable stream must fall back");
        assert!(err.to_string().contains("multi-track MP4"), "{err}");
    }

    #[test]
    fn direct_pcm_fast_path_scales_and_sanitizes_without_sample_buffer() {
        use symphonia::core::audio::{AsAudioBufferRef, AudioBuffer};

        let stereo_spec = SignalSpec::new(48_000, Channels::FRONT_LEFT | Channels::FRONT_RIGHT);
        let mut f32_buf = AudioBuffer::<f32>::new(4, stereo_spec);
        f32_buf.render_reserved(Some(3));
        f32_buf.chan_mut(0).copy_from_slice(&[0.5, f32::NAN, f32::from_bits(1)]);
        f32_buf.chan_mut(1).copy_from_slice(&[-0.25, f32::INFINITY, 1.0]);
        let mut out = Vec::new();
        assert!(try_decode_direct(&Downmix::Stereo, &f32_buf.as_audio_buffer_ref(), &mut out));
        assert_eq!(out, vec![0.5, -0.25, 0.0, 0.0, 0.0, 1.0]);

        let mono_spec = SignalSpec::new(48_000, Channels::FRONT_LEFT);
        let mut s16_buf = AudioBuffer::<i16>::new(2, mono_spec);
        s16_buf.render_reserved(Some(2));
        s16_buf.chan_mut(0).copy_from_slice(&[i16::MIN, 16_384]);
        out.clear();
        assert!(try_decode_direct(&Downmix::Mono, &s16_buf.as_audio_buffer_ref(), &mut out));
        assert_eq!(out, vec![-1.0, -1.0, 0.5, 0.5]);

        let mut s32_buf = AudioBuffer::<i32>::new(2, mono_spec);
        s32_buf.render_reserved(Some(2));
        s32_buf.chan_mut(0).copy_from_slice(&[i32::MIN, 1_073_741_824]);
        out.clear();
        assert!(try_decode_direct(&Downmix::Mono, &s32_buf.as_audio_buffer_ref(), &mut out));
        assert_eq!(out, vec![-1.0, -1.0, 0.5, 0.5]);
    }

    #[test]
    fn non_eof_source_read_errors_map_to_audio_error_io() {
        struct ResettingWav(io::Cursor<Vec<u8>>);
        impl Read for ResettingWav {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                match self.0.read(buf)? {
                    0 => Err(io::Error::new(io::ErrorKind::ConnectionReset, "link dropped")),
                    n => Ok(n),
                }
            }
        }

        let data_len = 48_000u32 * 4;
        let mut header = Vec::with_capacity(70_044);
        header.extend_from_slice(b"RIFF");
        header.extend_from_slice(&(36 + data_len).to_le_bytes());
        header.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0\x02\0");
        header.extend_from_slice(&48_000u32.to_le_bytes());
        header.extend_from_slice(&192_000u32.to_le_bytes());
        header.extend_from_slice(b"\x04\0\x10\0data");
        header.extend_from_slice(&data_len.to_le_bytes());
        let Err(probe_err) =
            Decoder::new(Body::new(Box::new(ResettingWav(io::Cursor::new(header.clone())))))
        else {
            panic!("expected probe read error on short stream");
        };
        assert!(
            matches!(probe_err, AudioError::Io(ref e) if e.kind() == io::ErrorKind::ConnectionReset)
        );

        header.resize(header.len() + 70_000, 0);
        let mut decoder =
            Decoder::new(Body::new(Box::new(ResettingWav(io::Cursor::new(header))))).unwrap();
        let mut out = Vec::new();
        let err = loop {
            match decoder.decode(&mut out) {
                Ok(true) => continue,
                Ok(false) => panic!("expected io error, got EOF"),
                Err(err) => break err,
            }
        };
        assert!(matches!(err, AudioError::Io(ref e) if e.kind() == io::ErrorKind::ConnectionReset));
    }
}
