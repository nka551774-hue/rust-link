//! System `libopus` stereo 48 kHz decoder and RFC 6716 packet inspection.

use std::ffi::c_int;
#[cfg(feature = "native-opus")]
use std::ptr::{self, NonNull};

/// Opus packets may contain up to 120 ms (`5760` samples @ 48 kHz).
#[cfg(feature = "native-opus")]
const MAX_PACKET_FRAMES: c_int = 5_760;

#[cfg(feature = "native-opus")]
#[repr(C)]
struct OpusDecoder {
    _private: [u8; 0],
}

#[cfg(feature = "native-opus")]
const OPUS_OK: c_int = 0;
#[cfg(feature = "native-opus")]
const OPUS_RESET_STATE: c_int = 4028;
/// Maximum RFC 6716 compressed payload length for a single 20 ms Opus packet.
#[cfg(feature = "native-opus")]
const MAX_DISCORD_OPUS_BYTES: usize = 1_275;

#[cfg(feature = "native-opus")]
extern "C" {
    fn opus_decoder_create(fs: c_int, channels: c_int, error: *mut c_int) -> *mut OpusDecoder;
    fn opus_decoder_destroy(st: *mut OpusDecoder);
    fn opus_decoder_ctl(st: *mut OpusDecoder, request: c_int, ...) -> c_int;
    fn opus_packet_get_nb_samples(data: *const u8, len: c_int, fs: c_int) -> c_int;
    fn opus_packet_parse(
        data: *const u8,
        len: c_int,
        out_toc: *mut u8,
        frames: *mut *const u8,
        size: *mut i16,
        payload_offset: *mut c_int,
    ) -> c_int;
    fn opus_decode(
        st: *mut OpusDecoder,
        data: *const u8,
        len: c_int,
        pcm: *mut i16,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
    fn opus_decode_float(
        st: *mut OpusDecoder,
        data: *const u8,
        len: c_int,
        pcm: *mut f32,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
}

#[cfg(feature = "native-opus")]
#[inline]
fn packet_frame_count(packet: &[u8]) -> Result<(c_int, c_int), DecodeError> {
    let length =
        c_int::try_from(packet.len()).ok().filter(|&n| n > 0).ok_or(DecodeError::Decode(-1))?;
    let frames = unsafe { opus_packet_get_nb_samples(packet.as_ptr(), length, 48_000) };
    (1..=MAX_PACKET_FRAMES)
        .contains(&frames)
        .then_some((length, frames))
        .ok_or_else(|| DecodeError::Decode(frames.min(-1)))
}

/// Returns the number of 48 kHz frames described by `packet`'s TOC byte(s).
#[cfg(feature = "native-opus")]
#[inline]
pub fn packet_samples(packet: &[u8]) -> Option<usize> {
    packet_frame_count(packet).ok().map(|(_, frames)| frames as usize)
}

#[cfg(not(feature = "native-opus"))]
#[inline]
pub fn packet_samples(_packet: &[u8]) -> Option<usize> {
    None
}

/// Validates without decoding that `packet` is a well-formed RFC 6716 20 ms Opus packet.
#[cfg(feature = "native-opus")]
#[inline]
pub fn is_valid_20ms_packet(packet: &[u8]) -> bool {
    if !(1..=MAX_DISCORD_OPUS_BYTES).contains(&packet.len()) {
        return false;
    }
    let Ok((length, 960)) = packet_frame_count(packet) else {
        return false;
    };
    let mut toc = 0u8;
    let mut frames = [ptr::null(); 48];
    let mut sizes = [0i16; 48];
    let mut payload_offset: c_int = 0;
    let parsed = unsafe {
        opus_packet_parse(
            packet.as_ptr(),
            length,
            &mut toc,
            frames.as_mut_ptr(),
            sizes.as_mut_ptr(),
            &mut payload_offset,
        )
    };
    (1..=48).contains(&parsed)
}

#[cfg(not(feature = "native-opus"))]
#[inline]
pub fn is_valid_20ms_packet(_packet: &[u8]) -> bool {
    false
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("libopus refused to create a decoder (code {0})")]
    Create(c_int),
    #[error("libopus failed to decode a packet (code {0})")]
    Decode(c_int),
}

/// Stereo 48 kHz Opus decoder.
#[cfg(feature = "native-opus")]
pub struct Decoder {
    state: NonNull<OpusDecoder>,
}

#[cfg(not(feature = "native-opus"))]
pub struct Decoder;

#[cfg(feature = "native-opus")]
unsafe impl Send for Decoder {}

#[cfg(feature = "native-opus")]
impl Decoder {
    pub fn new() -> Result<Self, DecodeError> {
        let mut error: c_int = OPUS_OK;
        let raw = unsafe { opus_decoder_create(48_000, 2, &mut error) };
        let Some(state) = NonNull::new(raw).filter(|_| error == OPUS_OK) else {
            if !raw.is_null() {
                unsafe { opus_decoder_destroy(raw) };
            }
            return Err(DecodeError::Create(error));
        };
        Ok(Self { state })
    }

    /// Clears internal predictor and overlap state after compressed-packet skips.
    #[inline]
    pub fn reset(&mut self) {
        unsafe {
            let _ = opus_decoder_ctl(self.state.as_ptr(), OPUS_RESET_STATE);
        }
    }

    #[inline]
    fn decode_into<T>(
        out: &mut Vec<T>,
        expected: c_int,
        f: impl FnOnce(*mut T) -> c_int,
    ) -> Result<usize, DecodeError> {
        let base = out.len();
        out.reserve(expected as usize * 2);
        // SAFETY: `out` has `>= expected * 2` writable `T` slots at `base`; `f` initializes `0..frames * 2`.
        let frames = f(out.spare_capacity_mut().as_mut_ptr().cast::<T>());
        if !(0..=expected).contains(&frames) {
            return Err(DecodeError::Decode(frames.min(-1)));
        }
        unsafe { out.set_len(base + frames as usize * 2) };
        Ok(frames as usize)
    }

    /// Decodes one packet into interleaved stereo 16-bit samples.
    pub fn decode(&mut self, packet: &[u8], out: &mut Vec<i16>) -> Result<usize, DecodeError> {
        let (length, expected) = packet_frame_count(packet)?;
        out.clear();
        Self::decode_into(out, expected, |dst| unsafe {
            opus_decode(self.state.as_ptr(), packet.as_ptr(), length, dst, expected, 0)
        })
    }

    /// Appends one decoded packet of unclipped interleaved stereo float samples onto `out`.
    pub fn decode_float_append(
        &mut self,
        packet: &[u8],
        out: &mut Vec<f32>,
    ) -> Result<usize, DecodeError> {
        let (length, expected) = packet_frame_count(packet)?;
        Self::decode_into(out, expected, |dst| unsafe {
            opus_decode_float(self.state.as_ptr(), packet.as_ptr(), length, dst, expected, 0)
        })
    }

    /// Appends packet-loss-concealed interleaved stereo float samples onto `out`.
    pub fn conceal_float_append(
        &mut self,
        frames: usize,
        out: &mut Vec<f32>,
    ) -> Result<usize, DecodeError> {
        let expected = c_int::try_from(frames)
            .ok()
            .filter(|f| (1..=MAX_PACKET_FRAMES).contains(f))
            .ok_or(DecodeError::Decode(-1))?;
        let base = out.len();
        out.resize(base + frames * 2, 0.0);
        let produced = unsafe {
            opus_decode_float(
                self.state.as_ptr(),
                ptr::null(),
                0,
                out[base..].as_mut_ptr(),
                expected,
                0,
            )
        };
        if !(0..=expected).contains(&produced) {
            out[base..].fill(0.0);
            return Ok(frames);
        }
        out.truncate(base + produced as usize * 2);
        Ok(produced as usize)
    }
}

#[cfg(not(feature = "native-opus"))]
impl Decoder {
    pub fn new() -> Result<Self, DecodeError> {
        Err(DecodeError::Create(-7))
    }

    pub fn reset(&mut self) {}

    pub fn decode(&mut self, _packet: &[u8], _out: &mut Vec<i16>) -> Result<usize, DecodeError> {
        Err(DecodeError::Decode(-7))
    }

    pub fn decode_float_append(
        &mut self,
        _packet: &[u8],
        _out: &mut Vec<f32>,
    ) -> Result<usize, DecodeError> {
        Err(DecodeError::Decode(-7))
    }

    pub fn conceal_float_append(
        &mut self,
        _frames: usize,
        _out: &mut Vec<f32>,
    ) -> Result<usize, DecodeError> {
        Err(DecodeError::Decode(-7))
    }
}

impl Decoder {
    /// Decodes one packet into unclipped interleaved stereo float samples.
    pub fn decode_float(
        &mut self,
        packet: &[u8],
        out: &mut Vec<f32>,
    ) -> Result<usize, DecodeError> {
        out.clear();
        self.decode_float_append(packet, out)
    }

    /// Invokes packet-loss concealment for `frames` samples into `out`.
    pub fn conceal_float(
        &mut self,
        frames: usize,
        out: &mut Vec<f32>,
    ) -> Result<usize, DecodeError> {
        out.clear();
        self.conceal_float_append(frames, out)
    }
}

#[cfg(feature = "native-opus")]
impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { opus_decoder_destroy(self.state.as_ptr()) };
    }
}

#[cfg(all(test, feature = "native-opus"))]
mod tests {
    use super::*;
    use crate::opus::{Encoder, FRAME_SAMPLES, FRAME_SAMPLES_STEREO};

    #[test]
    fn a_packet_survives_the_round_trip() {
        let mut encoder = Encoder::new(128_000, 10).unwrap();
        let pcm: Vec<i16> = (0..FRAME_SAMPLES)
            .flat_map(|i| {
                let s =
                    ((std::f64::consts::TAU * 440.0 * i as f64 / 48_000.0).sin() * 20_000.0) as i16;
                [s, s]
            })
            .collect();
        let packet = encoder.encode(&pcm).unwrap().to_vec();
        let mut decoder = Decoder::new().unwrap();
        let mut decoded = Vec::new();
        assert_eq!(decoder.decode(&packet, &mut decoded).unwrap(), FRAME_SAMPLES);
        assert_eq!(decoded.len(), FRAME_SAMPLES_STEREO);
        let peak = decoded.iter().map(|v| v.abs()).max().unwrap();
        assert!(peak > 15_000, "peak was {peak}");
    }

    #[test]
    fn a_damaged_packet_is_reported_rather_than_crashing() {
        let mut decoder = Decoder::new().unwrap();
        assert!(decoder.decode(&[0xff; 4], &mut Vec::new()).is_err());
    }

    #[test]
    fn multi_frame_packets_up_to_120_ms_are_decoded() {
        let mut decoder = Decoder::new().unwrap();
        let mut decoded = Vec::new();
        assert_eq!(decoder.decode(&[0xf9, 0xff, 0xfe, 0xff, 0xfe], &mut decoded).unwrap(), 1920);
        let mut packet = vec![0xfb, 6];
        packet.extend([0xff, 0xfe].repeat(6));
        assert_eq!(decoder.decode(&packet, &mut decoded).unwrap(), 5760);
        assert_eq!(decoded.len(), 11_520);
        packet[1] = 7;
        packet.extend_from_slice(&[0xff, 0xfe]);
        assert!(decoder.decode(&packet, &mut decoded).is_err());
        assert!(decoder.decode(&[], &mut decoded).is_err());
    }

    #[test]
    fn packet_loss_concealment_appends_bounded_frame_counts() {
        let mut decoder = Decoder::new().unwrap();
        let mut out = vec![0.25f32; 4];
        assert!(decoder.conceal_float_append(0, &mut out).is_err());
        assert!(decoder.conceal_float_append(MAX_PACKET_FRAMES as usize + 1, &mut out).is_err());
        assert_eq!(out.len(), 4);
        assert_eq!(decoder.conceal_float_append(960, &mut out).unwrap(), 960);
        assert_eq!(out.len(), 4 + 960 * 2);
    }
}
