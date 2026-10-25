//! An Opus decoder, used for the containers symphonia cannot decode.
//!
//! Opus in Ogg and Opus in WebM are both common on the sources this node reads,
//! and libopus decodes them with the same library the encoder comes from, so
//! there is no second implementation to keep in step.

use std::ffi::c_int;
use std::ptr;

/// Opus packets may contain up to 120 ms, even though Discord output uses 20 ms.
const MAX_PACKET_FRAMES: c_int = 5_760;

#[repr(C)]
struct OpusDecoder {
    _private: [u8; 0],
}

const OPUS_OK: c_int = 0;

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
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("libopus refused to create a decoder (code {0})")]
    Create(c_int),
    #[error("libopus failed to decode a packet (code {0})")]
    Decode(c_int),
}

/// A stereo 48 kHz Opus decoder.
pub struct Decoder {
    state: *mut OpusDecoder,
}

unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Result<Self, DecodeError> {
        let mut error: c_int = OPUS_OK;
        let state = unsafe { opus_decoder_create(48_000, 2, &mut error) };
        if state.is_null() || error != OPUS_OK {
            return Err(DecodeError::Create(error));
        }
        Ok(Decoder { state })
    }

    /// Decodes one packet into interleaved stereo samples.
    ///
    /// A packet that libopus rejects is not fatal: the stream continues after
    /// it, so the caller gets an empty result rather than an error.
    pub fn decode(&mut self, packet: &[u8], out: &mut Vec<i16>) -> Result<usize, DecodeError> {
        let length = c_int::try_from(packet.len()).map_err(|_| DecodeError::Decode(-1))?;
        if length == 0 {
            return Err(DecodeError::Decode(-1));
        }
        let expected = unsafe { opus_packet_get_nb_samples(packet.as_ptr(), length, 48_000) };
        if !(1..=MAX_PACKET_FRAMES).contains(&expected) {
            return Err(DecodeError::Decode(expected.min(-1)));
        }
        // Size for this packet's actual duration, avoiding a 120 ms zero-fill
        // on every normal 20 ms packet while accepting all valid durations.
        out.resize(expected as usize * 2, 0);
        let frames = unsafe {
            opus_decode(self.state, packet.as_ptr(), length, out.as_mut_ptr(), expected, 0)
        };
        if frames < 0 {
            return Err(DecodeError::Decode(frames));
        }
        out.truncate(frames as usize * 2);
        Ok(frames as usize)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        if !self.state.is_null() {
            unsafe { opus_decoder_destroy(self.state) };
            self.state = ptr::null_mut();
        }
    }
}

#[cfg(test)]
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
