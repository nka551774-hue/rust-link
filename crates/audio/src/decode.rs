//! Decoding a track into samples.
//!
//! Everything the node plays arrives as one stream of bytes from a source, with
//! no file name and no content type it can trust. The decoder therefore has to
//! work out what it has from the bytes themselves, which is what symphonia's
//! probe does for the formats it covers and what the Opus path below does for
//! the one it demuxes but cannot decode.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Mutex;

use symphonia::core::audio::{Channels, SampleBuffer, SignalSpec};
use symphonia::core::codecs::{Decoder as SymphoniaDecoder, DecoderOptions, CODEC_TYPE_OPUS};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::engine::Body;
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
            Ok(body) => body.read(buf),
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
    scratch: Vec<i16>,
    /// The interleaving buffer of the symphonia path, kept between packets so
    /// that a steady stream does not allocate one per packet.
    samples: SampleBuffer<f32>,
}

enum Inner {
    Symphonia {
        format: Box<dyn FormatReader>,
        decoder: Box<dyn SymphoniaDecoder>,
        track_id: u32,
        spec: SignalSpec,
        mixer: Downmix,
    },
    OggOpus {
        reader: Box<OggOpusReader<Body>>,
        decoder: opus_decode::Decoder,
        skip: u32,
        packet: Vec<u8>,
        pending: bool,
        gain: f32,
    },
}

impl Decoder {
    /// Opens a decoder over a body, working out the container from its first
    /// bytes.
    pub fn new(mut body: Body) -> Result<Self, AudioError> {
        body.peek(64)?;

        // Opus in Ogg is demuxed here rather than by symphonia, because
        // symphonia has no Opus decoder and libopus does.
        if body.head().starts_with(b"OggS") {
            let mut reader = Box::new(OggOpusReader::new(body));
            let mut packet = Vec::new();
            match reader.next_packet_into(&mut packet) {
                Ok(true) => {
                    let decoder = opus_decode::Decoder::new()
                        .map_err(|e| AudioError::Decode(e.to_string()))?;
                    let skip = reader.pre_skip();
                    let gain = 10.0f32.powf(reader.output_gain() as f32 / (20.0 * 256.0));
                    return Ok(Decoder {
                        inner: Inner::OggOpus {
                            reader,
                            decoder,
                            skip,
                            packet,
                            pending: true,
                            gain,
                        },
                        rate: 48_000,
                        scratch: Vec::new(),
                        samples: SampleBuffer::new(
                            0,
                            SignalSpec::new(48_000, Channels::FRONT_LEFT),
                        ),
                    });
                }
                Ok(false) => return Err(AudioError::Decode("the stream holds no audio".into())),
                Err(e) => return Err(AudioError::Decode(e.to_string())),
            }
        }

        let source = Unseekable { inner: Mutex::new(body) };
        let stream = MediaSourceStream::new(Box::new(source), Default::default());
        let probed = symphonia::default::get_probe()
            .format(
                &Hint::new(),
                stream,
                &FormatOptions { enable_gapless: true, ..Default::default() },
                &MetadataOptions::default(),
            )
            .map_err(|e| AudioError::Decode(e.to_string()))?;
        let format = probed.format;
        let track = format
            .default_track()
            .ok_or_else(|| AudioError::Decode("the stream holds no playable track".into()))?;
        let track_id = track.id;
        let rate = track.codec_params.sample_rate.unwrap_or(48_000);
        let channels =
            track.codec_params.channels.unwrap_or(Channels::FRONT_LEFT | Channels::FRONT_RIGHT);
        if track.codec_params.codec == CODEC_TYPE_OPUS {
            // Opus in a container symphonia demuxes but does not decode.
            return Err(AudioError::Decode("Opus in this container is not supported".into()));
        }
        let decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .map_err(|e| AudioError::Decode(e.to_string()))?;
        Ok(Decoder {
            inner: Inner::Symphonia {
                format,
                decoder,
                track_id,
                spec: SignalSpec::new(rate, channels),
                mixer: Downmix::new(channels),
            },
            rate,
            scratch: Vec::new(),
            samples: SampleBuffer::new(0, SignalSpec::new(rate, channels)),
        })
    }

    /// The source sample rate, before any resampling.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Decodes the next chunk, appending interleaved stereo samples to `out`.
    ///
    /// Returns whether anything was appended. A damaged packet is skipped rather
    /// than ending the track, which is what a listener expects from a stream
    /// that arrived over a lossy connection.
    pub fn decode(&mut self, out: &mut Vec<f32>) -> Result<bool, AudioError> {
        match &mut self.inner {
            Inner::OggOpus { reader, decoder, skip, packet, pending, gain } => {
                if !std::mem::take(pending) && !reader.next_packet_into(packet)? {
                    return Ok(false);
                }
                let frames = match decoder.decode(packet, &mut self.scratch) {
                    Ok(frames) => frames,
                    Err(_) => {
                        // A packet libopus will not take is one packet lost.
                        return Ok(true);
                    }
                };
                let mut start = 0usize;
                if *skip > 0 {
                    let drop = (*skip as usize).min(frames);
                    *skip -= drop as u32;
                    start = drop;
                }
                out.reserve((frames - start) * 2);
                let scale = *gain / 32768.0;
                out.extend(
                    self.scratch[start * 2..frames * 2].iter().map(|sample| *sample as f32 * scale),
                );
                Ok(true)
            }
            Inner::Symphonia { format, decoder, track_id, spec, mixer } => loop {
                let packet = match format.next_packet() {
                    Ok(packet) => packet,
                    Err(SymphoniaError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                        return Ok(false)
                    }
                    Err(SymphoniaError::ResetRequired) => return Ok(false),
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
                let decoded_spec = *audio.spec();
                if decoded_spec.rate != self.rate {
                    return Err(AudioError::Decode(
                        "source sample rate changed during playback".into(),
                    ));
                }
                let channels_changed = decoded_spec.channels != spec.channels;
                let needed = audio.frames() * decoded_spec.channels.count();
                if channels_changed || self.samples.capacity() < needed {
                    // Room for twice the packet means a stream of even packets
                    // reallocates once rather than on every step up.
                    self.samples = SampleBuffer::new((audio.frames() * 2) as u64, decoded_spec);
                }
                if channels_changed {
                    *spec = decoded_spec;
                    *mixer = Downmix::new(spec.channels);
                }
                self.samples.copy_interleaved_ref(audio);
                mixer.process(self.samples.samples(), out);
                return Ok(true);
            },
        }
    }
}

/// Folds whatever the decoder produced into stereo, using the coefficients
/// FFmpeg's own stereo downmix uses.
///
/// The matrix is normalised the way FFmpeg normalises it, which is what keeps a
/// 5.1 track as loud as the stereo track next to it instead of clipping.
struct Downmix {
    count: usize,
    left: Vec<(usize, f32)>,
    right: Vec<(usize, f32)>,
    left_gain: f32,
    right_gain: f32,
}

impl Downmix {
    /// Build coefficients once per channel layout, instead of allocating and
    /// normalising two vectors for every decoded packet.
    fn new(channels: Channels) -> Self {
        let count = channels.count();
        let mut left = Vec::new();
        let mut right = Vec::new();
        if count > 2 {
            for (index, channel) in channels.iter().enumerate() {
                if channel == Channels::FRONT_LEFT {
                    left.push((index, 1.0));
                } else if channel == Channels::FRONT_RIGHT {
                    right.push((index, 1.0));
                } else if channel == Channels::FRONT_CENTRE {
                    left.push((index, 0.707));
                    right.push((index, 0.707));
                } else if channel == Channels::REAR_LEFT || channel == Channels::SIDE_LEFT {
                    left.push((index, 0.707));
                } else if channel == Channels::REAR_RIGHT || channel == Channels::SIDE_RIGHT {
                    right.push((index, 0.707));
                }
            }
        }
        let left_gain = left.iter().map(|(_, weight)| weight).sum::<f32>().max(1.0);
        let right_gain = right.iter().map(|(_, weight)| weight).sum::<f32>().max(1.0);
        Self { count, left, right, left_gain, right_gain }
    }

    fn process(&self, samples: &[f32], out: &mut Vec<f32>) {
        if self.count == 0 {
            return;
        }
        let frames = samples.len() / self.count;
        out.reserve(frames * 2);
        match self.count {
            1 => {
                for sample in samples {
                    out.push(*sample);
                    out.push(*sample);
                }
            }
            2 => out.extend_from_slice(&samples[..frames * 2]),
            _ => {
                for frame in samples.chunks_exact(self.count) {
                    let mut l = 0.0;
                    let mut r = 0.0;
                    for (index, weight) in &self.left {
                        l += frame[*index] * weight;
                    }
                    for (index, weight) in &self.right {
                        r += frame[*index] * weight;
                    }
                    out.push(l / self.left_gain);
                    out.push(r / self.right_gain);
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
        let mut out = Vec::new();
        downmix(&[1.0, 0.5], &spec(Channels::FRONT_LEFT), &mut out);
        assert_eq!(out, vec![1.0, 1.0, 0.5, 0.5]);
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
}
