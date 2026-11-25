//! Decoding a track into samples.
//!
//! Everything the node plays arrives as one stream of bytes from a source, with
//! no file name and no content type it can trust. The decoder therefore has to
//! work out what it has from the bytes themselves, which is what symphonia's
//! probe does for the formats it covers and what the Opus path below does for
//! the one it demuxes but cannot decode.

use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
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

/// A source stream that cannot seek, which is what a pipe is.
///
/// symphonia wants a source that is `Send` and `Sync`, and a body is only the
/// former, so the body sits behind a lock. Nothing contends for it: one decoder
/// owns one body.
struct Unseekable {
    inner: Mutex<Body>,
}

impl Read for Unseekable {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.inner.get_mut() {
            Ok(body) => body.read(buf).map_err(|error| {
                // Symphonia uses UnexpectedEof as its normal end-of-stream
                // sentinel. A source's explicit read failure (for example an
                // exhausted HTTP range retry) must remain a playback failure.
                if error.kind() == io::ErrorKind::UnexpectedEof {
                    io::Error::other(error)
                } else {
                    error
                }
            }),
            Err(_) => Err(io::Error::other("the source is poisoned")),
        }
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

/// RFC 7845 section 4.6 mandates an 80 ms (3,840 samples @ 48 kHz) decoder
/// convergence window before the target sample when seeking in Opus streams.
const OPUS_SEEK_PREROLL_FRAMES: u64 = 3_840;

/// Pre-roll window (~8 AAC-LC packets) decoded before the target seek sample so
/// MDCT overlap-add state is warm when playback resumes.
const AAC_SEEK_PREROLL_FRAMES: u64 = 8_192;

/// Outcome of attempting to read a zero-transcode 20 ms Opus packet.
pub(crate) enum PassthroughStep {
    Packet,
    Eof,
    FallbackToTranscode,
}

/// A decoder that yields interleaved stereo float samples at the source rate.
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
        /// Interleaving buffer kept between packets so steady streams never reallocate.
        samples: SampleBuffer<f32>,
        can_skip_packets: bool,
        primed: bool,
        needs_reset: bool,
    },
    OggOpus(Box<OggDecoder>),
    ContainerOpus(Box<ContainerOpusDecoder>),
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
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        symphonia::default::get_codecs().make(params, &DecoderOptions::default())
    }))
    .map_err(|_| AudioError::Decode("the codec parameters are malformed".into()))?
    .map_err(map_symphonia_error)
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
    fn new(skip: u32, output_gain: i16) -> Result<Self, AudioError> {
        let gain_is_unity = output_gain == 0;
        let gain =
            if gain_is_unity { 1.0 } else { 10.0f32.powf(output_gain as f32 / (20.0 * 256.0)) };
        Ok(Self {
            decoder: opus_decode::Decoder::new().map_err(|e| AudioError::Decode(e.to_string()))?,
            initial_skip: skip,
            skip,
            gain,
            gain_is_unity,
            needs_reset: false,
        })
    }

    #[inline]
    fn can_passthrough(&self) -> bool {
        self.gain_is_unity && self.initial_skip <= 480
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
        if let Some(pkt) = older {
            if let Some(expected) = opus_decode::packet_samples(pkt) {
                let _ = self.decode_packet(pkt, expected, 0, None, 0, k_tail_out)?;
                k_tail_out.clear();
            }
        }
        if let Some(pkt) = latest {
            if let Some(expected) = opus_decode::packet_samples(pkt) {
                let _ = self.decode_packet(pkt, expected, 0, None, 0, k_tail_out)?;
            }
        }
        Ok(())
    }

    #[inline]
    fn decode_packet(
        &mut self,
        packet: &[u8],
        expected: usize,
        pkt_trim_start: usize,
        max_end: Option<usize>,
        pkt_trim_end: usize,
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
        let limit = max_end.map_or(frames, |keep| keep.min(frames));
        let end = limit.saturating_sub(pkt_trim_end);
        let start = drop_pre.max(pkt_trim_start).min(end);
        if start == end {
            out.truncate(base);
            return Ok(false);
        }
        out.truncate(base + end * 2);
        if start > 0 {
            out.drain(base..base + start * 2);
        }
        if self.gain == 1.0 {
            for s in &mut out[base..] {
                *s = crate::dsp::clean_f32(*s);
            }
        } else {
            let gain = self.gain;
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
    let channels = param_channels(params).map(|c| c.count()).unwrap_or(2);
    if !(1..=2).contains(&channels) {
        return Err(AudioError::Decode("unsupported Opus channel count".into()));
    }
    let Some(extra) = params.extra_data.as_deref() else {
        return Err(AudioError::Decode("missing Opus header in container".into()));
    };
    let mut skip = params.delay.unwrap_or(0);
    let output_gain;
    if let Some(rest) = extra.strip_prefix(b"OpusHead") {
        let &[version, ch, s0, s1, _, _, _, _, g0, g1, family, ..] = rest else {
            return Err(AudioError::Decode("unsupported OpusHead in container".into()));
        };
        if !(1..=2).contains(&ch) || family != 0 {
            return Err(AudioError::Decode("unsupported OpusHead in container".into()));
        }
        match version {
            // Symphonia's `isomp4` reader prepends `b"OpusHead"` onto the raw
            // big-endian ISO-BMFF `dOps` (`OpusSpecificBox`, Version = 0) payload.
            0 => {
                if skip == 0 {
                    skip = u32::from(u16::from_be_bytes([s0, s1]));
                }
                output_gain = i16::from_be_bytes([g0, g1]);
            }
            // RFC 7845 `OpusHead` (Matroska/WebM CodecPrivate): little-endian, version 1..=15.
            1..=15 => {
                if skip == 0 {
                    skip = u32::from(u16::from_le_bytes([s0, s1]));
                }
                output_gain = i16::from_le_bytes([g0, g1]);
            }
            _ => {
                return Err(AudioError::Decode("unsupported OpusHead in container".into()));
            }
        }
    } else if let &[0, ch, s0, s1, _, _, _, _, g0, g1, family, ..] = extra {
        // ISO-MP4 `dOps` OpusSpecificBox payload: version=0, OutputChannelCount,
        // PreSkip (BE u16), InputSampleRate (BE u32), OutputGain (BE i16), MappingFamily.
        if !(1..=2).contains(&ch) || family != 0 {
            return Err(AudioError::Decode("unsupported dOps channel mapping".into()));
        }
        if skip == 0 {
            skip = u32::from(u16::from_be_bytes([s0, s1]));
        }
        output_gain = i16::from_be_bytes([g0, g1]);
    } else {
        return Err(AudioError::Decode("unsupported Opus header in container".into()));
    }
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

    fn decode_with_skip(&mut self, skip: &mut u64, out: &mut Vec<f32>) -> Result<bool, AudioError> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(false);
                }
                Err(SymphoniaError::ResetRequired) => {
                    let track = select_audio_track(self.format.as_ref())?;
                    if track.codec_params.codec != CODEC_TYPE_OPUS {
                        return Err(AudioError::Decode("chained container codec changed".into()));
                    }
                    let (pre_skip, output_gain) = parse_container_opus_params(&track.codec_params)?;
                    let new_id = track.id;
                    self.opus = OpusStream::new(pre_skip, output_gain)?;
                    self.track_id = new_id;
                    continue;
                }
                Err(e) => return Err(map_symphonia_error(e)),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let buf = packet.buf();
            let Some(expected) = opus_decode::packet_samples(buf) else {
                continue;
            };
            let trim_start = packet.trim_start() as usize;
            let trim_end = packet.trim_end() as usize;
            let drop_pre = (self.opus.skip as usize).min(expected);
            let end = expected.saturating_sub(trim_end);
            let start = drop_pre.max(trim_start).min(end);
            let usable = end - start;
            if *skip > (usable as u64).saturating_add(OPUS_SEEK_PREROLL_FRAMES) {
                self.opus.skip -= drop_pre as u32;
                *skip -= usable as u64;
                self.opus.needs_reset = true;
                continue;
            }
            if self.opus.decode_packet(buf, expected, trim_start, None, trim_end, out)? {
                return Ok(true);
            }
        }
    }

    fn try_passthrough_next(
        &mut self,
        packet_index: u64,
        out_packet: &mut Vec<u8>,
        older_hist: Option<&[u8]>,
        latest_hist: Option<&[u8]>,
        k_tail_out: &mut Vec<f32>,
        next_pcm_out: &mut Vec<f32>,
    ) -> Result<PassthroughStep, AudioError> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(PassthroughStep::Eof);
                }
                Err(SymphoniaError::ResetRequired) => {
                    let track = select_audio_track(self.format.as_ref())?;
                    if track.codec_params.codec != CODEC_TYPE_OPUS {
                        return Err(AudioError::Decode("chained container codec changed".into()));
                    }
                    let (pre_skip, output_gain) = parse_container_opus_params(&track.codec_params)?;
                    let new_id = track.id;
                    self.opus = OpusStream::new(pre_skip, output_gain)?;
                    self.track_id = new_id;
                    if packet_index > 0 {
                        k_tail_out.clear();
                        next_pcm_out.clear();
                        let mut zero = 0;
                        let _ = self.decode_with_skip(&mut zero, next_pcm_out)?;
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
            let Some(expected) = opus_decode::packet_samples(buf) else {
                continue;
            };
            let trim_start = packet.trim_start() as usize;
            let trim_end = packet.trim_end() as usize;
            let trim_ok = trim_end == 0
                && if packet_index == 0 { trim_start <= 480 } else { trim_start == 0 };
            if self.opus.can_passthrough() && trim_ok && opus_decode::is_valid_20ms_packet(buf) {
                let drop_pre = (self.opus.skip as usize).min(expected);
                self.opus.skip -= drop_pre as u32;
                out_packet.clear();
                out_packet.extend_from_slice(buf);
                return Ok(PassthroughStep::Packet);
            }
            next_pcm_out.clear();
            if packet_index > 0 {
                self.opus.warm_from_history(older_hist, latest_hist, k_tail_out)?;
                let _ = self.opus.decode_packet(
                    buf,
                    expected,
                    trim_start,
                    None,
                    trim_end,
                    next_pcm_out,
                )?;
            } else {
                k_tail_out.clear();
                let _ = self.opus.decode_packet(
                    buf,
                    expected,
                    trim_start,
                    None,
                    trim_end,
                    next_pcm_out,
                )?;
            }
            return Ok(PassthroughStep::FallbackToTranscode);
        }
    }
}

struct OggPacket {
    offset: u32,
    length: u32,
    frames: u16,
    keep: u16,
}

/// Stage compressed packets for one page so RFC 7845 end trimming can cut
/// across any of its packets. A page has at most 255 completed packets and
/// 65,025 payload bytes, plus the first packet's bounded continuation history.
/// PCM storage remains one packet (at most 120 ms), not an entire decoded page.
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
        // Validate the initial granule and establish cropping before playback
        // starts, while the caller can still select its FFmpeg fallback.
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
                // The reader must delimit every page with completed packets;
                // this guard also keeps accidental metadata loss bounded.
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
                    let pre_skip = self.reader.pre_skip();
                    let output_gain = self.reader.output_gain();
                    self.opus.initial_skip = pre_skip;
                    self.opus.skip = pre_skip;
                    self.opus.gain_is_unity = output_gain == 0;
                    self.opus.gain = if output_gain == 0 {
                        1.0
                    } else {
                        10.0f32.powf(output_gain as f32 / (20.0 * 256.0))
                    };
                    if end_of_stream {
                        // A single EOS page can trim from a zero origin, or
                        // have a nonzero origin when its granule exceeds its
                        // complete decoded duration (RFC 7845 section 4.5).
                        granule.min(total)
                    } else {
                        if granule < total {
                            return Err(AudioError::Decode("invalid initial ogg granule".into()));
                        }
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

    fn decode_with_skip(&mut self, skip: &mut u64, out: &mut Vec<f32>) -> Result<bool, AudioError> {
        loop {
            if self.packets.is_empty() && !self.read_page()? {
                return Ok(false);
            }
            let packet = self.packets.pop_front().expect("a nonempty audio page was loaded");
            if self.ended && packet.keep == 0 {
                continue;
            }
            let frames = usize::from(packet.frames);
            let keep = usize::from(packet.keep).min(frames);
            let drop_pre = (self.opus.skip as usize).min(frames);
            let usable = keep.saturating_sub(drop_pre);
            if *skip > (usable as u64).saturating_add(OPUS_SEEK_PREROLL_FRAMES) {
                self.opus.skip -= drop_pre as u32;
                *skip -= usable as u64;
                self.opus.needs_reset = true;
                continue;
            }
            let start_byte = packet.offset as usize;
            let end_byte = start_byte + packet.length as usize;
            if self.opus.decode_packet(
                &self.page[start_byte..end_byte],
                frames,
                0,
                Some(keep),
                0,
                out,
            )? {
                return Ok(true);
            }
        }
    }

    fn try_passthrough_next(
        &mut self,
        packet_index: u64,
        out_packet: &mut Vec<u8>,
        older_hist: Option<&[u8]>,
        latest_hist: Option<&[u8]>,
        k_tail_out: &mut Vec<f32>,
        next_pcm_out: &mut Vec<f32>,
    ) -> Result<PassthroughStep, AudioError> {
        loop {
            if self.packets.is_empty() && !self.read_page()? {
                return Ok(PassthroughStep::Eof);
            }
            let packet = self.packets.pop_front().expect("a nonempty audio page was loaded");
            if self.ended && packet.keep == 0 {
                continue;
            }
            let frames = usize::from(packet.frames);
            let keep = usize::from(packet.keep).min(frames);
            let start_byte = packet.offset as usize;
            let end_byte = start_byte + packet.length as usize;
            let raw = &self.page[start_byte..end_byte];
            if self.opus.can_passthrough()
                && keep == frames
                && opus_decode::is_valid_20ms_packet(raw)
            {
                let drop_pre = (self.opus.skip as usize).min(frames);
                self.opus.skip -= drop_pre as u32;
                out_packet.clear();
                out_packet.extend_from_slice(raw);
                return Ok(PassthroughStep::Packet);
            }
            next_pcm_out.clear();
            if packet_index > 0 {
                self.opus.warm_from_history(older_hist, latest_hist, k_tail_out)?;
                let _ = self.opus.decode_packet(raw, frames, 0, Some(keep), 0, next_pcm_out)?;
            } else {
                k_tail_out.clear();
                let _ = self.opus.decode_packet(raw, frames, 0, Some(keep), 0, next_pcm_out)?;
            }
            return Ok(PassthroughStep::FallbackToTranscode);
        }
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

/// Validates ISO/IEC 14496-3 AudioSpecificConfig for Symphonia's AAC-LC-only
/// decoder, declining SBR/PS so `PureFrames::start` falls back to FFmpeg before
/// playback begins and recovering the true sample rate on 88.2/96 kHz M4As.
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
    fn read_bits(extra: &[u8], pos: &mut usize, count: usize) -> Option<u32> {
        if *pos + count > extra.len() * 8 {
            return None;
        }
        let mut val = 0u32;
        for _ in 0..count {
            let bit = (extra[*pos >> 3] >> (7 - (*pos & 7))) & 1;
            val = (val << 1) | u32::from(bit);
            *pos += 1;
        }
        Some(val)
    }
    let mut aot = read_bits(extra, &mut pos, 5)
        .ok_or_else(|| AudioError::Decode("truncated AAC config".into()))?;
    if aot == 31 {
        aot = 32
            + read_bits(extra, &mut pos, 6)
                .ok_or_else(|| AudioError::Decode("truncated AAC config".into()))?;
    }
    if aot == 5 || aot == 29 {
        return Err(AudioError::Decode("HE-AAC (SBR/PS) requires ffmpeg".into()));
    }
    let freq_idx = read_bits(extra, &mut pos, 4)
        .ok_or_else(|| AudioError::Decode("truncated AAC config".into()))?;
    let asc_rate = *AAC_RATES
        .get(freq_idx as usize)
        .ok_or_else(|| AudioError::Decode("unsupported AAC sample rate".into()))?;
    let chan_cfg = read_bits(extra, &mut pos, 4)
        .ok_or_else(|| AudioError::Decode("truncated AAC config".into()))?;
    if aot == 2 && chan_cfg != 0 {
        let _short = read_bits(extra, &mut pos, 1);
        if read_bits(extra, &mut pos, 1) == Some(1) {
            let _ = read_bits(extra, &mut pos, 14);
        }
        if read_bits(extra, &mut pos, 1) == Some(0)
            && extra.len() * 8 - pos >= 16
            && read_bits(extra, &mut pos, 11) == Some(0x2b7)
        {
            let mut ext_aot = read_bits(extra, &mut pos, 5).unwrap_or(0);
            if ext_aot == 31 {
                ext_aot = 32 + read_bits(extra, &mut pos, 6).unwrap_or(0);
            }
            if (ext_aot == 5 || ext_aot == 29) && read_bits(extra, &mut pos, 1) == Some(1) {
                return Err(AudioError::Decode("HE-AAC (SBR/PS) requires ffmpeg".into()));
            }
        }
    }
    if container_rate == Some(asc_rate.saturating_mul(2)) {
        return Err(AudioError::Decode("implicit HE-AAC (SBR) requires ffmpeg".into()));
    }
    Ok(Some(asc_rate))
}

impl Decoder {
    /// Opens a decoder over a body, working out the container from its first
    /// bytes.
    pub fn new(mut body: Body) -> Result<Self, AudioError> {
        body.peek(512)?;

        // Opus in Ogg is demuxed here rather than by symphonia, because
        // symphonia has no Opus decoder and libopus does.
        let container = detect(body.head());
        if container == Container::OggOpus {
            return Ok(Decoder {
                inner: Inner::OggOpus(Box::new(OggDecoder::new(body)?)),
                rate: 48_000,
            });
        }

        let source = Unseekable { inner: Mutex::new(body) };
        let stream = MediaSourceStream::new(Box::new(source), Default::default());
        // symphonia's demuxers panic on some malformed headers (a WAV with a
        // zero sample rate builds a zero TimeBase); untrusted bytes must decline.
        let probed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            symphonia::default::get_probe().format(
                &Hint::new(),
                stream,
                &FormatOptions { enable_gapless: true, ..Default::default() },
                &MetadataOptions::default(),
            )
        }))
        .map_err(|_| AudioError::Decode("the stream header is malformed".into()))?
        .map_err(map_symphonia_error)?;
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
            return Ok(Decoder { inner: Inner::ContainerOpus(Box::new(opus)), rate: 48_000 });
        }
        let asc_rate = if track.codec_params.codec == CODEC_TYPE_AAC {
            parse_aac_lc_rate(
                track.codec_params.extra_data.as_deref(),
                track.codec_params.sample_rate,
            )?
        } else {
            None
        };
        let rate = asc_rate.or(track.codec_params.sample_rate).unwrap_or(48_000);
        if rate == 0 {
            // A zero rate would make the resampler expand every frame ~10^6 times.
            return Err(AudioError::Decode("the stream has a zero sample rate".into()));
        }
        let channels = param_channels(&track.codec_params)
            .unwrap_or(Channels::FRONT_LEFT | Channels::FRONT_RIGHT);
        if channels.count() == 0 {
            return Err(AudioError::Decode("the stream has zero audio channels".into()));
        }
        let can_skip_packets = track.codec_params.codec == CODEC_TYPE_AAC
            && track.codec_params.time_base == Some(symphonia::core::units::TimeBase::new(1, rate));
        let decoder = make_codec_decoder(&track.codec_params)?;
        let spec = SignalSpec::new(rate, channels);
        Ok(Decoder {
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

    /// The source sample rate, before any resampling.
    #[inline]
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Whether the source is 48 kHz Opus with unity container gain and bounded
    /// encoder lookahead, allowing lossless zero-transcode packet passthrough
    /// whenever filters and volume are neutral.
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
        packet_index: u64,
        out_packet: &mut Vec<u8>,
        older_hist: Option<&[u8]>,
        latest_hist: Option<&[u8]>,
        k_tail_out: &mut Vec<f32>,
        next_pcm_out: &mut Vec<f32>,
    ) -> Result<PassthroughStep, AudioError> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match &mut self.inner {
            Inner::OggOpus(d) => d.try_passthrough_next(
                packet_index,
                out_packet,
                older_hist,
                latest_hist,
                k_tail_out,
                next_pcm_out,
            ),
            Inner::ContainerOpus(d) => d.try_passthrough_next(
                packet_index,
                out_packet,
                older_hist,
                latest_hist,
                k_tail_out,
                next_pcm_out,
            ),
            Inner::Symphonia { .. } => Ok(PassthroughStep::FallbackToTranscode),
        }))
        .unwrap_or_else(|_| Err(AudioError::Decode("the decoder panicked on the stream".into())))
    }

    /// Decodes the next chunk, appending interleaved stereo samples to `out`.
    ///
    /// Returns whether anything was appended. A damaged packet is skipped rather
    /// than ending the track, which is what a listener expects from a stream
    /// that arrived over a lossy connection.
    pub fn decode(&mut self, out: &mut Vec<f32>) -> Result<bool, AudioError> {
        let mut zero = 0;
        self.decode_with_skip(&mut zero, out)
    }

    /// Decodes the next chunk while skipping compressed demuxer packets that lie
    /// strictly before the codec's convergence pre-roll window of `*skip` frames.
    pub fn decode_with_skip(
        &mut self,
        skip: &mut u64,
        out: &mut Vec<f32>,
    ) -> Result<bool, AudioError> {
        // As in `new`: a symphonia panic on malformed packets ends the track
        // with an error instead of unwinding through the player.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.decode_next_with_skip(skip, out)
        }))
        .unwrap_or_else(|_| Err(AudioError::Decode("the decoder panicked on the stream".into())))
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
                        return Ok(false)
                    }
                    Err(SymphoniaError::ResetRequired) => {
                        let track = select_audio_track(format.as_ref())?;
                        let asc_rate = if track.codec_params.codec == CODEC_TYPE_AAC {
                            parse_aac_lc_rate(
                                track.codec_params.extra_data.as_deref(),
                                track.codec_params.sample_rate,
                            )?
                        } else {
                            None
                        };
                        let new_rate =
                            asc_rate.or(track.codec_params.sample_rate).unwrap_or(self.rate);
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
                        *can_skip_packets = track.codec_params.codec == CODEC_TYPE_AAC
                            && track.codec_params.time_base
                                == Some(symphonia::core::units::TimeBase::new(1, self.rate));
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
                    // A packet that does not decode is a packet lost.
                    Err(SymphoniaError::DecodeError(_)) => continue,
                    Err(e) => return Err(map_symphonia_error(e)),
                };
                // Vorbis primes its overlap state with a packet that produces
                // no PCM. Symphonia's interleaver indexes each channel even
                // for zero frames, which would panic on our empty buffer.
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
                    // Room for twice the packet means a stream of even packets
                    // reallocates once rather than on every step up.
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
    // SAFETY: `out.reserve(frames * 2)` reserves at least `frames` stereo pairs,
    // and every `slot[0]` and `slot[1]` in `0..frames` was initialized above.
    unsafe {
        out.set_len(base + frames * 2);
    }
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
    // SAFETY: `out.reserve(frames * 2)` reserves at least `frames` stereo pairs,
    // and every `slot[0]` and `slot[1]` in `0..frames` was initialized above.
    unsafe {
        out.set_len(base + frames * 2);
    }
}

fn try_decode_direct(mixer: &Downmix, audio: &AudioBufferRef<'_>, out: &mut Vec<f32>) -> bool {
    const S16_SCALE: f32 = 1.0 / 32_768.0;
    const S32_SCALE: f32 = 1.0 / 2_147_483_648.0;
    match (mixer, audio) {
        (Downmix::Stereo, AudioBufferRef::F32(buf)) => {
            append_stereo(buf.chan(0), buf.chan(1), out, crate::dsp::clean_f32);
            true
        }
        (Downmix::Stereo, AudioBufferRef::S16(buf)) => {
            append_stereo(buf.chan(0), buf.chan(1), out, |s| s as f32 * S16_SCALE);
            true
        }
        (Downmix::Stereo, AudioBufferRef::S32(buf)) => {
            append_stereo(buf.chan(0), buf.chan(1), out, |s| s as f32 * S32_SCALE);
            true
        }
        (Downmix::Mono, AudioBufferRef::F32(buf)) => {
            append_mono(buf.chan(0), out, crate::dsp::clean_f32);
            true
        }
        (Downmix::Mono, AudioBufferRef::S16(buf)) => {
            append_mono(buf.chan(0), out, |s| s as f32 * S16_SCALE);
            true
        }
        (Downmix::Mono, AudioBufferRef::S32(buf)) => {
            append_mono(buf.chan(0), out, |s| s as f32 * S32_SCALE);
            true
        }
        _ => false,
    }
}

/// Copies single-channel content to both sides at unity gain, regardless of its
/// position label. Multichannel content uses conventional centre/surround
/// weights, normalised per output to keep full-scale input from clipping.
/// This is not a bit-identical FFmpeg rematrix.
enum Downmix {
    Empty,
    Mono,
    Stereo,
    FallbackMono { count: usize, inv_count: f32 },
    Matrix { count: usize, weights: Vec<[f32; 2]>, inv_left: f32, inv_right: f32 },
}

impl Downmix {
    /// Build coefficients once per channel layout, instead of allocating and
    /// normalising vectors for every decoded packet.
    fn new(channels: Channels) -> Self {
        let count = channels.count();
        match count {
            0 => return Self::Empty,
            1 => return Self::Mono,
            2 if channels == (Channels::FRONT_LEFT | Channels::FRONT_RIGHT) => return Self::Stereo,
            _ => {}
        }
        let mut weights = Vec::with_capacity(count);
        let mut left_sum = 0.0f32;
        let mut right_sum = 0.0f32;
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
            Self::Mono => {
                out.reserve(samples.len() * 2);
                for &sample in samples {
                    let s = crate::dsp::clean_f32(sample);
                    out.extend_from_slice(&[s, s]);
                }
            }
            Self::Stereo => {
                let pairs = samples.as_chunks::<2>().0;
                out.reserve(pairs.len() * 2);
                for &[l, r] in pairs {
                    out.extend_from_slice(&[crate::dsp::clean_f32(l), crate::dsp::clean_f32(r)]);
                }
            }
            Self::FallbackMono { count, inv_count } => {
                let chunks = samples.chunks_exact(*count);
                out.reserve(chunks.len() * 2);
                for frame in chunks {
                    let mono =
                        frame.iter().copied().map(crate::dsp::clean_f32).sum::<f32>() * inv_count;
                    let m = crate::dsp::clean_f32(mono);
                    out.extend_from_slice(&[m, m]);
                }
            }
            Self::Matrix { count, weights, inv_left, inv_right } => {
                let chunks = samples.chunks_exact(*count);
                out.reserve(chunks.len() * 2);
                for frame in chunks {
                    let mut l = 0.0f32;
                    let mut r = 0.0f32;
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

    fn spec(channels: Channels) -> SignalSpec {
        SignalSpec::new(48_000, channels)
    }

    fn downmix(samples: &[f32], spec: &SignalSpec, out: &mut Vec<f32>) {
        Downmix::new(spec.channels).process(samples, out);
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
            let mut out = Vec::new();
            downmix(&[1.0, 0.5], &spec(channel), &mut out);
            assert_eq!(out, vec![1.0, 1.0, 0.5, 0.5], "channel={channel:?}");
        }
    }

    #[test]
    fn stereo_is_passed_through() {
        let mut out = Vec::new();
        downmix(
            &[1.0, 0.0, 0.0, 1.0],
            &spec(Channels::FRONT_LEFT | Channels::FRONT_RIGHT),
            &mut out,
        );
        assert_eq!(out, vec![1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn a_nonstereo_two_channel_layout_is_not_passed_through_as_left_right() {
        let mut out = Vec::new();
        downmix(&[1.0, 0.75], &spec(Channels::FRONT_CENTRE | Channels::LFE1), &mut out);
        assert!((out[0] - 0.707).abs() < 1e-6, "out={out:?}");
        assert!((out[1] - 0.707).abs() < 1e-6, "out={out:?}");
    }

    #[test]
    fn left_and_right_centre_channels_are_not_silenced() {
        let mut out = Vec::new();
        downmix(
            &[1.0, 0.75],
            &spec(Channels::FRONT_LEFT_CENTRE | Channels::FRONT_RIGHT_CENTRE),
            &mut out,
        );
        assert!((out[0] - 1.0).abs() < 1e-6, "out={out:?}");
        assert!((out[1] - 0.75).abs() < 1e-6, "out={out:?}");
    }

    #[test]
    fn top_front_left_and_right_channels_are_not_silenced() {
        let mut out = Vec::new();
        downmix(
            &[1.0, 0.75],
            &spec(Channels::TOP_FRONT_LEFT | Channels::TOP_FRONT_RIGHT),
            &mut out,
        );
        assert!((out[0] - 1.0).abs() < 1e-6, "out={out:?}");
        assert!((out[1] - 0.75).abs() < 1e-6, "out={out:?}");
    }

    #[test]
    fn multichannel_overhead_centre_uses_the_half_level_stereo_matrix() {
        let mut out = Vec::new();
        downmix(&[0.0, 1.0], &spec(Channels::TOP_CENTRE | Channels::LFE1), &mut out);
        assert_eq!(out, vec![0.5, 0.5]);
    }

    #[test]
    fn multichannel_rear_centre_is_half_level_but_front_height_centre_is_not() {
        let mut rear = Vec::new();
        downmix(&[0.0, 1.0], &spec(Channels::LFE1 | Channels::REAR_CENTRE), &mut rear);
        assert_eq!(rear, vec![0.5, 0.5]);
        let mut front = Vec::new();
        downmix(&[0.0, 1.0], &spec(Channels::LFE1 | Channels::TOP_FRONT_CENTRE), &mut front);
        assert_eq!(front, vec![0.707, 0.707]);
    }

    #[test]
    fn a_layout_without_known_positions_falls_back_to_audible_mono() {
        let mut out = Vec::new();
        downmix(&[1.0, 0.5], &spec(Channels::LFE1 | Channels::LFE2), &mut out);
        assert!((out[0] - 0.75).abs() < 1e-6, "out={out:?}");
        assert!((out[1] - 0.75).abs() < 1e-6, "out={out:?}");
    }

    #[test]
    fn surround_is_folded_with_the_centre_in_both_sides() {
        // Front left, front right, centre, low frequency.
        let layout =
            Channels::FRONT_LEFT | Channels::FRONT_RIGHT | Channels::FRONT_CENTRE | Channels::LFE1;
        let frame = [1.0f32, 0.0, 1.0, 1.0];
        let mut out = Vec::new();
        downmix(&frame, &spec(layout), &mut out);
        // The centre is shared, the low frequency channel is dropped, and the
        // result is normalised so it cannot exceed full scale.
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
        let mut samples = Vec::new();
        for _ in 0..4 {
            samples.extend_from_slice(&[1.0f32, 1.0, 1.0, 1.0, 1.0]);
        }
        let mut out = Vec::new();
        downmix(&samples, &spec(layout), &mut out);
        for sample in out {
            assert!(sample <= 1.0, "{sample} exceeds full scale");
        }
    }

    #[test]
    fn symphonia_isomp4_dops_big_endian_vs_rfc7845_little_endian() {
        // Symphonia 0.5.5 `isomp4` prepends `b"OpusHead"` onto raw big-endian ISO-BMFF `dOps`
        // (`Version = 0`, PreSkip = 0x0138 = 312, OutputGain = 0x0100 = +1 dB in Q7.8).
        let mut mp4_extra = b"OpusHead".to_vec();
        mp4_extra.extend_from_slice(&[0, 2, 0x01, 0x38, 0x00, 0x00, 0xbb, 0x80, 0x01, 0x00, 0]);
        let mut mp4_params = CodecParameters::new();
        mp4_params.with_extra_data(mp4_extra.into_boxed_slice());
        let (skip_be, gain_be) = parse_container_opus_params(&mp4_params).unwrap();
        assert_eq!(skip_be, 312);
        assert_eq!(gain_be, 256);

        // RFC 7845 `OpusHead` (`Version = 1`, little-endian PreSkip = 0x0138, OutputGain = 0x0100).
        let mut webm_extra = b"OpusHead".to_vec();
        webm_extra.extend_from_slice(&[1, 2, 0x38, 0x01, 0x80, 0xbb, 0x00, 0x00, 0x00, 0x01, 0]);
        let mut webm_params = CodecParameters::new();
        webm_params.with_extra_data(webm_extra.into_boxed_slice());
        let (skip_le, gain_le) = parse_container_opus_params(&webm_params).unwrap();
        assert_eq!(skip_le, 312);
        assert_eq!(gain_le, 256);
    }

    #[test]
    fn container_opus_params_require_header_and_honor_channel_layout() {
        use symphonia::core::audio::Layout;

        let headerless = CodecParameters::new();
        assert!(parse_container_opus_params(&headerless).is_err());

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
            let mut hdlr = vec![0u8; 25];
            hdlr[8..12].copy_from_slice(b"soun");
            let mut sowt = vec![0u8; 28];
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
            stbl_body.extend_from_slice(&mp4_box(b"stts", &empty_table));
            stbl_body.extend_from_slice(&mp4_box(b"stsc", &empty_table));
            stbl_body.extend_from_slice(&mp4_box(b"stsz", &stsz));
            stbl_body.extend_from_slice(&mp4_box(b"stco", &empty_table));
            let minf = mp4_box(b"minf", &mp4_box(b"stbl", &stbl_body));
            let mut mdia_body = mp4_box(b"mdhd", &mdhd);
            mdia_body.extend_from_slice(&mp4_box(b"hdlr", &hdlr));
            mdia_body.extend_from_slice(&minf);
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
        struct ResettingWav {
            header: io::Cursor<Vec<u8>>,
        }
        impl Read for ResettingWav {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let n = self.header.read(buf)?;
                if n > 0 {
                    return Ok(n);
                }
                Err(io::Error::new(io::ErrorKind::ConnectionReset, "link dropped"))
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
        let probe_err = match Decoder::new(Body::new(Box::new(ResettingWav {
            header: io::Cursor::new(header.clone()),
        }))) {
            Err(err) => err,
            Ok(_) => panic!("expected probe read error on short stream"),
        };
        assert!(
            matches!(probe_err, AudioError::Io(ref e) if e.kind() == io::ErrorKind::ConnectionReset)
        );

        header.resize(header.len() + 70_000, 0);
        let mut decoder =
            Decoder::new(Body::new(Box::new(ResettingWav { header: io::Cursor::new(header) })))
                .unwrap();
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
