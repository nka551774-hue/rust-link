//! 32-tap 4-term Blackman-Harris sinc resampler and WSOLA time stretcher.

use std::f64::consts::PI;

use crate::dsp::CHANNELS;

/// Window taps when not downsampling (`32` taps, >92 dB stopband rejection).
const TAPS: usize = 32;
/// Maximum half-window length when anti-alias downsampling (`32:1` max expansion).
const MAX_HALF: usize = 512;
/// Byte budget for pre-computed polyphase weight tables.
const MAX_TABLE_BYTES: usize = 256 * 1024;
/// Maximum polyphase period tabulated.
const MAX_PHASES: usize = 1024;

/// Windowed sinc stereo sample-rate converter.
#[derive(Debug, Clone)]
pub struct SincResampler {
    ratio: f64,
    position: f64,
    input_frames: u64,
    output_frames: u64,
    finished: bool,
    half: usize,
    scratch: Vec<f32>,
    phase: usize,
    centre_frame: usize,
    phases: Option<Phases>,
    buffer: Vec<f32>,
}

impl SincResampler {
    /// `ratio` is input frames per output frame (`44_100 / 48_000 = 0.91875`).
    pub fn new(ratio: f64) -> Self {
        let mut ratio = if ratio.is_finite() { ratio.clamp(1e-4, 1e4) } else { 1.0 };
        if (ratio - 1.0).abs() < 1e-12 {
            ratio = 1.0;
        }
        let cutoff = if ratio > 1.0 { 1.0 / ratio } else { 1.0 };
        let half = half_window(cutoff);
        let phases = (ratio != 1.0).then(|| Phases::new(ratio, cutoff, half)).flatten();
        if let Some(ref p) = phases {
            ratio = p.ratio;
        }
        Self {
            ratio,
            half,
            scratch: Vec::new(),
            position: half as f64,
            input_frames: 0,
            output_frames: 0,
            finished: false,
            phase: 0,
            centre_frame: half,
            phases,
            buffer: vec![0.0; half * CHANNELS],
        }
    }

    #[cfg(test)]
    fn without_phases(&mut self) {
        self.phases = None;
        self.phase = 0;
    }

    pub fn push(&mut self, input: &[f32]) {
        assert!(!self.finished, "cannot push after the resampler is finished");
        self.input_frames += (input.len() / CHANNELS) as u64;
        self.buffer.extend_from_slice(input);
    }

    /// Appends lookahead padding at EOF without lengthening the output duration.
    pub fn finish(&mut self) {
        if !self.finished {
            self.finished = true;
            self.buffer.resize(self.buffer.len() + (self.half + 1) * CHANNELS, 0.0);
        }
    }

    fn remaining(&self) -> usize {
        let exact = self.input_frames as f64 / self.ratio - 1e-9;
        let total = if exact.is_finite() && exact > 0.0 {
            exact.ceil().min(u64::MAX as f64) as u64
        } else {
            0
        };
        total.saturating_sub(self.output_frames).min(usize::MAX as u64) as usize
    }

    pub fn is_drained(&self) -> bool {
        self.finished && self.remaining() == 0
    }

    pub fn available(&self) -> usize {
        if self.finished {
            return self.remaining();
        }
        let avail =
            ((self.buffer.len() / CHANNELS) as f64 - self.half as f64 - self.position) / self.ratio;
        if avail.is_finite() && avail > 0.0 {
            avail.ceil().min(usize::MAX as f64) as usize
        } else {
            0
        }
    }

    pub fn produce(&mut self, limit: usize, out: &mut Vec<f32>) -> usize {
        let capped = if self.finished { limit.min(self.remaining()) } else { limit };
        if capped == 0 {
            return 0;
        }
        let produced = if self.ratio == 1.0 {
            self.produce_unity(capped, out)
        } else {
            self.produce_filtered(capped, out)
        };
        self.output_frames += produced as u64;
        if self.finished && produced < capped {
            self.output_frames = self.output_frames.saturating_add(self.remaining() as u64);
        }
        self.compact();
        produced
    }

    fn produce_unity(&mut self, limit: usize, out: &mut Vec<f32>) -> usize {
        let frames = self.buffer.len() / CHANNELS;
        if !self.position.is_finite() || self.position < 0.0 || self.position >= frames as f64 {
            return 0;
        }
        let centre = self.position as usize;
        let Some(avail) = frames.checked_sub(self.half + centre).filter(|&n| n > 0) else {
            return 0;
        };
        let count = limit.min(avail);
        let at = centre * CHANNELS;
        out.extend_from_slice(&self.buffer[at..at + count * CHANNELS]);
        self.position += count as f64;
        count
    }

    fn produce_filtered(&mut self, limit: usize, out: &mut Vec<f32>) -> usize {
        let buf_frames = self.buffer.as_chunks::<CHANNELS>().0;
        let frames = buf_frames.len();
        let width = 2 * self.half;
        let mut produced = 0;
        out.reserve(self.available().min(limit) * CHANNELS);
        if let Some(phases) = &self.phases {
            let (mut centre, mut phase) = (self.centre_frame, self.phase);
            let max_centre = frames.saturating_sub(self.half);
            if self.half == TAPS / 2 {
                let rows = phases.weights.as_chunks::<TAPS>().0;
                let steps = &phases.steps[..phases.period];
                while produced < limit && centre < max_centre && centre + 1 >= TAPS / 2 {
                    out.reserve(CHANNELS);
                    let base_len = out.len();
                    let spare = out.spare_capacity_mut().as_chunks_mut::<CHANNELS>().0;
                    let batch = spare.len().min(limit - produced);
                    let mut n = 0;
                    for slot in &mut spare[..batch] {
                        if centre >= max_centre || centre + 1 < TAPS / 2 {
                            break;
                        }
                        let Some(window) = buf_frames
                            .get(centre + 1 - TAPS / 2..)
                            .and_then(|s| s.first_chunk::<TAPS>())
                        else {
                            break;
                        };
                        let [l, r] = convolve_taps(window, &rows[phase]);
                        slot[0].write(l);
                        slot[1].write(r);
                        centre += steps[phase];
                        phase += 1;
                        if phase == phases.period {
                            phase = 0;
                        }
                        n += 1;
                    }
                    // SAFETY: `0..n` stereo pairs (`n * CHANNELS` `f32`s) were initialized above.
                    unsafe { out.set_len(base_len + n * CHANNELS) };
                    produced += n;
                    if n < batch {
                        break;
                    }
                }
            } else {
                while produced < limit && centre < max_centre && centre + 1 >= self.half {
                    let start = centre + 1 - self.half;
                    let base = phase * width;
                    out.extend_from_slice(&convolve(
                        &buf_frames[start..centre + self.half + 1],
                        &phases.weights[base..base + width],
                    ));
                    centre += phases.steps[phase];
                    phase += 1;
                    if phase == phases.period {
                        phase = 0;
                    }
                    produced += 1;
                }
            }
            self.centre_frame = centre;
            self.phase = phase;
            self.position = centre as f64 + phases.fractions[phase];
            return produced;
        }

        let cutoff = if self.ratio > 1.0 { 1.0 / self.ratio } else { 1.0 };
        let mut weights = std::mem::take(&mut self.scratch);
        weights.resize(width, 0.0);
        let max_centre = frames.saturating_sub(self.half);
        while produced < limit
            && self.position.is_finite()
            && self.position >= 0.0
            && self.position < max_centre as f64
        {
            let centre = self.position as usize;
            if centre >= max_centre || centre + 1 < self.half {
                break;
            }
            let start = centre + 1 - self.half;
            let fraction = self.position - centre as f64;
            let mut sum = 0.0f64;
            for (tap_index, slot) in weights.iter_mut().enumerate() {
                let w = tap(fraction, tap_offset(tap_index, self.half), cutoff);
                sum += w;
                *slot = w as f32;
            }
            let inv = if sum.abs() > 1e-9 { (1.0 / sum) as f32 } else { 0.0 };
            for w in &mut weights {
                *w *= inv;
            }
            out.extend_from_slice(&convolve(&buf_frames[start..centre + self.half + 1], &weights));
            self.position += self.ratio;
            produced += 1;
        }
        self.scratch = weights;
        produced
    }

    fn compact(&mut self) {
        let total_frames = self.buffer.len() / CHANNELS;
        let current_floor = if self.phases.is_some() {
            self.centre_frame
        } else if self.position.is_finite() && self.position >= 0.0 {
            (self.position as usize).min(total_frames)
        } else {
            0
        };
        let keep_from = current_floor.saturating_sub(self.half + 1);
        if keep_from > 4096 {
            let keep_from = keep_from.min(total_frames);
            self.buffer.drain(..keep_from * CHANNELS);
            self.centre_frame = self.centre_frame.saturating_sub(keep_from);
            self.position = match &self.phases {
                Some(phases) => self.centre_frame as f64 + phases.fractions[self.phase],
                None => self.position - keep_from as f64,
            };
        }
    }
}

/// 4-way independent accumulator FIR dot product for SLP SIMD vectorization.
#[inline(always)]
fn convolve_taps(frames: &[[f32; CHANNELS]; TAPS], weights: &[f32; TAPS]) -> [f32; CHANNELS] {
    let mut acc = [[0.0f32; CHANNELS]; 4];
    for i in 0..TAPS / 4 {
        for k in 0..4 {
            let w = weights[i * 4 + k];
            let [l, r] = frames[i * 4 + k];
            acc[k][0] += l * w;
            acc[k][1] += r * w;
        }
    }
    [
        (acc[0][0] + acc[1][0]) + (acc[2][0] + acc[3][0]),
        (acc[0][1] + acc[1][1]) + (acc[2][1] + acc[3][1]),
    ]
}

#[inline(always)]
fn convolve(frames: &[[f32; CHANNELS]], weights: &[f32]) -> [f32; CHANNELS] {
    if let (Ok(frames), Ok(weights)) =
        (<&[[f32; CHANNELS]; TAPS]>::try_from(frames), <&[f32; TAPS]>::try_from(weights))
    {
        return convolve_taps(frames, weights);
    }
    let mut acc = [[0.0f32; CHANNELS]; 4];
    let (frame_chunks, frame_rem) = frames.as_chunks::<4>();
    let (weight_chunks, weight_rem) = weights.as_chunks::<4>();
    for (fc, wc) in frame_chunks.iter().zip(weight_chunks) {
        for k in 0..4 {
            acc[k][0] += fc[k][0] * wc[k];
            acc[k][1] += fc[k][1] * wc[k];
        }
    }
    let mut left = (acc[0][0] + acc[1][0]) + (acc[2][0] + acc[3][0]);
    let mut right = (acc[0][1] + acc[1][1]) + (acc[2][1] + acc[3][1]);
    for (&[l, r], &w) in frame_rem.iter().zip(weight_rem) {
        left += l * w;
        right += r * w;
    }
    [left, right]
}

#[derive(Debug, Clone)]
struct Phases {
    ratio: f64,
    period: usize,
    fractions: Vec<f64>,
    steps: Vec<usize>,
    weights: Vec<f32>,
}

impl Phases {
    fn new(ratio: f64, cutoff: f64, half: usize) -> Option<Self> {
        if !ratio.is_finite() || ratio <= 0.0 || half == 0 {
            return None;
        }
        let width = 2 * half;
        let max_phases = (MAX_TABLE_BYTES / (width * 8)).clamp(1, MAX_PHASES);
        let period = (1..=max_phases).find(|&period| {
            let turns = ratio * period as f64;
            if !turns.is_finite() || turns < 0.5 || turns > (u64::MAX / MAX_PHASES as u64) as f64 {
                return false;
            }
            let nearest = (turns + 0.5) as u64;
            nearest > 0 && (turns - nearest as f64).abs() < 1e-12
        })?;
        let turns = (ratio * period as f64 + 0.5) as u64;
        let period_u64 = period as u64;
        let snapped_ratio = turns as f64 / period_u64 as f64;
        let cutoff = if snapped_ratio > 1.0 { 1.0 / snapped_ratio } else { cutoff };
        let mut fractions = Vec::with_capacity(period);
        let mut steps = Vec::with_capacity(period);
        let mut weights = Vec::with_capacity(period * width);
        let mut row_f64 = vec![0.0f64; width];
        for phase in 0..period {
            let num = (phase as u64) * turns;
            let (q_curr, rem) = (num / period_u64, num % period_u64);
            let q_next = ((phase as u64 + 1) * turns) / period_u64;
            steps.push((q_next - q_curr) as usize);
            let fraction = rem as f64 / period_u64 as f64;
            fractions.push(fraction);
            let mut sum = 0.0f64;
            for (tap_index, slot) in row_f64.iter_mut().enumerate() {
                let w = tap(fraction, tap_offset(tap_index, half), cutoff);
                *slot = w;
                sum += w;
            }
            let inv = if sum.abs() > 1e-9 { 1.0 / sum } else { 0.0 };
            weights.extend(row_f64.iter().map(|&w| (w * inv) as f32));
        }
        Some(Self { ratio: snapped_ratio, period, fractions, steps, weights })
    }
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-9 {
        1.0
    } else {
        let p = PI * x;
        p.sin() / p
    }
}

/// Minimum 4-term Blackman-Harris window (~92 dB sidelobe rejection).
fn window(x: f64) -> f64 {
    if x.abs() >= 1.0 {
        return 0.0;
    }
    let p = PI * (x + 1.0);
    0.35875 - 0.48829 * p.cos() + 0.14128 * (2.0 * p).cos() - 0.01168 * (3.0 * p).cos()
}

fn tap_offset(tap_index: usize, half: usize) -> i64 {
    tap_index as i64 + 1 - half as i64
}

fn half_window(cutoff: f64) -> usize {
    (((TAPS / 2) as f64 / cutoff).ceil() as usize).clamp(TAPS / 2, MAX_HALF)
}

fn tap(fraction: f64, offset: i64, cutoff: f64) -> f64 {
    let distance = (fraction - offset as f64) * cutoff;
    sinc(distance) * window(distance / (TAPS as f64 / 2.0))
}

/// Waveform-similarity overlap-add (WSOLA) pitch-preserving tempo stretcher.
#[derive(Debug, Clone)]
pub struct TimeStretcher {
    input: Vec<f32>,
    skip: usize,
    output: Vec<f32>,
    tail: [f32; HOP],
    sums: Vec<f32>,
    fades: [f32; HOP],
    primed: bool,
    analysis_frac: f64,
    total_input_frames: u64,
    total_output_frames: u64,
}

/// WSOLA frame length in samples (~21.3 ms @ 48 kHz).
pub const STRETCH_FRAME: usize = 1024;
const HOP: usize = STRETCH_FRAME / 2;
const SEARCH: usize = STRETCH_FRAME / 4;
const TAIL: usize = HOP * CHANNELS;

impl Default for TimeStretcher {
    fn default() -> Self {
        Self::new()
    }
}

#[inline(always)]
fn dot_hop(window: &[f32; HOP], tail: &[f32; HOP]) -> f32 {
    let mut acc = [0.0f32; 8];
    for (w, t) in window.as_chunks::<8>().0.iter().zip(tail.as_chunks::<8>().0) {
        for i in 0..8 {
            acc[i] += w[i] * t[i];
        }
    }
    ((acc[0] + acc[1]) + (acc[2] + acc[3])) + ((acc[4] + acc[5]) + (acc[6] + acc[7]))
}

#[inline(always)]
fn energy_hop(window: &[f32; HOP]) -> f32 {
    dot_hop(window, window)
}

#[inline(always)]
fn mono_sum([l, r]: [f32; CHANNELS]) -> f32 {
    let s = l + r;
    if s.is_finite() {
        s
    } else {
        0.0
    }
}

#[inline]
fn clean_stretch_factor(factor: f64) -> f64 {
    if factor.is_finite() {
        factor.clamp(0.05, 5.0)
    } else {
        1.0
    }
}

impl TimeStretcher {
    pub fn new() -> Self {
        let fades = std::array::from_fn(|offset| {
            (0.5 - 0.5 * (PI * (offset as f64 + 0.5) / HOP as f64).cos()) as f32
        });
        Self {
            input: Vec::new(),
            skip: 0,
            output: Vec::new(),
            tail: [0.0; HOP],
            sums: vec![0.0; SEARCH * 2 + HOP],
            fades,
            primed: false,
            analysis_frac: 0.0,
            total_input_frames: 0,
            total_output_frames: 0,
        }
    }

    pub fn push(&mut self, input: &[f32]) {
        let frames = input.len() / CHANNELS;
        self.total_input_frames += frames as u64;
        let skipped = self.skip.min(frames);
        self.skip -= skipped;
        self.input.extend_from_slice(&input[skipped * CHANNELS..]);
    }

    pub fn produce(&mut self, factor: f64) {
        let factor = clean_stretch_factor(factor);
        let hop_step = HOP as f64 * factor;
        let frames = self.input.len() / CHANNELS;
        let mut read = 0usize;
        while read + STRETCH_FRAME + SEARCH * 2 <= frames {
            let offset = if self.primed { self.best_offset(read) } else { 0 };
            let start = (read + offset) * CHANNELS;
            let segment = &self.input[start..start + STRETCH_FRAME * CHANNELS];
            if self.primed {
                let base = self.output.len() - TAIL;
                for ((out_frame, seg_frame), &fade) in self.output[base..]
                    .as_chunks_mut::<CHANNELS>()
                    .0
                    .iter_mut()
                    .zip(segment[..TAIL].as_chunks::<CHANNELS>().0)
                    .zip(&self.fades)
                {
                    out_frame[0] += (seg_frame[0] - out_frame[0]) * fade;
                    out_frame[1] += (seg_frame[1] - out_frame[1]) * fade;
                }
                self.output.extend_from_slice(&segment[TAIL..]);
            } else {
                self.output.extend_from_slice(segment);
                self.primed = true;
            }
            for (dst, &frame) in self
                .tail
                .iter_mut()
                .zip(segment[(STRETCH_FRAME - HOP) * CHANNELS..].as_chunks::<CHANNELS>().0)
            {
                *dst = mono_sum(frame);
            }
            let exact = hop_step + self.analysis_frac;
            let step = (exact as usize).max(1);
            self.analysis_frac = (exact - step as f64).max(0.0);
            read += step;
        }
        if read > 0 {
            self.input.drain(..read.min(frames) * CHANNELS);
            self.skip += read.saturating_sub(frames);
        }
    }

    pub fn take_output(&mut self) -> Vec<f32> {
        let mut out = Vec::new();
        self.take_output_into(&mut out);
        out
    }

    pub fn take_output_into(&mut self, out: &mut Vec<f32>) {
        let ready = self.output.len() - TAIL.min(self.output.len());
        self.total_output_frames += (ready / CHANNELS) as u64;
        out.extend_from_slice(&self.output[..ready]);
        self.output.drain(..ready);
    }

    pub fn finish(&mut self, factor: f64, out: &mut Vec<f32>) {
        if self.total_input_frames == 0 {
            return;
        }
        let factor = clean_stretch_factor(factor);
        let target_total = ((self.total_input_frames as f64 / factor + 0.5) as u64).max(1);
        let remaining_target = target_total.saturating_sub(self.total_output_frames) as usize;
        if !self.input.is_empty() || !self.primed {
            self.skip = 0;
            self.input.resize(self.input.len() + (STRETCH_FRAME + SEARCH * 2) * CHANNELS, 0.0);
            self.produce(factor);
        }
        let take_tail = remaining_target.min(self.output.len() / CHANNELS) * CHANNELS;
        if take_tail > 0 {
            out.extend_from_slice(&self.output[..take_tail]);
            self.total_output_frames += (take_tail / CHANNELS) as u64;
        }
        self.output.clear();
        self.input.clear();
        self.skip = 0;
        self.analysis_frac = 0.0;
    }

    fn best_offset(&mut self, read: usize) -> usize {
        let mut best = SEARCH;
        let mut best_score = f32::MIN;
        let input_frames = self.input.as_chunks::<CHANNELS>().0;
        let last = input_frames.len().saturating_sub(read).min(self.sums.len());
        if last < HOP {
            return best;
        }
        for (sum, &frame) in self.sums[..last].iter_mut().zip(&input_frames[read..read + last]) {
            *sum = mono_sum(frame);
        }
        let max_offset = (SEARCH * 2).min(last - HOP);
        let mut energy = 0.0f32;
        for offset in 0..=max_offset {
            let window: &[f32; HOP] = self.sums[offset..offset + HOP].try_into().unwrap();
            if offset & 63 == 0 {
                energy = energy_hop(window);
            } else {
                let (prev, next) = (self.sums[offset - 1], self.sums[offset + HOP - 1]);
                energy = (energy + next * next - prev * prev).max(0.0);
                if !energy.is_finite() {
                    energy = energy_hop(window);
                }
            }
            let dot = dot_hop(window, &self.tail);
            if dot <= 0.0 && best_score >= 0.0 {
                continue;
            }
            let score = dot / (energy.sqrt() + 1e-6);
            if score > best_score {
                best_score = score;
                best = offset;
            }
        }
        best
    }
}

/// Source-to-48 kHz input/output ratio with pitch and rate folded in.
pub fn ratio_for(input_rate: u32, pitch: f64, rate: f64) -> f64 {
    (input_rate as f64 / crate::dsp::SAMPLE_RATE as f64) * pitch * rate
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    fn tone_gain(input_rate: f64, ratio: f64, hertz: f64) -> f64 {
        let frames = 48_000;
        let input: Vec<f32> = (0..frames)
            .flat_map(|n| {
                let v = 0.5 * (TAU * hertz * n as f64 / input_rate).sin() as f32;
                [v; CHANNELS]
            })
            .collect();
        let mut resampler = SincResampler::new(ratio);
        resampler.push(&input);
        resampler.finish();
        let mut out = Vec::new();
        while !resampler.is_drained() && resampler.produce(4096, &mut out) > 0 {}
        let rms = |samples: &[f32]| {
            (samples.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / samples.len() as f64)
                .sqrt()
        };
        let margin = 2000 * CHANNELS;
        rms(&out[margin..out.len() - margin]) / rms(&input[margin..input.len() - margin])
    }

    fn tone(rate: f64, samples: usize) -> Vec<f32> {
        (0..samples)
            .flat_map(|i| [(TAU * rate * i as f64 / 48_000.0).sin() as f32; CHANNELS])
            .collect()
    }

    fn upward_crossings(frames: &[[f32; CHANNELS]]) -> usize {
        frames.windows(2).filter(|pair| pair[0][0] < 0.0 && pair[1][0] >= 0.0).count()
    }

    #[test]
    fn lowering_the_rate_removes_what_lies_above_the_new_nyquist_frequency() {
        assert!(tone_gain(192_000.0, 4.0, 40_000.0) < 0.001);
        assert!((0.94..1.06).contains(&tone_gain(192_000.0, 4.0, 1_000.0)));
        assert!(tone_gain(96_000.0, 2.0, 44_000.0) < 0.001);
        assert!((0.94..1.06).contains(&tone_gain(96_000.0, 2.0, 10_000.0)));
    }

    #[test]
    fn the_window_grows_with_the_ratio_and_stays_bounded() {
        assert_eq!(half_window(1.0), TAPS / 2);
        assert_eq!(half_window(0.5), TAPS);
        assert_eq!(half_window(0.25), TAPS * 2);
        assert_eq!(half_window(1e-6), MAX_HALF);
    }

    #[test]
    fn a_unity_ratio_resampler_reproduces_its_input() {
        let mut resampler = SincResampler::new(1.0);
        resampler.push(&tone(440.0, 4096));
        let mut out = Vec::new();
        assert_eq!(resampler.produce(2048, &mut out), 2048);
        for i in 0..1024 {
            let expected = (TAU * 440.0 * i as f64 / 48_000.0).sin() as f32;
            assert!((out[i * CHANNELS] - expected).abs() < 1e-3);
        }
    }

    #[test]
    fn the_unity_shortcut_matches_the_filter_it_replaces() {
        let input = tone(440.0, 4096);
        let mut shortcut = SincResampler::new(1.0);
        shortcut.push(&input);
        let mut fast = Vec::new();
        shortcut.produce(2048, &mut fast);

        let mut filtered = SincResampler::new(1.0);
        filtered.push(&input);
        let mut slow = Vec::new();
        filtered.produce_filtered(2048, &mut slow);

        assert_eq!(fast.len(), slow.len());
        for (a, b) in fast.iter().zip(&slow) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn the_weight_table_matches_the_weights_worked_out_per_frame() {
        let ratio = 44_100.0 / 48_000.0;
        assert_eq!(Phases::new(ratio, 1.0, TAPS / 2).unwrap().period, 160);

        let input = tone(1000.0, 8192);
        let mut tabled = SincResampler::new(ratio);
        tabled.push(&input);
        let mut with_table = Vec::new();
        tabled.produce(4096, &mut with_table);

        let mut computed = SincResampler::new(ratio);
        computed.without_phases();
        computed.push(&input);
        let mut without_table = Vec::new();
        computed.produce(4096, &mut without_table);

        assert_eq!(with_table.len(), without_table.len());
        for (a, b) in with_table.iter().zip(&without_table) {
            assert!((a - b).abs() < 1e-5);
        }
    }

    #[test]
    fn a_ratio_with_no_short_period_has_no_weight_table() {
        assert!(Phases::new(0.9173, 1.0, TAPS / 2).is_none());
    }

    #[test]
    fn halving_the_rate_halves_the_number_of_frames() {
        let mut resampler = SincResampler::new(2.0);
        resampler.push(&tone(200.0, 8192));
        let mut out = Vec::new();
        let produced = resampler.produce(8192, &mut out);
        let expected = 4096 - (TAPS / 2) as i64;
        assert!((produced as i64 - expected).abs() <= 2, "produced {produced}");
    }

    #[test]
    fn upsampling_cd_audio_preserves_high_passband_and_deep_stopband() {
        let air = tone_gain(44_100.0, 44_100.0 / 48_000.0, 18_000.0);
        assert!((0.97..1.02).contains(&air), "18 kHz passband gain was {air:.5}");
        let deep_stop = tone_gain(192_000.0, 4.0, 48_000.0);
        assert!(deep_stop < 0.000_05, "48 kHz stopband gain was {deep_stop:.7}");
    }

    #[test]
    fn finishing_keeps_short_sources_and_exact_output_duration() {
        for rate in [8_000, 22_050, 44_100, 48_000, 96_000] {
            for input_frames in [1, 4, 17, rate as usize] {
                let mut resampler = SincResampler::new(rate as f64 / 48_000.0);
                resampler.push(&vec![0.5; input_frames * CHANNELS]);
                resampler.finish();
                resampler.finish();
                let mut out = Vec::new();
                while resampler.produce(131, &mut out) > 0 {}
                let expected = (input_frames as u64 * 48_000).div_ceil(rate as u64) as usize;
                assert_eq!(out.len(), expected * CHANNELS, "rate={rate}, frames={input_frames}");
                assert!(resampler.is_drained());
                assert!(out.iter().any(|value| value.abs() > 0.01));
            }
        }
    }

    #[test]
    fn long_chunked_resampling_keeps_a_bounded_history_and_the_same_samples() {
        let input = tone(440.0, 240_000);
        let mut whole = SincResampler::new(44_100.0 / 48_000.0);
        whole.push(&input);
        whole.finish();
        let mut expected = Vec::new();
        while whole.produce(1024, &mut expected) > 0 {}
        let mut chunked = SincResampler::new(44_100.0 / 48_000.0);
        let mut actual = Vec::new();
        for chunk in input.chunks(512 * CHANNELS) {
            chunked.push(chunk);
            while chunked.produce(1024, &mut actual) > 0 {}
            assert!(chunked.buffer.len() < 10_000, "history grew to {}", chunked.buffer.len());
        }
        chunked.finish();
        while chunked.produce(1024, &mut actual) > 0 {}
        assert_eq!(actual.len(), expected.len());
        for (a, b) in actual.iter().zip(expected) {
            assert!((a - b).abs() < 1e-4);
        }
    }

    #[test]
    fn resampling_a_tone_keeps_its_frequency_when_the_ratio_is_one() {
        let mut resampler = SincResampler::new(1.0);
        resampler.push(&tone(1000.0, 8192));
        let mut out = Vec::new();
        resampler.produce(4096, &mut out);
        let crossings = upward_crossings(&out.as_chunks::<CHANNELS>().0[200..400]);
        assert!((crossings as i64 - 4).abs() <= 1, "{crossings} crossings in 200 frames");
    }

    #[test]
    fn the_ratio_folds_the_pitch_and_rate_in() {
        assert!((ratio_for(48_000, 1.0, 1.0) - 1.0).abs() < 1e-12);
        assert!((ratio_for(44_100, 1.0, 1.0) - 0.918_75).abs() < 1e-9);
        assert!((ratio_for(48_000, 2.0, 1.0) - 2.0).abs() < 1e-12);
    }

    #[test]
    fn raising_pitch_really_raises_the_frequency() {
        let mut resampler = SincResampler::new(ratio_for(48_000, 2.0, 1.0));
        resampler.push(&tone(440.0, 48_000));
        let mut out = Vec::new();
        resampler.produce(48_000, &mut out);
        let frames = out.as_chunks::<CHANNELS>().0;
        let window = &frames[1000..frames.len() - 1000];
        let hertz = upward_crossings(window) as f64 * 48_000.0 / window.len() as f64;
        assert!((hertz - 880.0).abs() < 5.0, "expected 880 Hz, got {hertz}");
    }

    #[test]
    fn fast_stretching_is_independent_of_input_chunk_boundaries() {
        let samples = tone(440.0, 48_000);
        let mut whole = TimeStretcher::new();
        whole.push(&samples);
        whole.produce(5.0);
        let expected = whole.take_output();
        let mut chunked = TimeStretcher::new();
        let mut actual = Vec::new();
        for chunk in samples.chunks(600 * CHANNELS) {
            chunked.push(chunk);
            chunked.produce(5.0);
            chunked.take_output_into(&mut actual);
        }
        assert_eq!(actual, expected);
        assert!((8_000..=10_000).contains(&(actual.len() / CHANNELS)));
    }

    #[test]
    fn a_stretcher_at_one_leaves_the_length_alone() {
        let mut stretcher = TimeStretcher::new();
        stretcher.push(&tone(440.0, 8192));
        stretcher.produce(1.0);
        let frames = stretcher.take_output().len() / CHANNELS;
        assert!((frames as i64 - 8192).abs() < 2048, "{frames} frames out of 8192");
    }

    #[test]
    fn a_faster_factor_shortens_the_output() {
        let input = tone(440.0, 48_000);
        let run = |factor| {
            let mut s = TimeStretcher::new();
            s.push(&input);
            s.produce(factor);
            s.take_output().len()
        };
        let (slow_len, fast_len) = (run(1.0), run(2.0));
        assert!(fast_len < slow_len * 3 / 4 && fast_len > slow_len * 2 / 5);
    }

    #[test]
    fn a_stretched_tone_stays_a_tone() {
        let mut stretcher = TimeStretcher::new();
        stretcher.push(&tone(440.0, 48_000));
        stretcher.produce(1.5);
        let out = stretcher.take_output();
        let crossings = upward_crossings(&out.as_chunks::<CHANNELS>().0[8000..16_000]);
        assert!((crossings as i64 - 73).abs() < 6, "{crossings} cycles, expected about 73");
    }

    #[test]
    fn finish_flushes_short_clips_and_high_speed_tails() {
        let mut stretcher = TimeStretcher::new();
        let mut out = Vec::new();
        stretcher.push(&tone(440.0, 600));
        stretcher.produce(2.0);
        stretcher.take_output_into(&mut out);
        assert!(out.is_empty());
        stretcher.finish(2.0, &mut out);
        assert_eq!(out.len(), 300 * CHANNELS);

        let input = tone(440.0, 48_000);
        let mut fast = TimeStretcher::new();
        let mut fast_out = Vec::new();
        for chunk in input.chunks(700 * CHANNELS) {
            fast.push(chunk);
            fast.produce(5.0);
            fast.take_output_into(&mut fast_out);
        }
        fast.finish(5.0, &mut fast_out);
        let frames = fast_out.len() / CHANNELS;
        assert!((frames as i64 - 9_600).abs() <= HOP as i64, "frames={frames}");
    }

    #[test]
    fn non_finite_and_extreme_ratios_and_stretch_factors_stay_safe() {
        let clip = tone(440.0, 2048);
        for bad_ratio in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -3.0, 1e-30, 1e30] {
            let mut resampler = SincResampler::new(bad_ratio);
            resampler.push(&clip);
            let mut out = Vec::new();
            let _ = resampler.produce(1024, &mut out);
            resampler.finish();
            let _ = resampler.produce(1024, &mut out);
            assert!(out.iter().all(|s| s.is_finite()));
        }
        for bad_factor in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -2.0, 100.0] {
            let mut stretcher = TimeStretcher::new();
            stretcher.push(&clip);
            stretcher.produce(bad_factor);
            let mut out = stretcher.take_output();
            stretcher.finish(bad_factor, &mut out);
            assert!(!out.is_empty() && out.iter().all(|s| s.is_finite()));
        }
    }

    #[test]
    fn phases_table_uses_tight_snap_tolerance_u64_products_and_aligns_ratio() {
        assert!(Phases::new(1.0 + 1e-10, 1.0, TAPS / 2).is_none());
        let steep = Phases::new(10_000.0, 1e-4, MAX_HALF).expect("exact integer ratio tabulates");
        assert_eq!((steep.period, steep.ratio), (1, 10_000.0));

        let snapped_half = SincResampler::new(0.5 + 1e-13);
        assert_eq!(snapped_half.ratio, 0.5);
        assert!(snapped_half.phases.is_some());

        let snapped_unity = SincResampler::new(1.0 + 1e-13);
        assert_eq!(snapped_unity.ratio, 1.0);
        assert!(snapped_unity.phases.is_none());
    }
}
