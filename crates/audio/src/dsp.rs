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

use crate::filters::{
    ChannelMix, Distortion, EqBand, FilterSet, Karaoke, LowPass, Modulation, Rotation,
};

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
    config: FilterSet,
    pre: Vec<Stage>,
    post: Vec<Stage>,
    /// Samples seen so far, which is the time base the low frequency
    /// oscillators run on.
    clock: u64,
}

fn same_non_volume(a: &FilterSet, b: &FilterSet) -> bool {
    a.bands == b.bands
        && a.karaoke == b.karaoke
        && a.tremolo == b.tremolo
        && a.vibrato == b.vibrato
        && a.rotation == b.rotation
        && a.distort == b.distort
        && a.mix == b.mix
        && a.low_pass == b.low_pass
}

fn is_identity_mix(mix: &ChannelMix) -> bool {
    mix.left_to_left == 1.0
        && mix.left_to_right == 0.0
        && mix.right_to_left == 0.0
        && mix.right_to_right == 1.0
}

impl DspChain {
    pub fn new(set: &FilterSet) -> Self {
        let (pre, post) = Self::build_stages(set);
        DspChain { config: set.clone(), pre, post, clock: 0 }
    }

    fn build_stages(set: &FilterSet) -> (Vec<Stage>, Vec<Stage>) {
        let mut pre = Vec::new();
        if set.gain != 1.0 {
            pre.push(Stage::Volume(set.gain));
        }
        if let Some(eq) = EqualizerStage::new(&set.bands) {
            pre.push(Stage::Equalizer(eq));
        }
        if let Some(k) = set.karaoke.filter(|k| k.gain.abs() > 1e-12) {
            pre.push(Stage::Karaoke(KaraokeStage::new(k)));
        }

        let mut post = Vec::new();
        if let Some(m) = set.tremolo.filter(|m| m.depth > 0.0) {
            post.push(Stage::Tremolo(TremoloStage::new(m)));
        }
        if let Some(m) = set.vibrato.filter(|m| m.depth > 0.0) {
            post.push(Stage::Vibrato(Box::new(VibratoStage::new(m))));
        }
        if let Some(r) = set.rotation {
            post.push(Stage::Rotation(RotationStage::new(r)));
        }
        if let Some(d) = set.distort {
            post.push(Stage::Distortion(DistortionStage::new(d)));
        }
        if let Some(m) = set.mix.filter(|m| !is_identity_mix(m)) {
            post.push(Stage::ChannelMix(m));
        }
        if let Some(l) = set.low_pass {
            post.push(Stage::LowPass(OnePoleStereo::new(l)));
        }
        (pre, post)
    }

    /// Updates volume and filters in place, preserving LFO phase (`clock`) and,
    /// when only volume changed, keeping biquad/delay-line filter states intact.
    pub fn reconfigure(&mut self, set: &FilterSet) {
        if same_non_volume(&self.config, set) {
            match (self.pre.first_mut(), set.gain != 1.0) {
                (Some(Stage::Volume(gain)), true) => *gain = set.gain,
                (Some(Stage::Volume(_)), false) => {
                    self.pre.remove(0);
                }
                (_, true) => self.pre.insert(0, Stage::Volume(set.gain)),
                (_, false) => {}
            }
            self.config.gain = set.gain;
            return;
        }
        let (pre, post) = Self::build_stages(set);
        self.pre = pre;
        self.post = post;
        self.config = set.clone();
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
    Vibrato(Box<VibratoStage>),
    Rotation(RotationStage),
    Distortion(DistortionStage),
    ChannelMix(ChannelMix),
    LowPass(OnePoleStereo),
}

const LFO_CHUNK: usize = 1024;

impl Stage {
    fn apply(&mut self, buf: &mut [f32], clock: u64) {
        match self {
            Stage::Volume(gain) => {
                let gain = *gain as f32;
                for sample in buf.iter_mut() {
                    *sample *= gain;
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
                for [l, r] in buf.as_chunks_mut::<CHANNELS>().0 {
                    (*l, *r) = (ll * *l + rl * *r, lr * *l + rr * *r);
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

#[inline(always)]
fn within_f32(v: f64) -> bool {
    const LIMIT: f64 = f32::MAX as f64;
    (-LIMIT..=LIMIT).contains(&v)
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
        // Octave bandwidth needs the bilinear transform correction. Omitting
        // w0 / sin(w0) increasingly narrows the upper bands compared to FFmpeg.
        let alpha =
            w0.sin() * (std::f64::consts::LN_2 / 2.0 * width_octaves * w0 / w0.sin()).sinh();
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

    #[inline]
    fn step(&mut self, x: f64) -> f64 {
        let x = if x.is_finite() { x } else { 0.0 };
        let mut y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        if !within_f32(y) {
            self.x1 = 0.0;
            self.x2 = 0.0;
            self.y1 = 0.0;
            self.y2 = 0.0;
            return 0.0;
        }
        if y.abs() < 1e-20 {
            y = 0.0;
        }
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// Shared biquad coefficients with 2-lane stereo `[f64; 2]` state so L and R
/// map directly to one 128-bit SIMD register (`__m128d` / `float64x2_t`).
#[derive(Debug, Clone, Copy, Default)]
struct BiquadStereo {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    x1: [f64; CHANNELS],
    x2: [f64; CHANNELS],
    y1: [f64; CHANNELS],
    y2: [f64; CHANNELS],
}

impl BiquadStereo {
    fn from_biquad(b: Biquad) -> Self {
        Self { b0: b.b0, b1: b.b1, b2: b.b2, a1: b.a1, a2: b.a2, ..Default::default() }
    }

    fn process(&mut self, buf: &mut [f32]) {
        let (b0, b1, b2, a1, a2) = (self.b0, self.b1, self.b2, self.a1, self.a2);
        let (mut x1, mut x2, mut y1, mut y2) = (self.x1, self.x2, self.y1, self.y2);
        for [l, r] in buf.as_chunks_mut::<CHANNELS>().0 {
            let x = [
                if l.is_finite() { *l as f64 } else { 0.0 },
                if r.is_finite() { *r as f64 } else { 0.0 },
            ];
            let mut y = [
                b0 * x[0] + b1 * x1[0] + b2 * x2[0] - a1 * y1[0] - a2 * y2[0],
                b0 * x[1] + b1 * x1[1] + b2 * x2[1] - a1 * y1[1] - a2 * y2[1],
            ];
            if within_f32(y[0]) && within_f32(y[1]) {
                if y[0].abs() < 1e-20 {
                    y[0] = 0.0;
                }
                if y[1].abs() < 1e-20 {
                    y[1] = 0.0;
                }
                x2 = x1;
                x1 = x;
                y2 = y1;
                y1 = y;
            } else {
                for ch in 0..CHANNELS {
                    if !within_f32(y[ch]) {
                        x1[ch] = 0.0;
                        x2[ch] = 0.0;
                        y1[ch] = 0.0;
                        y2[ch] = 0.0;
                        y[ch] = 0.0;
                    } else {
                        if y[ch].abs() < 1e-20 {
                            y[ch] = 0.0;
                        }
                        x2[ch] = x1[ch];
                        x1[ch] = x[ch];
                        y2[ch] = y1[ch];
                        y1[ch] = y[ch];
                    }
                }
            }
            *l = y[0] as f32;
            *r = y[1] as f32;
        }
        (self.x1, self.x2, self.y1, self.y2) = (x1, x2, y1, y2);
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
    fn new(bands: &[EqBand]) -> Option<Self> {
        let sections: Vec<BiquadStereo> = bands
            .iter()
            .filter(|b| b.decibels.abs() > 1e-12)
            .map(|b| BiquadStereo::from_biquad(Biquad::peaking(b.frequency, 0.666_667, b.decibels)))
            .collect();
        (!sections.is_empty()).then_some(EqualizerStage { sections })
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
    band: Biquad,
    gain: f64,
}

impl KaraokeStage {
    fn new(k: Karaoke) -> Self {
        KaraokeStage { band: Biquad::bandpass(k.filter_band, k.filter_width), gain: k.gain }
    }

    fn process(&mut self, buf: &mut [f32]) {
        for [l, r] in buf.as_chunks_mut::<CHANNELS>().0 {
            let l0 = if l.is_finite() { *l } else { 0.0 };
            let r0 = if r.is_finite() { *r } else { 0.0 };
            let mid = 0.5 * (l0 as f64 + r0 as f64);
            let cut =
                (self.band.step(mid) * self.gain).clamp(-(f32::MAX as f64), f32::MAX as f64) as f32;
            *l = l0 + cut;
            *r = r0 + cut;
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
        let rate_step = self.frequency / SAMPLE_RATE as f64;
        let (step_s, step_c) = (2.0 * PI * rate_step).sin_cos();
        let half_depth = 0.5 * self.depth;
        for (chunk_idx, chunk) in
            buf.as_chunks_mut::<CHANNELS>().0.chunks_mut(LFO_CHUNK).enumerate()
        {
            let phase0 = ((clock + (chunk_idx * LFO_CHUNK) as u64) as f64 * rate_step).fract();
            let (mut s, mut c) = (2.0 * PI * phase0).sin_cos();
            for [l, r] in chunk {
                let gain = (1.0 - half_depth * (1.0 + s)) as f32;
                *l *= gain;
                *r *= gain;
                (s, c) = (s * step_c + c * step_s, c * step_c - s * step_s);
            }
        }
    }
}

const VIBRATO_MAX_DELAY: usize = (5 * SAMPLE_RATE as usize) / 1000 + 2;
/// Power-of-two ring buffer (`256 >= VIBRATO_MAX_DELAY + 2 = 244`) so index
/// wrapping compiles to `& VIBRATO_MASK` with zero integer divisions or bounds checks.
const VIBRATO_RING: usize = 256;
const VIBRATO_MASK: usize = VIBRATO_RING - 1;

/// Vibrato moves the read position of a short delay line, which is what makes a
/// pitch wobble without changing the tempo.
#[derive(Debug, Clone)]
struct VibratoStage {
    frequency: f64,
    depth: f64,
    write: usize,
    history: [[f32; CHANNELS]; VIBRATO_RING],
}

impl VibratoStage {
    fn new(m: Modulation) -> Self {
        VibratoStage {
            frequency: m.frequency,
            depth: m.depth,
            write: 0,
            history: [[0.0; CHANNELS]; VIBRATO_RING],
        }
    }

    fn process(&mut self, buf: &mut [f32], clock: u64) {
        let rate_step = self.frequency / SAMPLE_RATE as f64;
        let (step_s, step_c) = (2.0 * PI * rate_step).sin_cos();
        let scale = 0.5 * self.depth * VIBRATO_MAX_DELAY as f64;
        let mut w = self.write;
        for (chunk_idx, chunk) in
            buf.as_chunks_mut::<CHANNELS>().0.chunks_mut(LFO_CHUNK).enumerate()
        {
            let phase0 = ((clock + (chunk_idx * LFO_CHUNK) as u64) as f64 * rate_step).fract();
            let (mut s, mut c) = (2.0 * PI * phase0).sin_cos();
            for frame in chunk {
                let delay = scale * (1.0 + s);
                let whole = delay.floor() as usize;
                let fraction = (delay - whole as f64) as f32;
                let clean = [
                    if frame[0].is_finite() { frame[0] } else { 0.0 },
                    if frame[1].is_finite() { frame[1] } else { 0.0 },
                ];
                self.history[w & VIBRATO_MASK] = clean;
                let [a0, a1] = self.history[w.wrapping_sub(whole) & VIBRATO_MASK];
                let [b0, b1] = self.history[w.wrapping_sub(whole + 1) & VIBRATO_MASK];
                frame[0] = a0 + (b0 - a0) * fraction;
                frame[1] = a1 + (b1 - a1) * fraction;
                w = (w + 1) & VIBRATO_MASK;
                (s, c) = (s * step_c + c * step_s, c * step_c - s * step_s);
            }
        }
        self.write = w;
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
        let rate_step = self.hertz / SAMPLE_RATE as f64;
        let (step_s, step_c) = (2.0 * PI * rate_step).sin_cos();
        for (chunk_idx, chunk) in
            buf.as_chunks_mut::<CHANNELS>().0.chunks_mut(LFO_CHUNK).enumerate()
        {
            let phase0 = ((clock + (chunk_idx * LFO_CHUNK) as u64) as f64 * rate_step).fract();
            let (mut s, mut c) = (2.0 * PI * phase0).sin_cos();
            for [l, r] in chunk {
                *l *= (0.5 + 0.5 * c) as f32;
                *r *= (0.5 - 0.5 * c) as f32;
                (s, c) = (s * step_c + c * step_s, c * step_c - s * step_s);
            }
        }
    }
}

/// The waveshaper the protocol exposes, evaluated the way the FFmpeg `aeval`
/// expression is, with track-constant zero-scale trig factors pre-folded.
#[derive(Debug, Clone, Copy)]
struct DistortionStage {
    d: Distortion,
    eff_scale: f64,
    const_out: f32,
    shared_sin_cos: bool,
}

impl DistortionStage {
    fn new(d: Distortion) -> Self {
        let mut eff_scale = d.scale;
        if d.sin_scale == 0.0 {
            eff_scale *= d.sin_offset.sin();
        }
        if d.cos_scale == 0.0 {
            eff_scale *= d.cos_offset.cos();
        }
        if d.tan_scale == 0.0 {
            eff_scale *= d.tan_offset.tan();
        }
        let const_y = eff_scale * 0.0 + d.offset;
        let const_out = if const_y.is_finite() { const_y.clamp(-1.0, 1.0) as f32 } else { 0.0 };
        let shared_sin_cos =
            d.sin_scale != 0.0 && d.sin_scale == d.cos_scale && d.sin_offset == d.cos_offset;
        DistortionStage { d, eff_scale, const_out, shared_sin_cos }
    }

    fn process(&mut self, buf: &mut [f32]) {
        if self.eff_scale == 0.0 {
            buf.fill(self.const_out);
            return;
        }
        let d = &self.d;
        let eff_scale = self.eff_scale;
        for sample in buf.iter_mut() {
            let x = if sample.is_finite() { *sample as f64 } else { 0.0 };
            let sc = if self.shared_sin_cos {
                let (s, c) = (x * d.sin_scale + d.sin_offset).sin_cos();
                s * c
            } else {
                let s =
                    if d.sin_scale != 0.0 { (x * d.sin_scale + d.sin_offset).sin() } else { 1.0 };
                let c =
                    if d.cos_scale != 0.0 { (x * d.cos_scale + d.cos_offset).cos() } else { 1.0 };
                s * c
            };
            let t = if d.tan_scale != 0.0 { (x * d.tan_scale + d.tan_offset).tan() } else { 1.0 };
            let y = eff_scale * (sc * t) + d.offset;
            *sample = if y.is_finite() { y.clamp(-1.0, 1.0) as f32 } else { 0.0 };
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
        let alpha = self.alpha;
        let [mut s0, mut s1] = self.state;
        for [l, r] in buf.as_chunks_mut::<CHANNELS>().0 {
            let x0 = if l.is_finite() { *l as f64 } else { 0.0 };
            let x1 = if r.is_finite() { *r as f64 } else { 0.0 };
            s0 += alpha * (x0 - s0);
            s1 += alpha * (x1 - s1);
            if !within_f32(s0) || s0.abs() < 1e-20 {
                s0 = 0.0;
            }
            if !within_f32(s1) || s1.abs() < 1e-20 {
                s1 = 0.0;
            }
            *l = s0 as f32;
            *r = s1 as f32;
        }
        self.state = [s0, s1];
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

    #[test]
    fn reconfigure_preserves_clock_and_recovers_from_nan_samples() {
        let mut chain = chain(serde_json::json!({"equalizer": [{"band": 8, "gain": 0.5}]}));
        let mut poison = vec![f32::NAN, f32::INFINITY, 0.25, -0.25];
        chain.process_pre(&mut poison);
        assert!(poison.iter().all(|s| s.is_finite()));

        let mut next = parse(&Object::default(), 50).unwrap();
        next.bands = chain.config.bands.clone();
        chain.reconfigure(&next);
        let mut clean = vec![0.5f32, -0.5];
        chain.process_pre(&mut clean);
        assert!(clean.iter().all(|s| s.is_finite()));
    }
}
