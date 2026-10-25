//! The Opus encoder.
//!
//! The node speaks Opus to Discord, and libopus is the encoder every other
//! implementation uses, so this binds to it directly rather than reimplementing
//! one. The bindings are deliberately small: an encoder, the controls that
//! matter for music, and nothing else.

#![allow(non_camel_case_types)]

use std::ffi::c_int;
use std::ptr;

type OpusInt32 = c_int;
type OpusInt16 = i16;

#[repr(C)]
struct OpusEncoder {
    _private: [u8; 0],
}

const OPUS_APPLICATION_AUDIO: c_int = 2049;
const OPUS_SIGNAL_MUSIC: c_int = 3002;

const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
const OPUS_SET_VBR_REQUEST: c_int = 4006;
const OPUS_SET_COMPLEXITY_REQUEST: c_int = 4010;
const OPUS_SET_INBAND_FEC_REQUEST: c_int = 4012;
const OPUS_SET_PACKET_LOSS_PERC_REQUEST: c_int = 4014;
const OPUS_SET_SIGNAL_REQUEST: c_int = 4024;
const OPUS_GET_LOOKAHEAD_REQUEST: c_int = 4027;
const OPUS_SET_LSB_DEPTH_REQUEST: c_int = 4036;

const OPUS_OK: c_int = 0;

/// A frame at 48 kHz is 20 ms, which is what Discord wants and what the pacing
/// loop counts in.
pub const FRAME_SAMPLES: usize = 960;
/// Two channels, so a frame carries 960 samples on each side.
pub const FRAME_SAMPLES_STEREO: usize = FRAME_SAMPLES * 2;

/// The largest packet libopus will produce for one frame, with room to spare.
const MAX_PACKET: usize = 4_000;

#[derive(Debug, thiserror::Error)]
pub enum OpusError {
    #[error("libopus refused to create an encoder (code {0})")]
    Create(c_int),
    #[error("libopus failed to encode a frame (code {0})")]
    Encode(c_int),
    #[error("the frame must hold exactly {FRAME_SAMPLES_STEREO} samples")]
    FrameSize,
}

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
pub struct Encoder {
    state: *mut OpusEncoder,
    buffer: Vec<u8>,
}

// The encoder is only ever touched from the playback task that owns it.
unsafe impl Send for Encoder {}

impl Encoder {
    /// Creates an encoder at `bitrate` bits per second, running at
    /// `complexity` on the encoder's own scale of zero to ten. A machine with
    /// more players than cores lowers it: the encoder then spends less on every
    /// frame, which is the knob the operator has for a busy node.
    pub fn new(bitrate: i32, complexity: i32) -> Result<Self, OpusError> {
        let mut error: c_int = OPUS_OK;
        let state = unsafe { opus_encoder_create(48_000, 2, OPUS_APPLICATION_AUDIO, &mut error) };
        if state.is_null() || error != OPUS_OK {
            return Err(OpusError::Create(error));
        }
        let encoder = Encoder { state, buffer: vec![0u8; MAX_PACKET] };
        // Music, not speech, and a bitrate the operator chose. Variable bitrate
        // is what keeps a quiet passage from costing as much as a loud one.
        encoder.control(OPUS_SET_SIGNAL_REQUEST, OPUS_SIGNAL_MUSIC);
        encoder.control(OPUS_SET_VBR_REQUEST, 1);
        encoder.control(OPUS_SET_BITRATE_REQUEST, bitrate);
        // The samples handed in are 16-bit integers, and saying so is what lets
        // the encoder tell near-silence from detail below the quantisation
        // noise instead of spending bits on the noise.
        encoder.control(OPUS_SET_LSB_DEPTH_REQUEST, 16);
        // The default is the encoder's own maximum, because a node is not short
        // of CPU the way a phone is; an operator with more players than cores
        // asks for less, and says so once for the whole node.
        encoder.control(OPUS_SET_COMPLEXITY_REQUEST, complexity);
        // Packets travel over UDP, where the transport recovers a lost one by
        // decoding the packet after it, so the encoder is told to expect a little
        // loss and to protect against it.
        encoder.control(OPUS_SET_INBAND_FEC_REQUEST, 1);
        encoder.control(OPUS_SET_PACKET_LOSS_PERC_REQUEST, 10);
        Ok(encoder)
    }

    fn control(&self, request: c_int, value: c_int) {
        unsafe {
            opus_encoder_ctl(self.state, request, value);
        }
    }

    /// How many samples the encoder holds back, which a caller needs if it is
    /// aligning output with a source position.
    pub fn lookahead(&self) -> i32 {
        let mut value: c_int = 0;
        unsafe {
            opus_encoder_ctl(self.state, OPUS_GET_LOOKAHEAD_REQUEST, &mut value as *mut c_int);
        }
        value
    }

    /// Encodes one 20 ms frame of interleaved stereo samples.
    pub fn encode(&mut self, pcm: &[i16]) -> Result<&[u8], OpusError> {
        if pcm.len() != FRAME_SAMPLES_STEREO {
            return Err(OpusError::FrameSize);
        }
        let written = unsafe {
            opus_encode(
                self.state,
                pcm.as_ptr(),
                FRAME_SAMPLES as c_int,
                self.buffer.as_mut_ptr(),
                MAX_PACKET as OpusInt32,
            )
        };
        if written < 0 {
            return Err(OpusError::Encode(written));
        }
        Ok(&self.buffer[..written as usize])
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if !self.state.is_null() {
            unsafe { opus_encoder_destroy(self.state) };
            self.state = ptr::null_mut();
        }
    }
}

/// Converts interleaved float samples to the 16 bit form libopus takes.
///
/// The clamp is not cosmetic: a filter chain is free to overshoot, and libopus
/// treats an out of range sample as undefined input.
pub fn to_i16(samples: &[f32], out: &mut [i16]) {
    for (target, sample) in out.iter_mut().zip(samples.iter()) {
        *target = (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16;
    }
}

#[cfg(test)]
mod tests {
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
    fn floats_are_clamped_before_they_become_integers() {
        let samples = [2.0f32, -2.0, 0.5, -0.5];
        let mut out = [0i16; 4];
        to_i16(&samples, &mut out);
        assert_eq!(out, [32767, -32767, 16384, -16384]);
    }
}
