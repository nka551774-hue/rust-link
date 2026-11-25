//! System `libopus` bindings for stereo 48 kHz music encoding.

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

/// Samples per channel in one 20 ms frame at 48 kHz.
pub const FRAME_SAMPLES: usize = crate::dsp::FRAME_SAMPLES;
/// Interleaved stereo sample count in one 20 ms frame (`1920`).
pub const FRAME_SAMPLES_STEREO: usize = FRAME_SAMPLES * crate::dsp::CHANNELS;

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
    fn opus_encode_float(
        st: *mut OpusEncoder,
        pcm: *const f32,
        frame_size: c_int,
        data: *mut u8,
        max_data_bytes: OpusInt32,
    ) -> OpusInt32;
    fn opus_pcm_soft_clip(
        pcm: *mut f32,
        frame_size: c_int,
        channels: c_int,
        softclip_mem: *mut f32,
    );
    fn opus_encoder_ctl(st: *mut OpusEncoder, request: c_int, ...) -> c_int;
}

#[inline]
fn check_frame_size<T>(pcm: &[T], frame_samples: usize) -> Result<(), OpusError> {
    if matches!(frame_samples, 120 | 240 | 480 | 960 | 1920 | 2880)
        && pcm.len() == frame_samples * crate::dsp::CHANNELS
    {
        Ok(())
    } else {
        Err(OpusError::FrameSize)
    }
}

/// Stereo 48 kHz Opus encoder tuned for music.
#[cfg(feature = "native-opus")]
pub struct Encoder {
    state: NonNull<OpusEncoder>,
    buffer: Vec<u8>,
    lookahead: i32,
    softclip_mem: [f32; crate::dsp::CHANNELS],
}

#[cfg(not(feature = "native-opus"))]
pub struct Encoder;

#[cfg(feature = "native-opus")]
unsafe impl Send for Encoder {}

#[cfg(feature = "native-opus")]
impl Encoder {
    pub fn new(bitrate: i32, complexity: i32) -> Result<Self, OpusError> {
        let mut error: c_int = OPUS_OK;
        let raw = unsafe { opus_encoder_create(48_000, 2, OPUS_APPLICATION_AUDIO, &mut error) };
        let Some(state) = NonNull::new(raw).filter(|_| error == OPUS_OK) else {
            if !raw.is_null() {
                unsafe { opus_encoder_destroy(raw) };
            }
            return Err(OpusError::Create(error));
        };
        let mut encoder = Self {
            state,
            buffer: Vec::new(),
            lookahead: 0,
            softclip_mem: [0.0; crate::dsp::CHANNELS],
        };
        for (request, value) in [
            (OPUS_SET_SIGNAL_REQUEST, OPUS_SIGNAL_MUSIC),
            (OPUS_SET_VBR_REQUEST, 1),
            (OPUS_SET_VBR_CONSTRAINT_REQUEST, 0),
            (OPUS_SET_BITRATE_REQUEST, bitrate),
            (OPUS_SET_LSB_DEPTH_REQUEST, 24),
            (OPUS_SET_COMPLEXITY_REQUEST, complexity),
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
        (code == OPUS_OK).then_some(()).ok_or(OpusError::Control { request, code })
    }

    #[inline]
    pub fn lookahead(&self) -> i32 {
        self.lookahead
    }

    #[inline]
    fn write_packet(
        out: &mut Vec<u8>,
        f: impl FnOnce(*mut u8) -> OpusInt32,
    ) -> Result<(), OpusError> {
        out.clear();
        out.reserve(MAX_PACKET);
        // SAFETY: `out` has `>= MAX_PACKET` writable spare bytes; `f` initializes `0..written`.
        let written = f(out.spare_capacity_mut().as_mut_ptr().cast::<u8>());
        if written < 0 {
            return Err(OpusError::Encode(written));
        }
        unsafe { out.set_len(written as usize) };
        Ok(())
    }

    /// Encodes a valid Opus frame size (`120|240|480|960|1920|2880`) of stereo `i16` PCM into `out`.
    #[inline]
    pub fn encode_frame_size(
        &mut self,
        pcm: &[i16],
        frame_samples: usize,
        out: &mut Vec<u8>,
    ) -> Result<(), OpusError> {
        check_frame_size(pcm, frame_samples)?;
        Self::write_packet(out, |dst| unsafe {
            opus_encode(
                self.state.as_ptr(),
                pcm.as_ptr(),
                frame_samples as c_int,
                dst,
                MAX_PACKET as OpusInt32,
            )
        })
    }

    /// Encodes a valid Opus frame size of stereo `f32` PCM into `out` with 24-bit precision
    /// and stateful C1-continuous soft clipping (`opus_pcm_soft_clip`).
    #[inline]
    pub fn encode_float_frame_size(
        &mut self,
        pcm: &mut [f32],
        frame_samples: usize,
        out: &mut Vec<u8>,
    ) -> Result<(), OpusError> {
        check_frame_size(pcm, frame_samples)?;
        for sample in pcm.iter_mut() {
            *sample = crate::dsp::clean_f32(*sample).clamp(-16.0, 16.0);
        }
        Self::write_packet(out, |dst| unsafe {
            opus_pcm_soft_clip(
                pcm.as_mut_ptr(),
                frame_samples as c_int,
                crate::dsp::CHANNELS as c_int,
                self.softclip_mem.as_mut_ptr(),
            );
            opus_encode_float(
                self.state.as_ptr(),
                pcm.as_ptr(),
                frame_samples as c_int,
                dst,
                MAX_PACKET as OpusInt32,
            )
        })
    }

    /// Encodes one 20 ms frame of interleaved stereo `i16` samples.
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
    pub fn encode_frame_size(
        &mut self,
        pcm: &[i16],
        frame_samples: usize,
        _out: &mut Vec<u8>,
    ) -> Result<(), OpusError> {
        check_frame_size(pcm, frame_samples)?;
        Err(OpusError::Encode(-7))
    }

    #[inline]
    pub fn encode_float_frame_size(
        &mut self,
        pcm: &mut [f32],
        frame_samples: usize,
        _out: &mut Vec<u8>,
    ) -> Result<(), OpusError> {
        check_frame_size(pcm, frame_samples)?;
        Err(OpusError::Encode(-7))
    }

    #[inline]
    pub fn encode(&mut self, pcm: &[i16]) -> Result<&[u8], OpusError> {
        self.encode_into(pcm, &mut Vec::new())?;
        Ok(&[])
    }
}

impl Encoder {
    /// Encodes one 20 ms frame of stereo `f32` samples directly into `out`.
    #[inline]
    pub fn encode_float_into(
        &mut self,
        pcm: &mut [f32],
        out: &mut Vec<u8>,
    ) -> Result<(), OpusError> {
        self.encode_float_frame_size(pcm, FRAME_SAMPLES, out)
    }

    /// Encodes one 20 ms frame of stereo `i16` samples directly into `out`.
    #[inline]
    pub fn encode_into(&mut self, pcm: &[i16], out: &mut Vec<u8>) -> Result<(), OpusError> {
        self.encode_frame_size(pcm, FRAME_SAMPLES, out)
    }
}

#[cfg(feature = "native-opus")]
impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { opus_encoder_destroy(self.state.as_ptr()) };
    }
}

/// Converts interleaved float samples to clamped 16-bit PCM.
#[inline]
pub fn to_i16(samples: &[f32], out: &mut [i16]) {
    for (target, &sample) in out.iter_mut().zip(samples) {
        let scaled = crate::dsp::clean_f32(sample).clamp(-1.0, 1.0) * 32767.0;
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
            (0..FRAME_SAMPLES)
                .flat_map(|i| {
                    let s = ((std::f64::consts::TAU * 440.0 * i as f64 / 48_000.0).sin() * 16_000.0)
                        as i16;
                    [s, s]
                })
                .collect()
        }

        #[test]
        fn an_encoder_produces_a_packet_for_each_frame() {
            let mut encoder = Encoder::new(128_000, 10).expect("libopus is available");
            let pcm = tone();
            let mut total = 0;
            for _ in 0..10 {
                let packet = encoder.encode(&pcm).unwrap();
                assert!(!packet.is_empty() && packet.len() < 1500);
                total += packet.len();
            }
            let average = total / 10;
            assert!((200..500).contains(&average), "average packet was {average} bytes");
        }

        #[test]
        fn a_frame_of_the_wrong_size_is_refused() {
            let mut encoder = Encoder::new(64_000, 10).unwrap();
            assert!(matches!(encoder.encode(&[0i16; 100]), Err(OpusError::FrameSize)));
        }

        #[test]
        fn the_lookahead_is_reported() {
            assert_eq!(Encoder::new(128_000, 10).unwrap().lookahead(), 312);
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

        #[test]
        fn float_encoder_soft_clips_overshoots_and_sanitises_non_finite_samples() {
            let mut encoder = Encoder::new(192_000, 10).expect("libopus is available");
            let mut frame = [0.0f32; FRAME_SAMPLES_STEREO];
            for (i, [l, r]) in frame.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                let s = 1.8 * (std::f64::consts::TAU * 440.0 * i as f64 / 48_000.0).sin() as f32;
                (*l, *r) = (s, s);
            }
            (frame[0], frame[1]) = (f32::NAN, f32::INFINITY);
            let mut out = Vec::new();
            encoder.encode_float_into(&mut frame, &mut out).unwrap();
            assert!(!out.is_empty() && out.len() < 1500);
            assert!(frame.iter().all(|v| v.is_finite() && v.abs() <= 1.0001));
        }
    }
}
