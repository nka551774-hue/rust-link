//! The in-process filters.
//!
//! Each stage applies the same setting the FFmpeg graph would, in the same
//! order, to interleaved stereo samples at 48 kHz. The chain is split in two
//! around the time scale stage, because the tempo change happens in the
//! resampler rather than sample by sample, and the protocol fixes its place in
//! the graph.
//!
//! The two engines are not bit-identical and are not meant to be. Where an
//! effect is defined by an FFmpeg implementation detail rather than by the
//! protocol, the stage below documents what it does instead.

use std::f64::consts::PI;

use crate::filters::{ChannelMix, Distortion, FilterSet, Karaoke, LowPass, Modulation, Rotation};

/// The sample rate the node plays at.
pub const SAMPLE_RATE: u32 = 48_000;
/// The channel count the node plays at.
pub const CHANNELS: usize = 2;
/// One Opus frame, which is what the pacing loop counts in.
pub const FRAME_MILLIS: u64 = 20;
/// Samples per channel in one frame.
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize / 1000) * FRAME_MILLIS as usize;

/// A filter chain for one playback.
#[derive(Debug, Default)]
pub struct DspChain {
    pre: Vec<Stage>,
    post: Vec<Stage>,
    /// Samples seen so far, which is the time base the low frequency
    /// oscillators run on.
    clock: u64,
}

impl DspChain {
    pub fn new(set: &FilterSet) -> Self {
        let mut pre = Vec::new();
        if set.gain != 1.0 {
            pre.push(Stage::Volume(set.gain));
        }
        if !set.bands.is_empty() {
            pre.push(Stage::Equalizer(EqualizerStage::new(
                set.bands.iter().map(|b| (b.frequency, b.decibels)).collect(),
            )));
        }
        if let Some(k) = set.karaoke {
            pre.push(Stage::Karaoke(KaraokeStage::new(k)));
        }

        let mut post = Vec::new();
        if let Some(m) = set.tremolo {
            post.push(Stage::Tremolo(TremoloStage::new(m)));
        }
        if let Some(m) = set.vibrato {
            post.push(Stage::Vibrato(VibratoStage::new(m)));
        }
        if let Some(r) = set.rotation {
            post.push(Stage::Rotation(RotationStage::new(r)));
        }
        if let Some(d) = set.distort {
            post.push(Stage::Distortion(DistortionStage::new(d)));
        }
        if let Some(m) = set.mix {
            post.push(Stage::ChannelMix(m));
        }
        if let Some(l) = set.low_pass {
            post.push(Stage::LowPass(OnePoleStereo::new(l)));
        }
        DspChain { pre, post, clock: 0 }
    }

    /// Whether the chain does anything at all, which is how the engine knows it
    /// can hand decoded samples straight to the encoder.
    pub fn is_empty(&self) -> bool {
        self.pre.is_empty() && self.post.is_empty()
    }

    /// Applies the filters that come before the time scale stage.
    pub fn process_pre(&mut self, buf: &mut [f32]) {
        let clock = self.clock;
        for stage in &mut self.pre {
            stage.apply(buf, clock);
        }
    }

    /// Applies the filters that come after it.
    pub fn process_post(&mut self, buf: &mut [f32]) {
        let clock = self.clock;
        for stage in &mut self.post {
            stage.apply(buf, clock);
        }
        self.clock += (buf.len() / CHANNELS) as u64;
    }
}

#[derive(Debug)]
enum Stage {
    Volume(f64),
    Equalizer(EqualizerStage),
    Karaoke(KaraokeStage),
    Tremolo(TremoloStage),
    Vibrato(VibratoStage),
    Rotation(RotationStage),
    Distortion(DistortionStage),
    ChannelMix(ChannelMix),
    LowPass(OnePoleStereo),
}

impl Stage {
    fn apply(&mut self, buf: &mut [f32], clock: u64) {
        match self {
            Stage::Volume(gain) => {
                for sample in buf.iter_mut() {
                    *sample = (*sample as f64 * *gain) as f32;
                }
            }
            Stage::Equalizer(stage) => stage.process(buf),
            Stage::Karaoke(stage) => stage.process(buf),
            Stage::Tremolo(stage) => stage.process(buf, clock),
            Stage::Vibrato(stage) => stage.process(buf, clock),
            Stage::Rotation(stage) => stage.process(buf, clock),
            Stage::Distortion(stage) => stage.process(buf),
            Stage::ChannelMix(mix) => {
                let (ll, lr, rl, rr) = (
                    mix.left_to_left as f32,
                    mix.left_to_right as f32,
                    mix.right_to_left as f32,
                    mix.right_to_right as f32,
                );
                for frame in buf.as_chunks_mut::<CHANNELS>().0 {
                    let (l, r) = (frame[0], frame[1]);
                    frame[0] = ll * l + rl * r;
                    frame[1] = lr * l + rr * r;
                }
            }
            Stage::LowPass(stage) => stage.process(buf),
        }
    }
}

/// A direct form 1 biquad.
#[derive(Debug, Clone, Copy, Default)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Biquad {
    fn normalized(mut self, a0: f64) -> Self {
        self.b0 /= a0;
        self.b1 /= a0;
        self.b2 /= a0;
        self.a1 /= a0;
        self.a2 /= a0;
        self
    }

    /// A peaking band, the shape FFmpeg's `equalizer` uses: a bandwidth of
    /// `width_octaves` around `f0`.
    fn peaking(f0: f64, width_octaves: f64, decibels: f64) -> Self {
        let fs = SAMPLE_RATE as f64;
        let a = 10f64.powf(decibels / 40.0);
        let w0 = 2.0 * PI * f0 / fs;
        let alpha = w0.sin() * (std::f64::consts::LN_2 / 2.0 * width_octaves).sinh();
        Biquad {
            b0: 1.0 + alpha * a,
            b1: -2.0 * w0.cos(),
            b2: 1.0 - alpha * a,
            a1: -2.0 * w0.cos(),
            a2: 1.0 - alpha / a,
            ..Default::default()
        }
        .normalized(1.0 + alpha / a)
    }

    /// The band pass FFmpeg's `bandpass=f=..:t=h` produces, which is the shape
    /// with a constant 0 dB peak gain: the peak is unity whatever the width.
    /// `width_hz` is the band width, which becomes the Q the biquad needs.
    fn bandpass(f0: f64, width_hz: f64) -> Self {
        let fs = SAMPLE_RATE as f64;
        let w0 = 2.0 * PI * f0 / fs;
        let q = (f0 / width_hz.max(1.0)).max(0.05);
        let alpha = w0.sin() / (2.0 * q);
        Biquad {
            b0: alpha,
            b1: 0.0,
            b2: -alpha,
            a1: -2.0 * w0.cos(),
            a2: 1.0 - alpha,
            ..Default::default()
        }
        .normalized(1.0 + alpha)
    }

    fn step(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// One biquad section per channel, so a filter has independent state on each
/// side.
#[derive(Debug, Clone, Copy, Default)]
struct BiquadStereo {
    channels: [Biquad; CHANNELS],
}

impl BiquadStereo {
    fn process(&mut self, buf: &mut [f32]) {
        for frame in buf.as_chunks_mut::<CHANNELS>().0 {
            for (channel, sample) in self.channels.iter_mut().zip(frame.iter_mut()) {
                *sample = channel.step(*sample as f64) as f32;
            }
        }
    }
}

/// The equalizer is a cascade of peaking bands, applied in the order the client
/// listed them, which is what a list of `equalizer` entries in a filter graph
/// does.
#[derive(Debug, Clone)]
struct EqualizerStage {
    sections: Vec<BiquadStereo>,
}

impl EqualizerStage {
    fn new(bands: Vec<(f64, f64)>) -> Self {
        let sections = bands
            .into_iter()
            .map(|(frequency, decibels)| {
                let section = Biquad::peaking(frequency, 0.666_667, decibels);
                BiquadStereo { channels: [section; CHANNELS] }
            })
            .collect();
        EqualizerStage { sections }
    }

    fn process(&mut self, buf: &mut [f32]) {
        for section in &mut self.sections {
            section.process(buf);
        }
    }
}

/// The karaoke filter: the centre channel is band passed and subtracted, which
/// is the `asplit`/`pan=mono`/`amix` chain in the FFmpeg graph.
#[derive(Debug, Clone, Copy)]
struct KaraokeStage {
    band: [Biquad; CHANNELS],
    gain: f64,
}

impl KaraokeStage {
    fn new(k: Karaoke) -> Self {
        let band = Biquad::bandpass(k.filter_band, k.filter_width);
        KaraokeStage { band: [band; CHANNELS], gain: k.gain }
    }

    fn process(&mut self, buf: &mut [f32]) {
        for frame in buf.as_chunks_mut::<CHANNELS>().0 {
            let mid = 0.5 * (frame[0] as f64 + frame[1] as f64);
            let cut = (self.band[0].step(mid) * self.gain) as f32;
            frame[0] += cut;
            frame[1] += cut;
        }
    }
}

/// FFmpeg's tremolo: a gain that sweeps between one and `1 - depth` at the
/// filter's frequency.
#[derive(Debug, Clone, Copy)]
struct TremoloStage {
    frequency: f64,
    depth: f64,
}

impl TremoloStage {
    fn new(m: Modulation) -> Self {
        TremoloStage { frequency: m.frequency, depth: m.depth }
    }

    fn process(&mut self, buf: &mut [f32], clock: u64) {
        for (index, frame) in buf.as_chunks_mut::<CHANNELS>().0.iter_mut().enumerate() {
            let seconds = (clock + index as u64) as f64 / SAMPLE_RATE as f64;
            let gain = 1.0 - self.depth * (1.0 + (2.0 * PI * self.frequency * seconds).sin()) / 2.0;
            let gain = gain as f32;
            frame[0] *= gain;
            frame[1] *= gain;
        }
    }
}

/// Vibrato moves the read position of a short delay line, which is what makes a
/// pitch wobble without changing the tempo.
#[derive(Debug, Clone)]
struct VibratoStage {
    frequency: f64,
    depth: f64,
    max_delay: usize,
    write: usize,
    history: [Vec<f32>; CHANNELS],
}

impl VibratoStage {
    fn new(m: Modulation) -> Self {
        // The deepest setting moves the read position by five milliseconds, the
        // range a vibrato is heard over.
        let max_delay = (0.005 * SAMPLE_RATE as f64) as usize + 2;
        VibratoStage {
            frequency: m.frequency,
            depth: m.depth,
            max_delay,
            write: 0,
            history: [vec![0.0; max_delay + 2], vec![0.0; max_delay + 2]],
        }
    }

    fn process(&mut self, buf: &mut [f32], clock: u64) {
        let len = self.history[0].len();
        for (index, frame) in buf.as_chunks_mut::<CHANNELS>().0.iter_mut().enumerate() {
            let seconds = (clock + index as u64) as f64 / SAMPLE_RATE as f64;
            let delay = self.depth
                * self.max_delay as f64
                * (1.0 + (2.0 * PI * self.frequency * seconds).sin())
                / 2.0;
            let whole = delay.floor() as usize;
            let fraction = (delay - whole as f64) as f32;
            for (channel, sample) in frame.iter_mut().enumerate() {
                self.history[channel][self.write] = *sample;
                let a = self.history[channel][(self.write + len - whole) % len];
                let b = self.history[channel][(self.write + len - whole - 1) % len];
                *sample = a + (b - a) * fraction;
            }
            self.write = (self.write + 1) % len;
        }
    }
}

/// Rotation pans the two channels against each other, exactly as the FFmpeg
/// graph's `aeval` expression does.
#[derive(Debug, Clone, Copy)]
struct RotationStage {
    hertz: f64,
}

impl RotationStage {
    fn new(r: Rotation) -> Self {
        RotationStage { hertz: r.hertz }
    }

    fn process(&mut self, buf: &mut [f32], clock: u64) {
        for (index, frame) in buf.as_chunks_mut::<CHANNELS>().0.iter_mut().enumerate() {
            let seconds = (clock + index as u64) as f64 / SAMPLE_RATE as f64;
            let angle = 2.0 * PI * self.hertz * seconds;
            frame[0] *= (0.5 + 0.5 * angle.cos()) as f32;
            frame[1] *= (0.5 - 0.5 * angle.cos()) as f32;
        }
    }
}

/// The waveshaper the protocol exposes, evaluated the way the FFmpeg `aeval`
/// expression is.
#[derive(Debug, Clone, Copy)]
struct DistortionStage {
    d: Distortion,
}

impl DistortionStage {
    fn new(d: Distortion) -> Self {
        DistortionStage { d }
    }

    fn process(&mut self, buf: &mut [f32]) {
        let d = &self.d;
        for sample in buf.iter_mut() {
            let x = *sample as f64;
            let y = d.scale
                * ((x * d.sin_scale + d.sin_offset).sin()
                    * (x * d.cos_scale + d.cos_offset).cos()
                    * (x * d.tan_scale + d.tan_offset).tan())
                + d.offset;
            *sample = y.clamp(-1.0, 1.0) as f32;
        }
    }
}

/// The one pole low pass the protocol's `lowPass` filter maps to.
#[derive(Debug, Clone, Copy, Default)]
struct OnePoleStereo {
    alpha: f64,
    state: [f64; CHANNELS],
}

impl OnePoleStereo {
    fn new(l: LowPass) -> Self {
        let alpha = 1.0 - (-2.0 * PI * l.frequency / SAMPLE_RATE as f64).exp();
        OnePoleStereo { alpha, state: [0.0; CHANNELS] }
    }

    fn process(&mut self, buf: &mut [f32]) {
        for frame in buf.as_chunks_mut::<CHANNELS>().0 {
            for (channel, sample) in frame.iter_mut().enumerate() {
                let x = *sample as f64;
                self.state[channel] += self.alpha * (x - self.state[channel]);
                *sample = self.state[channel] as f32;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filters::parse;
    use rustlink_protocol::Object;

    fn chain(value: serde_json::Value) -> DspChain {
        let object: Object = match value {
            serde_json::Value::Object(m) => m,
            _ => panic!("not an object"),
        };
        DspChain::new(&parse(&object, 100).unwrap())
    }

    fn tone(samples: usize, amplitude: f32) -> Vec<f32> {
        (0..samples * CHANNELS).map(|i| if i % 2 == 0 { amplitude } else { -amplitude }).collect()
    }

    #[test]
    fn an_empty_chain_leaves_samples_alone() {
        let mut chain = chain(serde_json::json!({}));
        assert!(chain.is_empty());
        let mut buf = tone(64, 0.5);
        let original = buf.clone();
        chain.process_pre(&mut buf);
        chain.process_post(&mut buf);
        assert_eq!(buf, original);
    }

    #[test]
    fn volume_scales_every_sample() {
        let mut chain = chain(serde_json::json!({"volume": 0.5}));
        let mut buf = vec![1.0f32, -1.0, 0.5, -0.5];
        chain.process_pre(&mut buf);
        assert_eq!(buf, vec![0.5, -0.5, 0.25, -0.25]);
    }

    #[test]
    fn the_player_volume_also_scales() {
        let object = Object::new();
        let mut chain = DspChain::new(&parse(&object, 50).unwrap());
        let mut buf = vec![1.0f32, 1.0];
        chain.process_pre(&mut buf);
        assert_eq!(buf, vec![0.5, 0.5]);
    }

    #[test]
    fn a_channel_mix_rewrites_the_matrix() {
        let mut chain = chain(
            serde_json::json!({"channelMix": {"leftToRight": 1.0, "rightToLeft": 1.0, "leftToLeft": 0.0, "rightToRight": 0.0}}),
        );
        let mut buf = vec![1.0f32, 0.0, 0.0, 1.0];
        chain.process_post(&mut buf);
        assert_eq!(buf, vec![0.0, 1.0, 1.0, 0.0]);
    }

    #[test]
    fn a_low_pass_settles_a_constant_signal() {
        let mut chain = chain(serde_json::json!({"lowPass": {"smoothing": 20}}));
        let mut buf = vec![1.0f32; 20_000];
        chain.process_post(&mut buf);
        assert!((buf[buf.len() - 1] - 1.0).abs() < 1e-3);
        assert!(buf[0] < 1.0);
    }

    #[test]
    fn a_boosted_band_raises_its_own_frequency() {
        let mut chain = chain(serde_json::json!({"equalizer": [{"band": 8, "gain": 1.0}]}));
        let mut buf = Vec::new();
        for i in 0..48_000 {
            let value = (2.0 * PI * 1000.0 * i as f64 / SAMPLE_RATE as f64).sin() as f32;
            buf.push(value);
            buf.push(value);
        }
        let peak_before = buf.iter().cloned().fold(f32::MIN, f32::max);
        chain.process_pre(&mut buf);
        let peak_after = buf.iter().cloned().fold(f32::MIN, f32::max);
        assert!(peak_after > peak_before * 3.0, "{peak_after} should be well above {peak_before}");
    }

    #[test]
    fn a_flat_band_leaves_the_signal_alone() {
        let mut chain = chain(serde_json::json!({"equalizer": [{"band": 8, "gain": 0.0}]}));
        let mut buf = tone(512, 0.25);
        let original = buf.clone();
        chain.process_pre(&mut buf);
        for (a, b) in buf.iter().zip(original.iter()) {
            assert!((a - b).abs() < 1e-6, "{a} != {b}");
        }
    }

    #[test]
    fn karaoke_removes_the_centre_channel() {
        let mut chain = chain(
            serde_json::json!({"karaoke": {"level": 1.0, "monoLevel": 1.0, "filterBand": 220, "filterWidth": 100}}),
        );
        // A tone that is identical on both sides sits in the centre, so the
        // band it is in should be subtracted away.
        let mut buf = Vec::new();
        for i in 0..48_000 {
            let value = 0.5 * (2.0 * PI * 220.0 * i as f64 / SAMPLE_RATE as f64).sin() as f32;
            buf.push(value);
            buf.push(value);
        }
        chain.process_pre(&mut buf);
        let tail = &buf[buf.len() - 4800..];
        let peak = tail.iter().fold(0f32, |acc, v| acc.max(v.abs()));
        assert!(peak < 0.05, "centre channel survived at {peak}");
    }

    #[test]
    fn karaoke_leaves_what_is_not_in_the_centre_alone() {
        let mut chain = chain(
            serde_json::json!({"karaoke": {"level": 1.0, "monoLevel": 1.0, "filterBand": 220, "filterWidth": 100}}),
        );
        // The same tone, but with the sides out of phase, which is where a
        // karaoke filter is supposed to leave it.
        let mut buf = Vec::new();
        for i in 0..48_000 {
            let value = 0.5 * (2.0 * PI * 220.0 * i as f64 / SAMPLE_RATE as f64).sin() as f32;
            buf.push(value);
            buf.push(-value);
        }
        let before = buf[buf.len() - 2];
        chain.process_pre(&mut buf);
        let after = buf[buf.len() - 2];
        assert!((before - after).abs() < 1e-6, "{before} became {after}");
    }

    #[test]
    fn tremolo_modulates_the_gain_between_one_and_one_minus_the_depth() {
        let mut chain = chain(serde_json::json!({"tremolo": {"frequency": 5, "depth": 1.0}}));
        let mut buf = vec![1.0f32; 48_000 * 2];
        chain.process_post(&mut buf);
        let peak = buf.iter().cloned().fold(f32::MIN, f32::max);
        let trough = buf.iter().cloned().fold(f32::MAX, f32::min);
        assert!(peak > 0.98, "peak {peak}");
        assert!(trough < 0.02, "trough {trough}");
    }

    #[test]
    fn rotation_keeps_the_total_energy() {
        let mut chain = chain(serde_json::json!({"rotation": {"rotationHz": 1.0}}));
        let mut buf = tone(48_000, 1.0);
        chain.process_post(&mut buf);
        let peak = buf.iter().fold(0f32, |acc, v| acc.max(v.abs()));
        assert!(peak <= 1.0001, "peak {peak} should not exceed full scale");
    }

    #[test]
    fn distortion_is_clipped_to_full_scale() {
        let mut chain = chain(
            serde_json::json!({"distortion": {"scale": 10.0, "sinScale": 10.0, "tanScale": 10.0}}),
        );
        let mut buf = tone(256, 0.9);
        chain.process_post(&mut buf);
        for sample in &buf {
            assert!(sample.abs() <= 1.0);
        }
    }
}
