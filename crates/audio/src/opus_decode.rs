//! An Opus decoder, used for the containers symphonia cannot decode.
//!
//! Opus in Ogg and Opus in WebM are both common on the sources this node reads,
//! and libopus decodes them with the same library the encoder comes from, so
//! there is no second implementation to keep in step.

use std::ffi::c_int;
#[cfg(feature = "native-opus")]
use std::ptr::{self, NonNull};

/// Opus packets may contain up to 120 ms, even though Discord output uses 20 ms.
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
extern "C" {
    fn opus_decoder_create(fs: c_int, channels: c_int, error: *mut c_int) -> *mut OpusDecoder;
    fn opus_decoder_destroy(st: *mut OpusDecoder);
    fn opus_packet_get_nb_samples(data: *const u8, len: c_int, fs: c_int) -> c_int;
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

/// Validates `packet`'s length and TOC sample count `(len, frames)`.
#[cfg(feature = "native-opus")]
#[inline]
fn packet_frame_count(packet: &[u8]) -> Result<(c_int, c_int), DecodeError> {
    let length = c_int::try_from(packet.len()).map_err(|_| DecodeError::Decode(-1))?;
    if length == 0 {
        return Err(DecodeError::Decode(-1));
    }
    let frames = unsafe { opus_packet_get_nb_samples(packet.as_ptr(), length, 48_000) };
    if !(1..=MAX_PACKET_FRAMES).contains(&frames) {
        return Err(DecodeError::Decode(frames.min(-1)));
    }
    Ok((length, frames))
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

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("libopus refused to create a decoder (code {0})")]
    Create(c_int),
    #[error("libopus failed to decode a packet (code {0})")]
    Decode(c_int),
}

/// A stereo 48 kHz Opus decoder.
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
        Ok(Decoder { state })
    }

    /// Decodes one packet into interleaved stereo 16-bit samples.
    pub fn decode(&mut self, packet: &[u8], out: &mut Vec<i16>) -> Result<usize, DecodeError> {
        let (length, expected) = packet_frame_count(packet)?;
        let cap = expected as usize * 2;
        out.clear();
        out.reserve(cap);
        // SAFETY: `out.reserve(cap)` guarantees at least `expected * 2` writable `i16` slots,
        // and `opus_decode` initializes `0..frames * 2` (`0 <= frames <= expected`) without reading `pcm`.
        let frames = unsafe {
            opus_decode(
                self.state.as_ptr(),
                packet.as_ptr(),
                length,
                out.spare_capacity_mut().as_mut_ptr().cast::<i16>(),
                expected,
                0,
            )
        };
        if frames < 0 || frames > expected {
            return Err(DecodeError::Decode(frames.min(-1)));
        }
        unsafe {
            out.set_len(frames as usize * 2);
        }
        Ok(frames as usize)
    }

    /// Decodes one packet into unclipped interleaved stereo float samples so
    /// container `output_gain` can scale peaks above 0 dBFS without hard-clipping.
    pub fn decode_float(
        &mut self,
        packet: &[u8],
        out: &mut Vec<f32>,
    ) -> Result<usize, DecodeError> {
        out.clear();
        self.decode_float_append(packet, out)
    }

    /// Appends one decoded packet of unclipped interleaved stereo float samples onto `out`.
    pub fn decode_float_append(
        &mut self,
        packet: &[u8],
        out: &mut Vec<f32>,
    ) -> Result<usize, DecodeError> {
        let (length, expected) = packet_frame_count(packet)?;
        let base = out.len();
        let cap = expected as usize * 2;
        out.reserve(cap);
        // SAFETY: `out.reserve(cap)` guarantees at least `expected * 2` writable `f32` slots
        // starting at `base`, and `opus_decode_float` initializes `0..frames * 2`.
        let frames = unsafe {
            opus_decode_float(
                self.state.as_ptr(),
                packet.as_ptr(),
                length,
                out.spare_capacity_mut().as_mut_ptr().cast::<f32>(),
                expected,
                0,
            )
        };
        if frames < 0 || frames > expected {
            return Err(DecodeError::Decode(frames.min(-1)));
        }
        unsafe {
            out.set_len(base + frames as usize * 2);
        }
        Ok(frames as usize)
    }

    /// Invokes libopus packet-loss concealment for `frames` samples when a
    /// packet's payload is damaged so timeline and pre-skip stay aligned.
    pub fn conceal_float(
        &mut self,
        frames: usize,
        out: &mut Vec<f32>,
    ) -> Result<usize, DecodeError> {
        out.clear();
        self.conceal_float_append(frames, out)
    }

    /// Appends concealed interleaved stereo float samples onto `out`.
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
        if produced < 0 {
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

    pub fn decode(&mut self, _packet: &[u8], _out: &mut Vec<i16>) -> Result<usize, DecodeError> {
        Err(DecodeError::Decode(-7))
    }

    pub fn decode_float(
        &mut self,
        _packet: &[u8],
        _out: &mut Vec<f32>,
    ) -> Result<usize, DecodeError> {
        Err(DecodeError::Decode(-7))
    }

    pub fn decode_float_append(
        &mut self,
        _packet: &[u8],
        _out: &mut Vec<f32>,
    ) -> Result<usize, DecodeError> {
        Err(DecodeError::Decode(-7))
    }

    pub fn conceal_float(
        &mut self,
        _frames: usize,
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

#[cfg(feature = "native-opus")]
impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { opus_decoder_destroy(self.state.as_ptr()) };
    }
}

#[cfg(all(test, feature = "native-opus"))]
mod tests {
    use super::*;
    use crate::opus::Encoder;
    use crate::opus::{FRAME_SAMPLES, FRAME_SAMPLES_STEREO};

    #[test]
    fn a_packet_survives_the_round_trip() {
        let mut encoder = Encoder::new(128_000, 10).unwrap();
        let mut pcm = Vec::with_capacity(FRAME_SAMPLES_STEREO);
        for i in 0..FRAME_SAMPLES {
            let value = (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 48_000.0).sin();
            pcm.push((value * 20_000.0) as i16);
            pcm.push((value * 20_000.0) as i16);
        }
        let packet = encoder.encode(&pcm).unwrap().to_vec();
        let mut decoder = Decoder::new().unwrap();
        let mut decoded = Vec::new();
        let frames = decoder.decode(&packet, &mut decoded).unwrap();
        assert_eq!(frames, FRAME_SAMPLES);
        assert_eq!(decoded.len(), FRAME_SAMPLES_STEREO);
        // A lossy codec, so the check is that the tone came back rather than
        // that the samples match.
        let peak = decoded.iter().map(|v| v.abs()).max().unwrap();
        assert!(peak > 15_000, "peak was {peak}");
    }

    #[test]
    fn a_damaged_packet_is_reported_rather_than_crashing() {
        let mut decoder = Decoder::new().unwrap();
        let mut decoded = Vec::new();
        assert!(decoder.decode(&[0xff, 0xff, 0xff, 0xff], &mut decoded).is_err());
    }

    #[test]
    fn multi_frame_packets_up_to_120_ms_are_decoded() {
        let mut decoder = Decoder::new().unwrap();
        let mut decoded = Vec::new();
        // Two and six CBR CELT silence frames, each 20 ms long.
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
}
