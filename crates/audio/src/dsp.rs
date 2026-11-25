//! In-process stereo 48 kHz DSP filter chain split around the timescale stage.

use std::f64::consts::{PI, TAU};

use crate::filters::{
    ChannelMix, Distortion, EqBand, FilterSet, Karaoke, LowPass, Modulation, Rotation,
};

/// Playback sample rate (Hz).
pub const SAMPLE_RATE: u32 = 48_000;
/// Playback channel count (stereo).
pub const CHANNELS: usize = 2;
/// Duration of one Opus frame (ms).
pub const FRAME_MILLIS: u64 = 20;
/// Samples per channel in one 20 ms frame (`960`).
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize / 1000) * FRAME_MILLIS as usize;

/// Pre- and post-timescale DSP chain for one playback.
#[derive(Debug, Default)]
pub struct DspChain {
    config: FilterSet,
    pre: Vec<Stage>,
    post: Vec<Stage>,
}

/// Flushes `NaN`, `+-Inf`, `-0.0`, subnormals, and magnitudes below `1e-20` to `+0.0`
/// using integer bit arithmetic without triggering x86 SSE denormal assists.
#[inline(always)]
pub(crate) fn clean_f32(s: f32) -> f32 {
    const MIN_BITS: u32 = 1e-20_f32.to_bits();
    const INF_BITS: u32 = f32::INFINITY.to_bits();
    let bits = s.to_bits();
    let keep = (bits & 0x7fff_ffff).wrapping_sub(MIN_BITS) < INF_BITS - MIN_BITS;
    f32::from_bits(if keep { bits } else { 0 })
}

#[inline(always)]
fn finite_or(v: f64, fallback: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        fallback
    }
}

#[inline(always)]
fn saturate_f32(y: f64) -> f32 {
    const LIMIT: f64 = f32::MAX as f64;
    if y.is_nan() || y.abs() < 1e-20 {
        0.0
    } else {
        y.clamp(-LIMIT, LIMIT) as f32
    }
}

#[inline(always)]
fn clip_unit_f32(y: f64) -> f32 {
    if y.is_finite() && y.abs() >= 1e-20 {
        y.clamp(-1.0, 1.0) as f32
    } else {
        0.0
    }
}

#[inline]
fn clamp_biquad_f0(f0: f64) -> f64 {
    finite_or(f0, 1000.0).clamp(1.0, SAMPLE_RATE as f64 * 0.5 - 1.0)
}

#[inline]
fn clamp_depth(depth: f64) -> f64 {
    finite_or(depth, 0.0).clamp(0.0, 1.0)
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

fn is_identity_mix(m: &ChannelMix) -> bool {
    (m.left_to_left, m.left_to_right, m.right_to_left, m.right_to_right) == (1.0, 0.0, 0.0, 1.0)
}

impl DspChain {
    pub fn new(set: &FilterSet) -> Self {
        let mut chain = Self::default();
        chain.reconfigure(set);
        chain
    }

    /// Updates volume and filters in place, preserving LFO phase and filter state histories.
    pub fn reconfigure(&mut self, set: &FilterSet) {
        if self.config == *set {
            return;
        }
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
            self.config.time = set.time;
            return;
        }
        let (mut old_eq, mut old_karaoke) = (None, None);
        for stage in self.pre.drain(..) {
            match stage {
                Stage::Equalizer(eq) => old_eq = Some(eq),
                Stage::Karaoke(k) => old_karaoke = Some(k),
                _ => {}
            }
        }
        let (mut old_tremolo, mut old_vibrato, mut old_rotation, mut old_low_pass) =
            (None, None, None, None);
        for stage in self.post.drain(..) {
            match stage {
                Stage::Tremolo(t) => old_tremolo = Some(t),
                Stage::Vibrato(v) => old_vibrato = Some(v),
                Stage::Rotation(r) => old_rotation = Some(r),
                Stage::LowPass(lp) => old_low_pass = Some(lp),
                _ => {}
            }
        }
        if set.gain != 1.0 {
            self.pre.push(Stage::Volume(set.gain));
        }
        if let Some(mut eq) = EqualizerStage::new(&set.bands) {
            if let Some(prev) = old_eq {
                for dst in &mut eq.sections {
                    if let Some(src) =
                        prev.sections.iter().find(|s| s.frequency_bits == dst.frequency_bits)
                    {
                        (dst.x1, dst.x2, dst.y1, dst.y2) = (src.x1, src.x2, src.y1, src.y2);
                    }
                }
            }
            self.pre.push(Stage::Equalizer(eq));
        }
        if let Some(k) = set.karaoke.filter(|k| k.gain.abs() > 1e-12) {
            let mut stage = KaraokeStage::new(k);
            if let Some(prev) = old_karaoke {
                (stage.band.x1, stage.band.x2, stage.band.y1, stage.band.y2) =
                    (prev.band.x1, prev.band.x2, prev.band.y1, prev.band.y2);
            }
            self.pre.push(Stage::Karaoke(stage));
        }
        if let Some(t) = set.tremolo.filter(|m| clamp_depth(m.depth) > 0.0) {
            let mut stage = TremoloStage::new(t);
            if let Some(prev) = old_tremolo {
                stage.phase = prev.phase;
            }
            self.post.push(Stage::Tremolo(stage));
        }
        if let Some(v) = set.vibrato.filter(|m| clamp_depth(m.depth) > 0.0) {
            let stage = match old_vibrato {
                Some(mut prev) => {
                    prev.frequency = v.frequency;
                    prev.depth = clamp_depth(v.depth);
                    prev
                }
                None => Box::new(VibratoStage::new(v)),
            };
            self.post.push(Stage::Vibrato(stage));
        }
        if let Some(r) = set.rotation {
            let mut stage = RotationStage::new(r);
            if let Some(prev) = old_rotation {
                stage.phase = prev.phase;
            }
            self.post.push(Stage::Rotation(stage));
        }
        if let Some(d) = set.distort {
            self.post.push(Stage::Distortion(DistortionStage::new(d)));
        }
        if let Some(m) = set.mix.filter(|m| !is_identity_mix(m)) {
            self.post.push(Stage::ChannelMix(m));
        }
        if let Some(l) = set.low_pass {
            let mut stage = OnePoleStereo::new(l);
            if let Some(prev) = old_low_pass {
                stage.state = prev.state;
            }
            self.post.push(Stage::LowPass(stage));
        }
        self.config = set.clone();
    }

    pub fn is_empty(&self) -> bool {
        self.pre.is_empty() && self.post.is_empty()
    }

    pub fn process_pre(&mut self, buf: &mut [f32]) {
        for stage in &mut self.pre {
            stage.apply(buf);
        }
    }

    pub fn process_post(&mut self, buf: &mut [f32]) {
        for stage in &mut self.post {
            stage.apply(buf);
        }
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
    fn apply(&mut self, buf: &mut [f32]) {
        match self {
            Self::Volume(gain) => {
                let g = finite_or(*gain, 0.0);
                for sample in buf {
                    *sample = saturate_f32(clean_f32(*sample) as f64 * g);
                }
            }
            Self::Equalizer(stage) => stage.process(buf),
            Self::Karaoke(stage) => stage.process(buf),
            Self::Tremolo(stage) => stage.process(buf),
            Self::Vibrato(stage) => stage.process(buf),
            Self::Rotation(stage) => stage.process(buf),
            Self::Distortion(stage) => stage.process(buf),
            Self::ChannelMix(mix) => {
                let (ll, lr, rl, rr) = (
                    finite_or(mix.left_to_left, 0.0),
                    finite_or(mix.left_to_right, 0.0),
                    finite_or(mix.right_to_left, 0.0),
                    finite_or(mix.right_to_right, 0.0),
                );
                for [l, r] in buf.as_chunks_mut::<CHANNELS>().0 {
                    let (l0, r0) = (clean_f32(*l) as f64, clean_f32(*r) as f64);
                    *l = saturate_f32(ll * l0 + rl * r0);
                    *r = saturate_f32(lr * l0 + rr * r0);
                }
            }
            Self::LowPass(stage) => stage.process(buf),
        }
    }
}

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
        for c in [&mut self.b0, &mut self.b1, &mut self.b2, &mut self.a1, &mut self.a2] {
            *c /= a0;
        }
        self
    }

    fn peaking(f0: f64, width_octaves: f64, decibels: f64) -> Self {
        let f0 = clamp_biquad_f0(f0);
        let width_octaves = finite_or(width_octaves, 0.666_667).clamp(0.01, 10.0);
        let decibels = finite_or(decibels, 0.0).clamp(-120.0, 120.0);
        let a = 10f64.powf(decibels / 40.0);
        let w0 = TAU * f0 / SAMPLE_RATE as f64;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 * (std::f64::consts::LN_2 * 0.5 * width_octaves * w0 / sin_w0).sinh();
        Self {
            b0: 1.0 + alpha * a,
            b1: -2.0 * cos_w0,
            b2: 1.0 - alpha * a,
            a1: -2.0 * cos_w0,
            a2: 1.0 - alpha / a,
            ..Default::default()
        }
        .normalized(1.0 + alpha / a)
    }

    fn bandpass(f0: f64, width_hz: f64) -> Self {
        let f0 = clamp_biquad_f0(f0);
        let width_hz = finite_or(width_hz, 100.0).max(1.0);
        let w0 = TAU * f0 / SAMPLE_RATE as f64;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * (f0 / width_hz).max(0.05));
        Self {
            b0: alpha,
            b1: 0.0,
            b2: -alpha,
            a1: -2.0 * cos_w0,
            a2: 1.0 - alpha,
            ..Default::default()
        }
        .normalized(1.0 + alpha)
    }

    #[inline]
    fn step(&mut self, x: f64) -> f64 {
        let x = if x.is_finite() && x.abs() >= 1e-20 { x } else { 0.0 };
        let mut y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        if !within_f32(y) {
            (self.x1, self.x2, self.y1, self.y2) = (0.0, 0.0, 0.0, 0.0);
            return 0.0;
        }
        if y.abs() < 1e-20 {
            y = 0.0;
        }
        (self.x2, self.x1, self.y2, self.y1) = (self.x1, x, self.y1, y);
        y
    }
}

/// Shared biquad coefficients with 2-lane stereo `[f64; 2]` state (`__m128d` / `float64x2_t`).
#[derive(Debug, Clone, Copy, Default)]
struct BiquadStereo {
    frequency_bits: u64,
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
    fn from_biquad(frequency: f64, b: Biquad) -> Self {
        Self {
            frequency_bits: frequency.to_bits(),
            b0: b.b0,
            b1: b.b1,
            b2: b.b2,
            a1: b.a1,
            a2: b.a2,
            ..Default::default()
        }
    }

    fn process(&mut self, buf: &mut [f32]) {
        let (b0, b1, b2, a1, a2) = (self.b0, self.b1, self.b2, self.a1, self.a2);
        let (mut x1, mut x2, mut y1, mut y2) = (self.x1, self.x2, self.y1, self.y2);
        for [l, r] in buf.as_chunks_mut::<CHANNELS>().0 {
            let x = [clean_f32(*l) as f64, clean_f32(*r) as f64];
            let mut y = [
                b0 * x[0] + b1 * x1[0] + b2 * x2[0] - a1 * y1[0] - a2 * y2[0],
                b0 * x[1] + b1 * x1[1] + b2 * x2[1] - a1 * y1[1] - a2 * y2[1],
            ];
            if within_f32(y[0]) & within_f32(y[1]) {
                if y[0].abs() < 1e-20 {
                    y[0] = 0.0;
                }
                if y[1].abs() < 1e-20 {
                    y[1] = 0.0;
                }
                (x2, x1, y2, y1) = (x1, x, y1, y);
            } else {
                for ch in 0..CHANNELS {
                    if !within_f32(y[ch]) {
                        (x1[ch], x2[ch], y1[ch], y2[ch], y[ch]) = (0.0, 0.0, 0.0, 0.0, 0.0);
                    } else {
                        if y[ch].abs() < 1e-20 {
                            y[ch] = 0.0;
                        }
                        (x2[ch], x1[ch], y2[ch], y1[ch]) = (x1[ch], x[ch], y1[ch], y[ch]);
                    }
                }
            }
            (*l, *r) = (y[0] as f32, y[1] as f32);
        }
        (self.x1, self.x2, self.y1, self.y2) = (x1, x2, y1, y2);
    }
}

#[derive(Debug, Clone)]
struct EqualizerStage {
    sections: Vec<BiquadStereo>,
}

impl EqualizerStage {
    fn new(bands: &[EqBand]) -> Option<Self> {
        let sections: Vec<BiquadStereo> = bands
            .iter()
            .filter(|b| b.decibels.abs() > 1e-12)
            .map(|b| {
                BiquadStereo::from_biquad(
                    b.frequency,
                    Biquad::peaking(b.frequency, 0.666_667, b.decibels),
                )
            })
            .collect();
        (!sections.is_empty()).then_some(Self { sections })
    }

    fn process(&mut self, buf: &mut [f32]) {
        for section in &mut self.sections {
            section.process(buf);
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct KaraokeStage {
    band: Biquad,
    gain: f64,
}

impl KaraokeStage {
    fn new(k: Karaoke) -> Self {
        Self { band: Biquad::bandpass(k.filter_band, k.filter_width), gain: k.gain }
    }

    fn process(&mut self, buf: &mut [f32]) {
        let gain = finite_or(self.gain, 0.0);
        for [l, r] in buf.as_chunks_mut::<CHANNELS>().0 {
            let (l0, r0) = (clean_f32(*l) as f64, clean_f32(*r) as f64);
            let cut = self.band.step(0.5 * (l0 + r0)) * gain;
            *l = saturate_f32(l0 + cut);
            *r = saturate_f32(r0 + cut);
        }
    }
}

#[inline(always)]
fn for_each_lfo_chunk(
    buf: &mut [f32],
    phase: &mut f64,
    hertz: f64,
    mut body: impl FnMut(&mut [[f32; CHANNELS]], f64, f64, f64, f64),
) {
    let rate_step = finite_or(hertz / SAMPLE_RATE as f64, 0.0);
    let (step_s, step_c) = (TAU * rate_step).sin_cos();
    for chunk in buf.as_chunks_mut::<CHANNELS>().0.chunks_mut(LFO_CHUNK) {
        let (s, c) = (TAU * *phase).sin_cos();
        body(chunk, s, c, step_s, step_c);
        *phase = (*phase + chunk.len() as f64 * rate_step).rem_euclid(1.0);
    }
}

#[derive(Debug, Clone, Copy)]
struct TremoloStage {
    frequency: f64,
    depth: f64,
    phase: f64,
}

impl TremoloStage {
    fn new(m: Modulation) -> Self {
        Self { frequency: m.frequency, depth: clamp_depth(m.depth), phase: 0.0 }
    }

    fn process(&mut self, buf: &mut [f32]) {
        let half_depth = 0.5 * self.depth;
        for_each_lfo_chunk(
            buf,
            &mut self.phase,
            self.frequency,
            |chunk, mut s, mut c, step_s, step_c| {
                for [l, r] in chunk {
                    let gain = (1.0 - half_depth * (1.0 + s)) as f32;
                    *l = clean_f32(clean_f32(*l) * gain);
                    *r = clean_f32(clean_f32(*r) * gain);
                    (s, c) = (s * step_c + c * step_s, c * step_c - s * step_s);
                }
            },
        );
    }
}

const VIBRATO_MAX_DELAY: usize = (5 * SAMPLE_RATE as usize) / 1000 + 2;
const VIBRATO_RING: usize = 256;
const VIBRATO_MASK: usize = VIBRATO_RING - 1;

#[derive(Debug, Clone)]
struct VibratoStage {
    frequency: f64,
    depth: f64,
    phase: f64,
    write: usize,
    history: [[f32; CHANNELS]; VIBRATO_RING],
}

impl VibratoStage {
    fn new(m: Modulation) -> Self {
        Self {
            frequency: m.frequency,
            depth: clamp_depth(m.depth),
            phase: 0.0,
            write: 0,
            history: [[0.0; CHANNELS]; VIBRATO_RING],
        }
    }

    fn process(&mut self, buf: &mut [f32]) {
        let scale = 0.5 * self.depth * VIBRATO_MAX_DELAY as f64;
        let mut w = self.write;
        for_each_lfo_chunk(
            buf,
            &mut self.phase,
            self.frequency,
            |chunk, mut s, mut c, step_s, step_c| {
                for frame in chunk {
                    let delay = (scale * (1.0 + s)).clamp(0.0, VIBRATO_MAX_DELAY as f64);
                    let whole = delay as usize;
                    let fraction = delay - whole as f64;
                    self.history[w & VIBRATO_MASK] = [clean_f32(frame[0]), clean_f32(frame[1])];
                    let [a0, a1] = self.history[w.wrapping_sub(whole) & VIBRATO_MASK];
                    let [b0, b1] = self.history[w.wrapping_sub(whole + 1) & VIBRATO_MASK];
                    frame[0] = saturate_f32(a0 as f64 + (b0 as f64 - a0 as f64) * fraction);
                    frame[1] = saturate_f32(a1 as f64 + (b1 as f64 - a1 as f64) * fraction);
                    w = (w + 1) & VIBRATO_MASK;
                    (s, c) = (s * step_c + c * step_s, c * step_c - s * step_s);
                }
            },
        );
        self.write = w;
    }
}

#[derive(Debug, Clone, Copy)]
struct RotationStage {
    hertz: f64,
    phase: f64,
}

impl RotationStage {
    fn new(r: Rotation) -> Self {
        Self { hertz: r.hertz, phase: 0.0 }
    }

    fn process(&mut self, buf: &mut [f32]) {
        for_each_lfo_chunk(
            buf,
            &mut self.phase,
            self.hertz,
            |chunk, mut s, mut c, step_s, step_c| {
                for [l, r] in chunk {
                    *l = clean_f32(clean_f32(*l) * (0.5 + 0.5 * c) as f32);
                    *r = clean_f32(clean_f32(*r) * (0.5 - 0.5 * c) as f32);
                    (s, c) = (s * step_c + c * step_s, c * step_c - s * step_s);
                }
            },
        );
    }
}

#[derive(Debug, Clone, Copy)]
enum TrigMode {
    Constant(f32),
    SinSquared,
    HalfSinDouble,
    HalfSinDoubleTan,
    General,
}

#[derive(Debug, Clone, Copy)]
struct DistortionStage {
    d: Distortion,
    eff_scale: f64,
    mode: TrigMode,
}

#[inline(always)]
fn apply_wave(buf: &mut [f32], mut wave: impl FnMut(f64) -> f64) {
    for sample in buf {
        *sample = clip_unit_f32(wave(clean_f32(*sample) as f64));
    }
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
        let all_const = d.sin_scale == 0.0 && d.cos_scale == 0.0 && d.tan_scale == 0.0;
        let mode = if eff_scale == 0.0 || all_const {
            TrigMode::Constant(clip_unit_f32(if eff_scale == 0.0 {
                d.offset
            } else {
                eff_scale + d.offset
            }))
        } else if d.sin_scale != 0.0 && d.sin_scale == d.cos_scale && d.sin_offset == d.cos_offset {
            if d.tan_scale == 0.0 {
                TrigMode::HalfSinDouble
            } else if d.tan_scale == d.sin_scale && d.tan_offset == d.sin_offset {
                TrigMode::SinSquared
            } else {
                TrigMode::HalfSinDoubleTan
            }
        } else {
            TrigMode::General
        };
        Self { d, eff_scale, mode }
    }

    fn process(&mut self, buf: &mut [f32]) {
        let (d, k) = (&self.d, self.eff_scale);
        match self.mode {
            TrigMode::Constant(out) => buf.fill(out),
            TrigMode::SinSquared => apply_wave(buf, |x| {
                let s = (x * d.sin_scale + d.sin_offset).sin();
                k * (s * s) + d.offset
            }),
            TrigMode::HalfSinDouble => apply_wave(buf, |x| {
                k * 0.5 * (2.0 * (x * d.sin_scale + d.sin_offset)).sin() + d.offset
            }),
            TrigMode::HalfSinDoubleTan => apply_wave(buf, |x| {
                let sc = 0.5 * (2.0 * (x * d.sin_scale + d.sin_offset)).sin();
                k * (sc * (x * d.tan_scale + d.tan_offset).tan()) + d.offset
            }),
            TrigMode::General => apply_wave(buf, |x| {
                let s =
                    if d.sin_scale != 0.0 { (x * d.sin_scale + d.sin_offset).sin() } else { 1.0 };
                let c =
                    if d.cos_scale != 0.0 { (x * d.cos_scale + d.cos_offset).cos() } else { 1.0 };
                let t =
                    if d.tan_scale != 0.0 { (x * d.tan_scale + d.tan_offset).tan() } else { 1.0 };
                k * (s * c * t) + d.offset
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct OnePoleStereo {
    alpha: f64,
    state: [f64; CHANNELS],
}

impl OnePoleStereo {
    fn new(l: LowPass) -> Self {
        let alpha = if !l.frequency.is_finite() {
            1.0
        } else if l.frequency <= 0.0 {
            0.0
        } else {
            (1.0 - (-2.0 * PI * l.frequency / SAMPLE_RATE as f64).exp()).clamp(0.0, 1.0)
        };
        Self { alpha, state: [0.0; CHANNELS] }
    }

    fn process(&mut self, buf: &mut [f32]) {
        let alpha = self.alpha;
        let [mut s0, mut s1] = self.state;
        for [l, r] in buf.as_chunks_mut::<CHANNELS>().0 {
            s0 += alpha * (clean_f32(*l) as f64 - s0);
            s1 += alpha * (clean_f32(*r) as f64 - s1);
            if !within_f32(s0) || s0.abs() < 1e-20 {
                s0 = 0.0;
            }
            if !within_f32(s1) || s1.abs() < 1e-20 {
                s1 = 0.0;
            }
            (*l, *r) = (s0 as f32, s1 as f32);
        }
        self.state = [s0, s1];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filters::parse;
    use rustlink_protocol::Object;
    use serde_json::json;

    fn chain(value: serde_json::Value) -> DspChain {
        let serde_json::Value::Object(object) = value else { panic!("not an object") };
        DspChain::new(&parse(&object, 100).unwrap())
    }

    fn tone(samples: usize, amplitude: f32) -> Vec<f32> {
        (0..samples * CHANNELS).map(|i| if i & 1 == 0 { amplitude } else { -amplitude }).collect()
    }

    fn sine_stereo(freq: f64, amplitude: f32, right_sign: f32) -> Vec<f32> {
        (0..48_000)
            .flat_map(|i| {
                let v = amplitude * (TAU * freq * i as f64 / SAMPLE_RATE as f64).sin() as f32;
                [v, v * right_sign]
            })
            .collect()
    }

    #[test]
    fn an_empty_chain_leaves_samples_alone() {
        let mut chain = chain(json!({}));
        assert!(chain.is_empty());
        let mut buf = tone(64, 0.5);
        let original = buf.clone();
        chain.process_pre(&mut buf);
        chain.process_post(&mut buf);
        assert_eq!(buf, original);
    }

    #[test]
    fn volume_scales_every_sample() {
        let mut chain = chain(json!({"volume": 0.5}));
        let mut buf = vec![1.0f32, -1.0, 0.5, -0.5];
        chain.process_pre(&mut buf);
        assert_eq!(buf, vec![0.5, -0.5, 0.25, -0.25]);
    }

    #[test]
    fn the_player_volume_also_scales() {
        let mut chain = DspChain::new(&parse(&Object::new(), 50).unwrap());
        let mut buf = vec![1.0f32, 1.0];
        chain.process_pre(&mut buf);
        assert_eq!(buf, vec![0.5, 0.5]);
    }

    #[test]
    fn a_channel_mix_rewrites_the_matrix() {
        let mut chain = chain(json!({
            "channelMix": {"leftToRight": 1.0, "rightToLeft": 1.0, "leftToLeft": 0.0, "rightToRight": 0.0}
        }));
        let mut buf = vec![1.0f32, 0.0, 0.0, 1.0];
        chain.process_post(&mut buf);
        assert_eq!(buf, vec![0.0, 1.0, 1.0, 0.0]);
    }

    #[test]
    fn a_low_pass_settles_a_constant_signal() {
        let mut chain = chain(json!({"lowPass": {"smoothing": 20}}));
        let mut buf = vec![1.0f32; 20_000];
        chain.process_post(&mut buf);
        assert!((buf[buf.len() - 1] - 1.0).abs() < 1e-3 && buf[0] < 1.0);
    }

    #[test]
    fn a_boosted_band_raises_its_own_frequency() {
        let mut chain = chain(json!({"equalizer": [{"band": 8, "gain": 1.0}]}));
        let mut buf = sine_stereo(1000.0, 1.0, 1.0);
        let peak_before = buf.iter().copied().fold(f32::MIN, f32::max);
        chain.process_pre(&mut buf);
        let peak_after = buf.iter().copied().fold(f32::MIN, f32::max);
        assert!(peak_after > peak_before * 3.0, "{peak_after} should be well above {peak_before}");
    }

    #[test]
    fn a_flat_band_leaves_the_signal_alone() {
        let mut chain = chain(json!({"equalizer": [{"band": 8, "gain": 0.0}]}));
        let mut buf = tone(512, 0.25);
        let original = buf.clone();
        chain.process_pre(&mut buf);
        for (a, b) in buf.iter().zip(&original) {
            assert!((a - b).abs() < 1e-6, "{a} != {b}");
        }
    }

    #[test]
    fn karaoke_removes_the_centre_channel() {
        let mut chain = chain(json!({
            "karaoke": {"level": 1.0, "monoLevel": 1.0, "filterBand": 220, "filterWidth": 100}
        }));
        let mut buf = sine_stereo(220.0, 0.5, 1.0);
        chain.process_pre(&mut buf);
        let peak = buf[buf.len() - 4800..].iter().fold(0f32, |acc, v| acc.max(v.abs()));
        assert!(peak < 0.05, "centre channel survived at {peak}");
    }

    #[test]
    fn karaoke_leaves_what_is_not_in_the_centre_alone() {
        let mut chain = chain(json!({
            "karaoke": {"level": 1.0, "monoLevel": 1.0, "filterBand": 220, "filterWidth": 100}
        }));
        let mut buf = sine_stereo(220.0, 0.5, -1.0);
        let before = buf[buf.len() - 2];
        chain.process_pre(&mut buf);
        let after = buf[buf.len() - 2];
        assert!((before - after).abs() < 1e-6, "{before} became {after}");
    }

    #[test]
    fn tremolo_modulates_the_gain_between_one_and_one_minus_the_depth() {
        let mut chain = chain(json!({"tremolo": {"frequency": 5, "depth": 1.0}}));
        let mut buf = vec![1.0f32; 48_000 * 2];
        chain.process_post(&mut buf);
        let peak = buf.iter().copied().fold(f32::MIN, f32::max);
        let trough = buf.iter().copied().fold(f32::MAX, f32::min);
        assert!(peak > 0.98 && trough < 0.02, "peak={peak}, trough={trough}");
    }

    #[test]
    fn rotation_keeps_the_total_energy() {
        let mut chain = chain(json!({"rotation": {"rotationHz": 1.0}}));
        let mut buf = tone(48_000, 1.0);
        chain.process_post(&mut buf);
        let peak = buf.iter().fold(0f32, |acc, v| acc.max(v.abs()));
        assert!(peak <= 1.0001, "peak {peak} should not exceed full scale");
    }

    #[test]
    fn distortion_is_clipped_to_full_scale() {
        let mut chain =
            chain(json!({"distortion": {"scale": 10.0, "sinScale": 10.0, "tanScale": 10.0}}));
        let mut buf = tone(256, 0.9);
        chain.process_post(&mut buf);
        assert!(buf.iter().all(|s| s.abs() <= 1.0));
    }

    #[test]
    fn reconfigure_preserves_clock_and_recovers_from_nan_samples() {
        let mut chain = chain(json!({"equalizer": [{"band": 8, "gain": 0.5}]}));
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

    #[test]
    fn clean_f32_flushes_non_finite_and_subnormals_without_altering_audio() {
        for zeroed in [
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::from_bits(1),
            -f32::from_bits(1),
            1e-25,
        ] {
            assert_eq!(clean_f32(zeroed), 0.0);
        }
        assert_eq!(clean_f32(-0.0).to_bits(), 0.0f32.to_bits());
        for exact in [0.5f32, -0.5, f32::MAX, -f32::MAX] {
            assert_eq!(clean_f32(exact), exact);
        }
    }

    #[test]
    fn karaoke_volume_and_channel_mix_saturate_instead_of_overflowing_to_infinity() {
        let vol_set = FilterSet {
            gain: 4.0,
            karaoke: Some(Karaoke { filter_band: 220.0, filter_width: 100.0, gain: -2.0 }),
            mix: Some(ChannelMix {
                left_to_left: 1.0,
                left_to_right: 1.0,
                right_to_left: 1.0,
                right_to_right: 1.0,
            }),
            ..Default::default()
        };
        let mut chain = DspChain::new(&vol_set);
        let mut buf = vec![f32::MAX; 64];
        chain.process_pre(&mut buf);
        chain.process_post(&mut buf);
        assert!(buf.iter().all(|s| s.is_finite() && *s > 0.0));
    }

    #[test]
    fn reconfigure_preserves_lfo_phase_and_syncs_neutral_timescale() {
        let mut set = FilterSet {
            tremolo: Some(Modulation { frequency: 2.0, depth: 1.0 }),
            vibrato: Some(Modulation { frequency: 3.0, depth: 0.5 }),
            rotation: Some(Rotation { hertz: 1.5 }),
            ..Default::default()
        };
        let mut chain = DspChain::new(&set);
        chain.process_post(&mut vec![1.0f32; 3600 * CHANNELS]);

        let mut trem_only = FilterSet {
            tremolo: Some(Modulation { frequency: 2.0, depth: 1.0 }),
            ..Default::default()
        };
        let mut ref_chain = DspChain::new(&trem_only);
        ref_chain.process_post(&mut vec![1.0f32; 3600 * CHANNELS]);
        let mut probe_same = DspChain::new(&trem_only);
        probe_same.process_post(&mut vec![1.0f32; 3600 * CHANNELS]);
        let mut before_reconf = [1.0f32; CHANNELS];
        probe_same.process_post(&mut before_reconf);

        trem_only.tremolo = Some(Modulation { frequency: 13.0, depth: 1.0 });
        ref_chain.reconfigure(&trem_only);
        let mut after_reconf = [1.0f32; CHANNELS];
        ref_chain.process_post(&mut after_reconf);
        assert!((after_reconf[0] - before_reconf[0]).abs() < 1e-6);

        set.gain = 0.8;
        set.time = Some(crate::filters::Timescale { speed: 1.0, pitch: 1.0, rate: 1.0 });
        chain.reconfigure(&set);
        assert_eq!(chain.config, set);
    }

    #[test]
    fn out_of_range_biquad_modulator_and_lowpass_parameters_clamp_safely() {
        for bad_f0 in [0.0, -50.0, 24_000.0, 96_000.0, f64::NAN, f64::INFINITY] {
            assert!(Biquad::peaking(bad_f0, 0.666_667, 6.0).step(0.5).is_finite());
            assert!(Biquad::bandpass(bad_f0, 0.0).step(0.5).is_finite());
        }
        let mut set = FilterSet {
            tremolo: Some(Modulation { frequency: 5.0, depth: 50.0 }),
            vibrato: Some(Modulation { frequency: 5.0, depth: 50.0 }),
            low_pass: Some(LowPass { smoothing: 1.0, frequency: f64::INFINITY }),
            ..Default::default()
        };
        let mut chain = DspChain::new(&set);
        let mut buf = tone(512, 0.5);
        chain.process_post(&mut buf);
        assert!(buf.iter().all(|s| s.is_finite()));

        set.low_pass = Some(LowPass { smoothing: 1.0, frequency: -10.0 });
        chain.reconfigure(&set);
        chain.process_post(&mut buf);
        assert!(buf.iter().all(|s| s.is_finite()));
    }
}
