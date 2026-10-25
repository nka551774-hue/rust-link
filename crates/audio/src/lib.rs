//! The audio pipeline: filters, the in-process decoder and DSP chain, the Opus
//! encoder, and the FFmpeg engine that covers everything the built-in path does
//! not.

pub mod decode;
pub mod dsp;
pub mod engine;
pub mod error;
pub mod ffmpeg;
pub mod filters;
pub mod ogg;
pub mod opus;
pub mod opus_decode;
pub mod pure;
pub mod resample;

pub use dsp::{DspChain, CHANNELS, FRAME_MILLIS, FRAME_SAMPLES, SAMPLE_RATE};
pub use engine::{detect, Body, Container, Engine, Frames, Preference};
pub use error::AudioError;
pub use filters::{
    disabled_filters, parse as parse_filters, ChannelMix, Distortion, EqBand, FilterError,
    FilterSet, Karaoke, LowPass, Modulation, Rotation, Timescale, EQUALIZER_FREQUENCIES,
    FILTER_NAMES,
};
pub use opus::{to_i16, Encoder, OpusError};
pub use resample::{ratio_for, SincResampler, TimeStretcher};
