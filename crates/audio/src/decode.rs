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

use symphonia::core::audio::{Channels, SampleBuffer, SignalSpec};
use symphonia::core::codecs::{Decoder as SymphoniaDecoder, DecoderOptions, CODEC_TYPE_OPUS};
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
    },
    OggOpus(Box<OggDecoder>),
    ContainerOpus(Box<ContainerOpusDecoder>),
}

#[inline]
fn finite_or_zero(s: f32) -> f32 {
    if s.is_finite() {
        s
    } else {
        0.0
    }
}

#[inline]
fn extend_scaled_finite(samples: &[f32], gain: f32, out: &mut Vec<f32>) {
    if gain == 1.0 {
        out.extend(samples.iter().copied().map(finite_or_zero));
    } else {
        out.extend(samples.iter().map(|&s| finite_or_zero(s * gain)));
    }
}

struct ContainerOpusDecoder {
    format: Box<dyn FormatReader>,
    decoder: opus_decode::Decoder,
    track_id: u32,
    skip: u32,
    gain: f32,
    scratch: Vec<f32>,
}

fn parse_container_opus_params(
    params: &symphonia::core::codecs::CodecParameters,
) -> Result<(u32, f32), AudioError> {
    let channels = params.channels.map(|c| c.count()).unwrap_or(2);
    if !(1..=2).contains(&channels) {
        return Err(AudioError::Decode("unsupported Opus channel count".into()));
    }
    let mut skip = params.delay.unwrap_or(0);
    let mut output_gain = 0i16;
    if let Some(extra) = params.extra_data.as_deref() {
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
        }
    }
    let gain = 10.0f32.powf(output_gain as f32 / (20.0 * 256.0));
    Ok((skip, gain))
}

impl ContainerOpusDecoder {
    fn from_track(
        format: Box<dyn FormatReader>,
        track_id: u32,
        params: &symphonia::core::codecs::CodecParameters,
    ) -> Result<Self, AudioError> {
        let (skip, gain) = parse_container_opus_params(params)?;
        Ok(Self {
            format,
            decoder: opus_decode::Decoder::new().map_err(|e| AudioError::Decode(e.to_string()))?,
            track_id,
            skip,
            gain,
            scratch: Vec::new(),
        })
    }

    fn decode(&mut self, out: &mut Vec<f32>) -> Result<bool, AudioError> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(false);
                }
                Err(SymphoniaError::ResetRequired) => continue,
                Err(e) => return Err(AudioError::Decode(e.to_string())),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let buf = packet.buf();
            let Some(expected) = opus_decode::packet_samples(buf) else {
                continue;
            };
            let frames = match self.decoder.decode_float(buf, &mut self.scratch) {
                Ok(frames) => frames,
                Err(_) => self
                    .decoder
                    .conceal_float(expected, &mut self.scratch)
                    .map_err(|e| AudioError::Decode(e.to_string()))?,
            };
            let pkt_trim_start = packet.trim_start() as usize;
            let drop_pre = (self.skip as usize).min(frames);
            self.skip -= drop_pre as u32;
            let start = drop_pre.max(pkt_trim_start).min(frames);
            let end = frames.saturating_sub(packet.trim_end() as usize).max(start);
            if end == start {
                continue;
            }
            extend_scaled_finite(&self.scratch[start * 2..end * 2], self.gain, out);
            return Ok(true);
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
    decoder: opus_decode::Decoder,
    skip: u32,
    gain: f32,
    page: Vec<u8>,
    packets: VecDeque<OggPacket>,
    previous_granule: Option<u64>,
    ended: bool,
    scratch: Vec<f32>,
}

impl OggDecoder {
    fn new(body: Body) -> Result<Self, AudioError> {
        let mut decoder = Self {
            reader: OggOpusReader::new(body),
            decoder: opus_decode::Decoder::new().map_err(|e| AudioError::Decode(e.to_string()))?,
            skip: 0,
            gain: 1.0,
            page: Vec::new(),
            packets: VecDeque::new(),
            previous_granule: None,
            ended: false,
            scratch: Vec::new(),
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
                    self.skip = self.reader.pre_skip();
                    self.gain = 10.0f32.powf(self.reader.output_gain() as f32 / (20.0 * 256.0));
                    if end_of_stream {
                        if granule < u64::from(self.skip) {
                            return Err(AudioError::Decode("ogg end precedes pre-skip".into()));
                        }
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

    fn decode(&mut self, out: &mut Vec<f32>) -> Result<bool, AudioError> {
        loop {
            if self.packets.is_empty() && !self.read_page()? {
                return Ok(false);
            }
            let packet = self.packets.pop_front().expect("a nonempty audio page was loaded");
            if self.ended && packet.keep == 0 {
                continue;
            }
            let pkt_frames = usize::from(packet.frames);
            let drop = (self.skip as usize).min(pkt_frames);
            self.skip -= drop as u32;
            let start_byte = packet.offset as usize;
            let end_byte = start_byte + packet.length as usize;
            let frames = match self
                .decoder
                .decode_float(&self.page[start_byte..end_byte], &mut self.scratch)
            {
                Ok(frames) => frames,
                // The TOC gave a valid duration: conceal the damaged packet so it
                // still occupies its exact duration on the page's clock.
                Err(_) => self
                    .decoder
                    .conceal_float(pkt_frames, &mut self.scratch)
                    .map_err(|e| AudioError::Decode(e.to_string()))?,
            };
            let end = usize::from(packet.keep).min(frames);
            let start = drop.min(end);
            if start == end {
                continue;
            }
            extend_scaled_finite(&self.scratch[start * 2..end * 2], self.gain, out);
            return Ok(true);
        }
    }
}

impl Decoder {
    /// Opens a decoder over a body, working out the container from its first
    /// bytes.
    pub fn new(mut body: Body) -> Result<Self, AudioError> {
        body.peek(512)?;

        // Opus in Ogg is demuxed here rather than by symphonia, because
        // symphonia has no Opus decoder and libopus does.
        if detect(body.head()) == Container::OggOpus {
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
        .map_err(|e| AudioError::Decode(e.to_string()))?;
        let format = probed.format;
        let track = format
            .default_track()
            .ok_or_else(|| AudioError::Decode("the stream holds no playable track".into()))?;
        let track_id = track.id;
        if track.codec_params.codec == CODEC_TYPE_OPUS {
            let params = track.codec_params.clone();
            let opus = ContainerOpusDecoder::from_track(format, track_id, &params)?;
            return Ok(Decoder { inner: Inner::ContainerOpus(Box::new(opus)), rate: 48_000 });
        }
        let rate = track.codec_params.sample_rate.unwrap_or(48_000);
        if rate == 0 {
            // A zero rate would make the resampler expand every frame ~10^6 times.
            return Err(AudioError::Decode("the stream has a zero sample rate".into()));
        }
        let channels =
            track.codec_params.channels.unwrap_or(Channels::FRONT_LEFT | Channels::FRONT_RIGHT);
        if channels.count() == 0 {
            return Err(AudioError::Decode("the stream has zero audio channels".into()));
        }
        let decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .map_err(|e| AudioError::Decode(e.to_string()))?;
        let spec = SignalSpec::new(rate, channels);
        Ok(Decoder {
            inner: Inner::Symphonia {
                format,
                decoder,
                track_id,
                spec,
                mixer: Downmix::new(channels),
                samples: SampleBuffer::new(0, spec),
            },
            rate,
        })
    }

    /// The source sample rate, before any resampling.
    #[inline]
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Decodes the next chunk, appending interleaved stereo samples to `out`.
    ///
    /// Returns whether anything was appended. A damaged packet is skipped rather
    /// than ending the track, which is what a listener expects from a stream
    /// that arrived over a lossy connection.
    pub fn decode(&mut self, out: &mut Vec<f32>) -> Result<bool, AudioError> {
        // As in `new`: a symphonia panic on malformed packets ends the track
        // with an error instead of unwinding through the player.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.decode_next(out)))
            .unwrap_or_else(|_| {
                Err(AudioError::Decode("the decoder panicked on the stream".into()))
            })
    }

    fn decode_next(&mut self, out: &mut Vec<f32>) -> Result<bool, AudioError> {
        match &mut self.inner {
            Inner::OggOpus(decoder) => decoder.decode(out),
            Inner::ContainerOpus(decoder) => decoder.decode(out),
            Inner::Symphonia { format, decoder, track_id, spec, mixer, samples } => loop {
                let packet = match format.next_packet() {
                    Ok(packet) => packet,
                    Err(SymphoniaError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                        return Ok(false)
                    }
                    Err(SymphoniaError::ResetRequired) => {
                        decoder.reset();
                        continue;
                    }
                    Err(e) => return Err(AudioError::Decode(e.to_string())),
                };
                if packet.track_id() != *track_id {
                    continue;
                }
                let audio = match decoder.decode(&packet) {
                    Ok(audio) => audio,
                    // A packet that does not decode is a packet lost.
                    Err(SymphoniaError::DecodeError(_)) => continue,
                    Err(e) => return Err(AudioError::Decode(e.to_string())),
                };
                // Vorbis primes its overlap state with a packet that produces
                // no PCM. Symphonia's interleaver indexes each channel even
                // for zero frames, which would panic on our empty buffer.
                if audio.frames() == 0 {
                    continue;
                }
                let decoded_spec = *audio.spec();
                if decoded_spec.rate != self.rate {
                    return Err(AudioError::Decode(
                        "source sample rate changed during playback".into(),
                    ));
                }
                let channels_changed = decoded_spec.channels != spec.channels;
                let needed = audio.frames() * decoded_spec.channels.count();
                if channels_changed || samples.capacity() < needed {
                    // Room for twice the packet means a stream of even packets
                    // reallocates once rather than on every step up.
                    *samples = SampleBuffer::new((audio.frames() * 2) as u64, decoded_spec);
                }
                if channels_changed {
                    *spec = decoded_spec;
                    *mixer = Downmix::new(spec.channels);
                }
                samples.copy_interleaved_ref(audio);
                mixer.process(samples.samples(), out);
                return Ok(true);
            },
        }
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
                out.extend(samples.iter().flat_map(|&sample| {
                    let s = finite_or_zero(sample);
                    [s, s]
                }));
            }
            Self::Stereo => {
                let len = (samples.len() / 2) * 2;
                out.extend(samples[..len].iter().copied().map(finite_or_zero));
            }
            Self::FallbackMono { count, inv_count } => {
                out.extend(samples.chunks_exact(*count).flat_map(|frame| {
                    let mono = frame.iter().copied().map(finite_or_zero).sum::<f32>() * inv_count;
                    [mono, mono]
                }));
            }
            Self::Matrix { count, weights, inv_left, inv_right } => {
                out.extend(samples.chunks_exact(*count).flat_map(|frame| {
                    let mut l = 0.0f32;
                    let mut r = 0.0f32;
                    for (&sample, &[lw, rw]) in frame.iter().zip(weights) {
                        let s = finite_or_zero(sample);
                        l += s * lw;
                        r += s * rw;
                    }
                    [l * inv_left, r * inv_right]
                }));
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
        use symphonia::core::codecs::CodecParameters;

        // Symphonia 0.5.5 `isomp4` prepends `b"OpusHead"` onto raw big-endian ISO-BMFF `dOps`
        // (`Version = 0`, PreSkip = 0x0138 = 312, OutputGain = 0x0100 = +1 dB in Q7.8).
        let mut mp4_extra = b"OpusHead".to_vec();
        mp4_extra.extend_from_slice(&[0, 2, 0x01, 0x38, 0x00, 0x00, 0xbb, 0x80, 0x01, 0x00, 0]);
        let mut mp4_params = CodecParameters::new();
        mp4_params.with_extra_data(mp4_extra.into_boxed_slice());
        let (skip_be, gain_be) = parse_container_opus_params(&mp4_params).unwrap();
        assert_eq!(skip_be, 312);
        let expected_gain = 10.0f32.powf(256.0 / (20.0 * 256.0));
        assert!((gain_be - expected_gain).abs() < 1e-6, "gain_be={gain_be}");

        // RFC 7845 `OpusHead` (`Version = 1`, little-endian PreSkip = 0x0138, OutputGain = 0x0100).
        let mut webm_extra = b"OpusHead".to_vec();
        webm_extra.extend_from_slice(&[1, 2, 0x38, 0x01, 0x80, 0xbb, 0x00, 0x00, 0x00, 0x01, 0]);
        let mut webm_params = CodecParameters::new();
        webm_params.with_extra_data(webm_extra.into_boxed_slice());
        let (skip_le, gain_le) = parse_container_opus_params(&webm_params).unwrap();
        assert_eq!(skip_le, 312);
        assert!((gain_le - expected_gain).abs() < 1e-6, "gain_le={gain_le}");
    }
}
