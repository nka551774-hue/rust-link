//! The Opus encoder.
//!
//! The node speaks Opus to Discord, and libopus is the encoder every other
//! implementation uses, so this binds to it directly rather than reimplementing
//! one. The bindings are deliberately small: an encoder, the controls that
//! matter for music, and nothing else.

#![allow(non_camel_case_types)]

use std::ffi::c_int;
#[cfg(feature = "native-opus")]
use std::ptr::NonNull;

#[cfg(feature = "native-opus")]
type OpusInt32 = c_int;
#[cfg(feature = "native-opus")]
type OpusInt16 = i16;

#[cfg(feature = "native-opus")]
#[repr(C)]
struct OpusEncoder {
    _private: [u8; 0],
}

#[cfg(feature = "native-opus")]
const OPUS_APPLICATION_AUDIO: c_int = 2049;
#[cfg(feature = "native-opus")]
const OPUS_SIGNAL_MUSIC: c_int = 3002;

#[cfg(feature = "native-opus")]
const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
#[cfg(feature = "native-opus")]
const OPUS_SET_VBR_REQUEST: c_int = 4006;
#[cfg(feature = "native-opus")]
const OPUS_SET_COMPLEXITY_REQUEST: c_int = 4010;
#[cfg(feature = "native-opus")]
const OPUS_SET_INBAND_FEC_REQUEST: c_int = 4012;
#[cfg(feature = "native-opus")]
const OPUS_SET_PACKET_LOSS_PERC_REQUEST: c_int = 4014;
#[cfg(feature = "native-opus")]
const OPUS_SET_VBR_CONSTRAINT_REQUEST: c_int = 4020;
#[cfg(feature = "native-opus")]
const OPUS_SET_SIGNAL_REQUEST: c_int = 4024;
#[cfg(feature = "native-opus")]
const OPUS_GET_LOOKAHEAD_REQUEST: c_int = 4027;
#[cfg(feature = "native-opus")]
const OPUS_SET_LSB_DEPTH_REQUEST: c_int = 4036;

#[cfg(feature = "native-opus")]
const OPUS_OK: c_int = 0;

/// A frame at 48 kHz is 20 ms, which is what Discord wants and what the pacing
/// loop counts in.
pub const FRAME_SAMPLES: usize = crate::dsp::FRAME_SAMPLES;
/// Two channels, so a frame carries 960 samples on each side.
pub const FRAME_SAMPLES_STEREO: usize = FRAME_SAMPLES * crate::dsp::CHANNELS;

/// The largest packet libopus will produce for one frame, with room to spare.
#[cfg(feature = "native-opus")]
const MAX_PACKET: usize = 4_000;

#[derive(Debug, thiserror::Error)]
pub enum OpusError {
    #[error("libopus refused to create an encoder (code {0})")]
    Create(c_int),
    #[error("libopus failed to encode a frame (code {0})")]
    Encode(c_int),
    #[error("libopus rejected encoder control {request} (code {code})")]
    Control { request: c_int, code: c_int },
    #[error("the frame must hold exactly {FRAME_SAMPLES_STEREO} samples")]
    FrameSize,
}

#[cfg(feature = "native-opus")]
extern "C" {
    fn opus_encoder_create(
        fs: OpusInt32,
        channels: c_int,
        application: c_int,
        error: *mut c_int,
    ) -> *mut OpusEncoder;
    fn opus_encoder_destroy(st: *mut OpusEncoder);
    fn opus_encode(
        st: *mut OpusEncoder,
        pcm: *const OpusInt16,
        frame_size: c_int,
        data: *mut u8,
        max_data_bytes: OpusInt32,
    ) -> OpusInt32;
    fn opus_encoder_ctl(st: *mut OpusEncoder, request: c_int, ...) -> c_int;
}

/// A stereo 48 kHz Opus encoder tuned for music.
#[cfg(feature = "native-opus")]
pub struct Encoder {
    state: NonNull<OpusEncoder>,
    buffer: Vec<u8>,
    lookahead: i32,
}

#[cfg(not(feature = "native-opus"))]
pub struct Encoder;

// The encoder is only ever touched from the playback task that owns it.
#[cfg(feature = "native-opus")]
unsafe impl Send for Encoder {}

#[cfg(feature = "native-opus")]
impl Encoder {
    /// Creates an encoder at `bitrate` bits per second, running at
    /// `complexity` on the encoder's own scale of zero to ten. A machine with
    /// more players than cores lowers it: the encoder then spends less on every
    /// frame, which is the knob the operator has for a busy node.
    pub fn new(bitrate: i32, complexity: i32) -> Result<Self, OpusError> {
        let mut error: c_int = OPUS_OK;
        let raw = unsafe { opus_encoder_create(48_000, 2, OPUS_APPLICATION_AUDIO, &mut error) };
        let Some(state) = NonNull::new(raw).filter(|_| error == OPUS_OK) else {
            if !raw.is_null() {
                unsafe { opus_encoder_destroy(raw) };
            }
            return Err(OpusError::Create(error));
        };
        let mut encoder = Encoder { state, buffer: Vec::new(), lookahead: 0 };
        for (request, value) in [
            // Music, not speech, and variable bitrate so quiet passages spend fewer bits.
            (OPUS_SET_SIGNAL_REQUEST, OPUS_SIGNAL_MUSIC),
            (OPUS_SET_VBR_REQUEST, 1),
            (OPUS_SET_VBR_CONSTRAINT_REQUEST, 0),
            (OPUS_SET_BITRATE_REQUEST, bitrate),
            // 16-bit integer input lets libopus avoid spending bits below quantisation noise.
            (OPUS_SET_LSB_DEPTH_REQUEST, 16),
            (OPUS_SET_COMPLEXITY_REQUEST, complexity),
            // Request the same loss allowance as the FFmpeg path.
            (OPUS_SET_INBAND_FEC_REQUEST, 1),
            (OPUS_SET_PACKET_LOSS_PERC_REQUEST, 10),
        ] {
            encoder.control(request, value)?;
        }
        let code = unsafe {
            opus_encoder_ctl(
                encoder.state.as_ptr(),
                OPUS_GET_LOOKAHEAD_REQUEST,
                &mut encoder.lookahead as *mut c_int,
            )
        };
        if code != OPUS_OK {
            return Err(OpusError::Control { request: OPUS_GET_LOOKAHEAD_REQUEST, code });
        }
        Ok(encoder)
    }

    fn control(&self, request: c_int, value: c_int) -> Result<(), OpusError> {
        let code = unsafe { opus_encoder_ctl(self.state.as_ptr(), request, value) };
        if code == OPUS_OK {
            Ok(())
        } else {
            Err(OpusError::Control { request, code })
        }
    }

    /// How many samples the encoder holds back, which a caller needs if it is
    /// aligning output with a source position.
    #[inline]
    pub fn lookahead(&self) -> i32 {
        self.lookahead
    }

    /// Encodes one 20 ms frame of interleaved stereo samples directly into `out`,
    /// reusing `out`'s allocation without an intermediate copy or zero-fill.
    #[inline]
    pub fn encode_into(&mut self, pcm: &[i16], out: &mut Vec<u8>) -> Result<(), OpusError> {
        if pcm.len() != FRAME_SAMPLES_STEREO {
            return Err(OpusError::FrameSize);
        }
        out.clear();
        out.reserve(MAX_PACKET);
        // SAFETY: `out.reserve(MAX_PACKET)` guarantees at least `MAX_PACKET`
        // writable bytes in `spare_capacity_mut()`, and `opus_encode` initializes
        // `0..written` bytes (`0 <= written <= MAX_PACKET`) without reading `data`.
        let written = unsafe {
            opus_encode(
                self.state.as_ptr(),
                pcm.as_ptr(),
                FRAME_SAMPLES as c_int,
                out.spare_capacity_mut().as_mut_ptr().cast::<u8>(),
                MAX_PACKET as OpusInt32,
            )
        };
        if written < 0 {
            return Err(OpusError::Encode(written));
        }
        unsafe {
            out.set_len(written as usize);
        }
        Ok(())
    }

    /// Encodes one 20 ms frame of interleaved stereo samples.
    #[inline]
    pub fn encode(&mut self, pcm: &[i16]) -> Result<&[u8], OpusError> {
        let mut buffer = std::mem::take(&mut self.buffer);
        let result = self.encode_into(pcm, &mut buffer);
        self.buffer = buffer;
        result?;
        Ok(&self.buffer)
    }
}

#[cfg(not(feature = "native-opus"))]
impl Encoder {
    pub fn new(_bitrate: i32, _complexity: i32) -> Result<Self, OpusError> {
        Err(OpusError::Create(-7))
    }

    #[inline]
    pub fn lookahead(&self) -> i32 {
        312
    }

    #[inline]
    pub fn encode_into(&mut self, pcm: &[i16], _out: &mut Vec<u8>) -> Result<(), OpusError> {
        if pcm.len() != FRAME_SAMPLES_STEREO {
            return Err(OpusError::FrameSize);
        }
        Err(OpusError::Encode(-7))
    }

    #[inline]
    pub fn encode(&mut self, pcm: &[i16]) -> Result<&[u8], OpusError> {
        self.encode_into(pcm, &mut Vec::new())?;
        Ok(&[])
    }
}

#[cfg(feature = "native-opus")]
impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { opus_encoder_destroy(self.state.as_ptr()) };
    }
}

/// Converts interleaved float samples to the 16 bit form libopus takes.
///
/// The clamp is not cosmetic: a filter chain is free to overshoot, and libopus
/// treats an out of range sample as undefined input.
#[inline]
pub fn to_i16(samples: &[f32], out: &mut [i16]) {
    for (target, &sample) in out.iter_mut().zip(samples) {
        let clean = crate::dsp::clean_f32(sample).clamp(-1.0, 1.0);
        let scaled = clean * 32767.0;
        *target = (scaled + 0.5f32.copysign(scaled)) as i16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_are_clamped_before_they_become_integers() {
        let samples = [2.0f32, -2.0, 0.5, -0.5, f32::NAN, f32::INFINITY];
        let mut out = [0i16; 6];
        to_i16(&samples, &mut out);
        assert_eq!(out, [32767, -32767, 16384, -16384, 0, 0]);
    }

    #[test]
    fn saturated_max_and_subnormals_convert_cleanly() {
        let samples = [f32::MAX, -f32::MAX, f32::NEG_INFINITY, f32::from_bits(1), -0.0];
        let mut out = [0i16; 5];
        to_i16(&samples, &mut out);
        assert_eq!(out, [32767, -32767, 0, 0, 0]);
    }

    #[cfg(feature = "native-opus")]
    mod native {
        use super::*;

        fn tone() -> Vec<i16> {
            let mut pcm = Vec::with_capacity(FRAME_SAMPLES_STEREO);
            for i in 0..FRAME_SAMPLES {
                let value = (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 48_000.0).sin();
                pcm.push((value * 16_000.0) as i16);
                pcm.push((value * 16_000.0) as i16);
            }
            pcm
        }

        #[test]
        fn an_encoder_produces_a_packet_for_each_frame() {
            let mut encoder = Encoder::new(128_000, 10).expect("libopus is available");
            let pcm = tone();
            let mut sizes = Vec::new();
            for _ in 0..10 {
                let packet = encoder.encode(&pcm).unwrap();
                assert!(!packet.is_empty());
                assert!(packet.len() < 1500, "a 20 ms packet should not approach the MTU");
                sizes.push(packet.len());
            }
            // A 128 kbps stream carries about 320 bytes in 20 ms.
            let average = sizes.iter().sum::<usize>() / sizes.len();
            assert!((200..500).contains(&average), "average packet was {average} bytes");
        }

        #[test]
        fn a_frame_of_the_wrong_size_is_refused() {
            let mut encoder = Encoder::new(64_000, 10).unwrap();
            assert!(matches!(encoder.encode(&[0i16; 100]), Err(OpusError::FrameSize)));
        }

        #[test]
        fn the_lookahead_is_reported() {
            let encoder = Encoder::new(128_000, 10).unwrap();
            // 6.5 ms at 48 kHz, the encoder's own delay.
            assert_eq!(encoder.lookahead(), 312);
        }

        #[test]
        fn a_lower_complexity_still_encodes_every_frame() {
            let mut encoder = Encoder::new(128_000, 4).expect("libopus is available");
            let pcm = tone();
            for _ in 0..10 {
                assert!(!encoder.encode(&pcm).unwrap().is_empty());
            }
        }

        #[test]
        fn invalid_controls_are_reported_instead_of_silently_using_defaults() {
            assert!(matches!(Encoder::new(192_000, 11), Err(OpusError::Control { .. })));
            assert!(matches!(Encoder::new(0, 10), Err(OpusError::Control { .. })));
        }
    }
}
