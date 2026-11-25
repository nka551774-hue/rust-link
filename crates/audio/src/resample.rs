//! Rate conversion and time stretching.
//!
//! Two jobs live here because they are the same job seen twice. Converting a
//! source that is not 48 kHz is resampling at a fixed ratio. The `timescale`
//! filter is resampling at a ratio the client chose, plus a stretch that puts
//! the tempo back where the pitch change would otherwise have moved it.
//!
//! The resampler is a windowed sinc interpolator. It is not the cheapest way to
//! change a rate, but it is the one that stays clean when the ratio is not a
//! ratio of small integers, which is the case for every source that is not
//! already at 48 kHz.

use crate::dsp::CHANNELS;

/// Taps of the window when the rate is not being lowered. Sixteen is enough that
/// the error sits below the noise floor of the Opus encoder that follows.
const TAPS: usize = 16;

/// The widest half-window a resampler may use. Lowering the rate narrows the
/// filter's cutoff, and a narrower cutoff needs proportionally more taps to keep
/// its stopband; this bounds that at a ratio of 32 to 1.
const MAX_HALF: usize = 256;

/// The bytes a weight table may take, which bounds how many phases it holds.
const MAX_TABLE_BYTES: usize = 128 * 1024;

/// The longest period a weight table may repeat over. A table is 128 bytes per
/// phase, so this bounds one resampler at 128 KiB.
const MAX_PHASES: usize = 1024;

/// Converts an interleaved stereo stream from one rate to another.
#[derive(Debug, Clone)]
pub struct SincResampler {
    /// Input frames per output frame.
    ratio: f64,
    /// Where the next output frame is read from, in input frames.
    position: f64,
    input_frames: u64,
    output_frames: u64,
    finished: bool,
    /// Half the window, in input frames. It is `TAPS / 2` until the rate is
    /// lowered, and then grows with the ratio so that the filter keeps the same
    /// number of sinc lobes after its cutoff has been narrowed.
    half: usize,
    /// The weights of the frame being produced, kept between frames so that
    /// producing one does not allocate.
    scratch: Vec<f32>,
    /// Which row of the weight table the next output frame reads.
    phase: usize,
    /// Exact integer read frame used alongside `phases`, eliminating cumulative
    /// floating-point phase drift across multi-hour streams.
    centre_frame: usize,
    /// Pre-normalised weights of every phase, when the ratio repeats them.
    phases: Option<Phases>,
    buffer: Vec<f32>,
}

impl SincResampler {
    /// `ratio` is input frames per output frame: 48000 in and 48000 out is 1.0,
    /// 44100 in and 48000 out is 0.91875.
    pub fn new(ratio: f64) -> Self {
        let mut ratio = if ratio.is_finite() { ratio.clamp(1e-4, 1e4) } else { 1.0 };
        if (ratio - 1.0).abs() < 1e-12 {
            ratio = 1.0;
        }
        let cutoff = if ratio > 1.0 { 1.0 / ratio } else { 1.0 };
        let half = half_window(cutoff);
        // The unity ratio has its own path that needs no weights at all.
        let phases = if ratio == 1.0 { None } else { Phases::new(ratio, cutoff, half) };
        if let Some(ref p) = phases {
            ratio = p.ratio;
        }
        SincResampler {
            ratio,
            half,
            scratch: Vec::new(),
            // Zero history puts the first source sample at the first output
            // position instead of dropping half a filter window from the song.
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

    /// Drops the weight table, so a test can compare the table against the
    /// weights worked out frame by frame.
    #[cfg(test)]
    fn without_phases(&mut self) {
        self.phases = None;
        self.phase = 0;
    }

    /// Appends interleaved input.
    pub fn push(&mut self, input: &[f32]) {
        assert!(!self.finished, "cannot push after the resampler is finished");
        self.input_frames += (input.len() / CHANNELS) as u64;
        self.buffer.extend_from_slice(input);
    }

    /// Supplies the final lookahead without including the padding in the
    /// output duration. Calling this twice does not append more silence.
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

    /// How many whole output frames can be produced from what has been pushed.
    pub fn available(&self) -> usize {
        if self.finished {
            return self.remaining();
        }
        let frames = self.buffer.len() / CHANNELS;
        // floor(position) + half_window + 1 must fit in the input buffer.
        let avail = (frames as f64 - self.half as f64 - self.position) / self.ratio;
        if avail.is_finite() && avail > 0.0 {
            avail.ceil().min(usize::MAX as f64) as usize
        } else {
            0
        }
    }

    /// Produces up to `limit` output frames, appending them to `out`.
    ///
    /// Returns how many frames were produced, which is zero when the caller has
    /// not pushed enough input yet.
    pub fn produce(&mut self, limit: usize, out: &mut Vec<f32>) -> usize {
        if limit == 0 {
            return 0;
        }
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

    /// The unity-ratio case: every tap but the centre falls on a whole number
    /// of input frames, where the windowed sinc is zero, so the filter passes
    /// the input through untouched. Most sources are already 48 kHz, and this
    /// is what keeps them from paying for a filter that does not move them.
    fn produce_unity(&mut self, limit: usize, out: &mut Vec<f32>) -> usize {
        let frames = self.buffer.len() / CHANNELS;
        if !self.position.is_finite() || self.position < 0.0 || self.position >= frames as f64 {
            return 0;
        }
        // The same window rule the filtered path uses, so both report the same
        // number of frames for the same input: the last frame that can be
        // produced has half a window of frames ahead of it.
        let centre = self.position as usize;
        if centre >= frames.saturating_sub(self.half) {
            return 0;
        }
        let count = limit.min(frames - self.half - centre);
        if count == 0 {
            return 0;
        }
        // A whole run of frames is one contiguous copy, which is what makes a
        // 48 kHz source a memcpy instead of a frame at a time.
        let at = centre * CHANNELS;
        out.extend_from_slice(&self.buffer[at..at + count * CHANNELS]);
        self.position += count as f64;
        count
    }

    /// The general case: a windowed sinc interpolation at the read position.
    fn produce_filtered(&mut self, limit: usize, out: &mut Vec<f32>) -> usize {
        let buf_frames = self.buffer.as_chunks::<CHANNELS>().0;
        let frames = buf_frames.len();
        let width = 2 * self.half;
        let mut produced = 0;
        out.reserve(self.available().min(limit) * CHANNELS);
        if let Some(phases) = &self.phases {
            let mut centre = self.centre_frame;
            let mut phase = self.phase;
            let max_centre = frames.saturating_sub(self.half);
            if self.half == TAPS / 2 {
                let rows = phases.weights.as_chunks::<TAPS>().0;
                let steps = &phases.steps[..phases.period];
                let base_len = out.len();
                let spare = out.spare_capacity_mut().as_chunks_mut::<CHANNELS>().0;
                let batch = spare.len().min(limit);
                for slot in &mut spare[..batch] {
                    if centre >= max_centre || centre + 1 < TAPS / 2 {
                        break;
                    }
                    let start = centre + 1 - (TAPS / 2);
                    let Some(window) =
                        buf_frames.get(start..).and_then(|s| s.first_chunk::<TAPS>())
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
                    produced += 1;
                }
                // SAFETY: `0..produced` stereo pairs (`produced * CHANNELS` `f32`s) were initialized above.
                unsafe {
                    out.set_len(base_len + produced * CHANNELS);
                }
                while produced < limit {
                    if centre >= max_centre || centre + 1 < TAPS / 2 {
                        break;
                    }
                    let start = centre + 1 - (TAPS / 2);
                    let Some(window) =
                        buf_frames.get(start..).and_then(|s| s.first_chunk::<TAPS>())
                    else {
                        break;
                    };
                    out.extend_from_slice(&convolve_taps(window, &rows[phase]));
                    centre += steps[phase];
                    phase += 1;
                    if phase == phases.period {
                        phase = 0;
                    }
                    produced += 1;
                }
            } else {
                while produced < limit {
                    if centre >= max_centre || centre + 1 < self.half {
                        break;
                    }
                    let start = centre + 1 - self.half;
                    let end = centre + self.half + 1;
                    let base = phase * width;
                    let row = &phases.weights[base..base + width];
                    let sample = convolve(&buf_frames[start..end], row);
                    out.extend_from_slice(&sample);
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

        // Downsampling needs the filter narrowed to the new Nyquist, or the
        // content above it folds back as aliasing.
        let cutoff = if self.ratio > 1.0 { 1.0 / self.ratio } else { 1.0 };
        let mut weights = std::mem::take(&mut self.scratch);
        weights.resize(width, 0.0);
        let max_centre = frames.saturating_sub(self.half);
        while produced < limit {
            if !self.position.is_finite()
                || self.position < 0.0
                || self.position >= max_centre as f64
            {
                break;
            }
            let centre = self.position as usize;
            if centre >= max_centre || centre + 1 < self.half {
                break;
            }
            let start = centre + 1 - self.half;
            let end = centre + self.half + 1;
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
            let sample = convolve(&buf_frames[start..end], &weights);
            out.extend_from_slice(&sample);
            self.position += self.ratio;
            produced += 1;
        }
        self.scratch = weights;
        produced
    }

    /// Drops the input behind the read position, which will never be read
    /// again because the filter only looks half a window back.
    fn compact(&mut self) {
        let total_frames = self.buffer.len() / CHANNELS;
        let current_floor = if self.phases.is_some() {
            self.centre_frame
        } else if self.position.is_finite() && self.position >= 0.0 {
            if self.position >= total_frames as f64 {
                total_frames
            } else {
                self.position as usize
            }
        } else {
            0
        };
        // Everything before the next read position's window is never read again.
        let keep_from = current_floor.saturating_sub(self.half + 1);
        // position is relative to the current buffer. Mixing an absolute
        // consumed count into it retained progressively more audio on long runs.
        if keep_from > 4096 {
            let keep_from = keep_from.min(total_frames);
            let drop = keep_from * CHANNELS;
            self.buffer.drain(..drop);
            self.centre_frame = self.centre_frame.saturating_sub(keep_from);
            if let Some(phases) = &self.phases {
                self.position = self.centre_frame as f64 + phases.fractions[self.phase];
            } else {
                self.position -= keep_from as f64;
            }
        }
    }
}

/// Four-way independent accumulator FIR dot product so LLVM SLP-vectorizes and
/// unrolls the 16-tap stereo window without needing unsafe or `-ffast-math`.
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

/// The filter weights of every fractional read position a ratio repeats.
#[derive(Debug, Clone)]
struct Phases {
    /// Exact rational ratio `turns / period` this table advances by.
    ratio: f64,
    /// How many output frames the fractional position takes to come round.
    period: usize,
    /// Exact fractional read position `[0, 1)` of each row.
    fractions: Vec<f64>,
    /// Exact integer input-frame step to take after producing each phase.
    steps: Vec<usize>,
    /// `period` sets of `2 * half` pre-normalised weights.
    weights: Vec<f32>,
}

impl Phases {
    /// Builds the table for `ratio`, or nothing when the fractional positions
    /// do not come round within a period worth tabulating.
    fn new(ratio: f64, cutoff: f64, half: usize) -> Option<Phases> {
        if !ratio.is_finite() || ratio <= 0.0 || half == 0 {
            return None;
        }
        let width = 2 * half;
        // A wider window means a bigger row, so fewer rows fit in the budget.
        let max_phases = (MAX_TABLE_BYTES / (width * 8)).clamp(1, MAX_PHASES);
        let period = (1..=max_phases).find(|period| {
            let turns = ratio * *period as f64;
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
            let q_curr = num / period_u64;
            let rem = num % period_u64;
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
        Some(Phases { ratio: snapped_ratio, period, fractions, steps, weights })
    }
}

/// The normalised sinc: one at zero, zero at every other integer.
fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-9 {
        1.0
    } else {
        let p = std::f64::consts::PI * x;
        p.sin() / p
    }
}

/// A Blackman window, which trades a slightly wider transition for stopband
/// attenuation well past what audio needs.
fn window(x: f64) -> f64 {
    if x.abs() >= 1.0 {
        return 0.0;
    }
    let p = std::f64::consts::PI * (x + 1.0);
    0.42 - 0.5 * p.cos() + 0.08 * (2.0 * p).cos()
}

/// Where the tap at `tap_index` sits relative to the read position, in input
/// frames. The taps run from half a window behind the read position to half a
/// window ahead of it.
fn tap_offset(tap_index: usize, half: usize) -> i64 {
    tap_index as i64 + 1 - half as i64
}

/// Half the window for a filter whose cutoff is `cutoff` of the input's Nyquist
/// frequency. The window is `TAPS / 2` input frames while the rate is not being
/// lowered, and `1 / cutoff` times that when it is: narrowing the cutoff
/// stretches every sinc lobe by the same factor, so a fixed number of taps would
/// hold fewer lobes and let the content above the new Nyquist frequency through.
fn half_window(cutoff: f64) -> usize {
    (((TAPS / 2) as f64 / cutoff).ceil() as usize).clamp(TAPS / 2, MAX_HALF)
}

/// One tap of the windowed sinc: the weight of the input frame `offset` frames
/// from the read position, which sits `fraction` of the way between two frames.
fn tap(fraction: f64, offset: i64, cutoff: f64) -> f64 {
    let distance = (fraction - offset as f64) * cutoff;
    sinc(distance) * window(distance / (TAPS as f64 / 2.0))
}

/// Changes tempo without changing pitch.
///
/// This is a waveform-similarity overlap-add stretcher: frames of the input are
/// placed at a synthesis hop, and each one is nudged within a small search
/// window to the offset where it lines up best with what has already been
/// written. Aligning the waveforms is what keeps the result from sounding like
/// two copies of the track played at once, which is what a plain overlap-add
/// does at any useful ratio.
#[derive(Debug, Clone)]
pub struct TimeStretcher {
    input: Vec<f32>,
    /// At high tempos the analysis hop can pass the buffered input. Keep the
    /// remainder as a skip for the next push instead of draining past the end.
    skip: usize,
    /// Samples that are finished, less the tail the next frame still has to
    /// blend with. The stretcher owns them so that a caller cannot drain the
    /// tail out from under it.
    output: Vec<f32>,
    /// Mono-summed tail of the last frame written, which the next frame aligns to.
    tail: [f32; HOP],
    /// The summed channels of the frames the alignment search reads, kept
    /// between frames so that the search does not allocate one of its own.
    sums: Vec<f32>,
    /// The cross-fade of each frame of the overlap, worked out once.
    fades: [f32; HOP],
    primed: bool,
    analysis_frac: f64,
    total_input_frames: u64,
    total_output_frames: u64,
}

/// Frame length in samples. At 48 kHz this is a little over 21 ms, which is long
/// enough to contain a pitch period and short enough to follow a transient.
pub const STRETCH_FRAME: usize = 1024;
/// The hop the output advances by, which is half a frame.
const HOP: usize = STRETCH_FRAME / 2;
/// Half-width of the WSOLA alignment search window, in frames.
const SEARCH: usize = STRETCH_FRAME / 4;
/// The overlap between one frame and the next, counted in stereo samples.
const TAIL: usize = HOP * CHANNELS;

impl Default for TimeStretcher {
    fn default() -> Self {
        Self::new()
    }
}

/// Eight-lane independent accumulator dot product over `HOP = 512` frames so
/// LLVM SLP-vectorizes WSOLA cross-correlation into packed SIMD instructions.
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
        let mut fades = [0.0f32; HOP];
        for (offset, fade) in fades.iter_mut().enumerate() {
            *fade = (0.5 - 0.5 * (std::f64::consts::PI * (offset as f64 + 0.5) / HOP as f64).cos())
                as f32;
        }
        TimeStretcher {
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

    /// Stretches the buffered input by `factor`.
    ///
    /// A factor above one is faster: the output is shorter than the input by
    /// that factor, and the pitch is unchanged.
    pub fn produce(&mut self, factor: f64) {
        let factor = clean_stretch_factor(factor);
        // A faster factor consumes more input per output frame, which is what
        // makes the output shorter.
        let hop_step = HOP as f64 * factor;
        let frames = self.input.len() / CHANNELS;
        let mut read = 0usize;
        // The frame is taken from the read position plus an alignment offset,
        // and the offset reaches as far as twice the search window, so the
        // whole frame and the whole window have to be buffered before a frame
        // can be placed.
        while read + STRETCH_FRAME + SEARCH * 2 <= frames {
            let offset = if self.primed { self.best_offset(read) } else { 0 };
            let start = (read + offset) * CHANNELS;
            let segment = &self.input[start..start + STRETCH_FRAME * CHANNELS];
            if self.primed {
                // The frame overlaps what was just written by one hop, so that
                // much of the output is cross-faded in place rather than
                // appended: the output advances by the hop, not by the frame.
                let base = self.output.len() - TAIL;
                let tail_out = &mut self.output[base..];
                for ((out_frame, seg_frame), &fade) in tail_out
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
            let tail_frames = segment[(STRETCH_FRAME - HOP) * CHANNELS..].as_chunks::<CHANNELS>().0;
            for (dst, &[l, r]) in self.tail.iter_mut().zip(tail_frames) {
                let sum = l + r;
                *dst = if sum.is_finite() { sum } else { 0.0 };
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

    /// Takes the samples that are finished, keeping the tail that the next
    /// frame still has to blend with.
    pub fn take_output(&mut self) -> Vec<f32> {
        let mut out = Vec::new();
        self.take_output_into(&mut out);
        out
    }

    /// Appends the finished samples to `out`, keeping the tail that the next
    /// frame still has to blend with.
    ///
    /// This is `take_output` for a caller that already owns a buffer, which
    /// saves an allocation and a copy on every pump of the pipeline.
    pub fn take_output_into(&mut self, out: &mut Vec<f32>) {
        let keep = TAIL.min(self.output.len());
        let ready = self.output.len() - keep;
        self.total_output_frames += (ready / CHANNELS) as u64;
        out.extend_from_slice(&self.output[..ready]);
        self.output.drain(..ready);
    }

    /// Flushes the final buffered samples and overlap tail at stream end so
    /// high stretch factors (`factor > 1.0`) neither swallow flush padding via
    /// `self.skip` nor strand `TAIL` frames inside `self.output`.
    pub fn finish(&mut self, factor: f64, out: &mut Vec<f32>) {
        if self.total_input_frames == 0 {
            return;
        }
        let factor = clean_stretch_factor(factor);
        let target_total = ((self.total_input_frames as f64 / factor + 0.5) as u64).max(1);
        let remaining_target = target_total.saturating_sub(self.total_output_frames) as usize;
        if !self.input.is_empty() || !self.primed {
            self.skip = 0;
            let pad_samples = (STRETCH_FRAME + SEARCH * 2) * CHANNELS;
            self.input.resize(self.input.len() + pad_samples, 0.0);
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

    /// Finds the offset within the search window where the next frame lines up
    /// best with the tail already written, judged on the summed channels.
    fn best_offset(&mut self, read: usize) -> usize {
        let mut best = SEARCH;
        let mut best_score = f32::MIN;
        let input_frames = self.input.as_chunks::<CHANNELS>().0;
        let last = input_frames.len().saturating_sub(read).min(self.sums.len());
        if last < HOP {
            return best;
        }
        for (sum, &[l, r]) in self.sums[..last].iter_mut().zip(&input_frames[read..read + last]) {
            let s = l + r;
            *sum = if s.is_finite() { s } else { 0.0 };
        }
        let max_offset = (SEARCH * 2).min(last - HOP);
        let mut energy = 0.0f32;
        for offset in 0..=max_offset {
            let window: &[f32; HOP] = self.sums[offset..offset + HOP].try_into().unwrap();
            if offset % 64 == 0 {
                energy = energy_hop(window);
            } else {
                let prev = self.sums[offset - 1];
                let next = self.sums[offset + HOP - 1];
                energy = (energy + next * next - prev * prev).max(0.0);
                if !energy.is_finite() {
                    energy = energy_hop(window);
                }
            }
            let dot = dot_hop(window, &self.tail);
            if dot <= 0.0 && best_score >= 0.0 {
                continue;
            }
            // Normalising by the energy keeps a loud passage from always
            // winning over a quiet one that fits better.
            let score = dot / (energy.sqrt() + 1e-6);
            if score > best_score {
                best_score = score;
                best = offset;
            }
        }
        best
    }
}

/// The ratio that turns a source rate into the rate the node plays at, with the
/// timescale's pitch and rate folded in.
pub fn ratio_for(input_rate: u32, pitch: f64, rate: f64) -> f64 {
    let base = input_rate as f64 / crate::dsp::SAMPLE_RATE as f64;
    // The ratio is input frames consumed per output frame. A higher pitch or
    // playback rate consumes more source frames, just like FFmpeg's asetrate.
    base * pitch * rate
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The level of a tone after resampling, relative to what went in. `ratio`
    /// is input frames per output frame; the tone is `hertz` at `input_rate`.
    fn tone_gain(input_rate: f64, ratio: f64, hertz: f64) -> f64 {
        let frames = 48_000;
        let mut input = Vec::with_capacity(frames * CHANNELS);
        for n in 0..frames {
            let value = (2.0 * std::f64::consts::PI * hertz * n as f64 / input_rate).sin() as f32;
            input.extend([0.5 * value; CHANNELS]);
        }
        let mut resampler = SincResampler::new(ratio);
        resampler.push(&input);
        resampler.finish();
        let mut out = Vec::new();
        while !resampler.is_drained() && resampler.produce(4096, &mut out) > 0 {}
        let rms = |samples: &[f32]| {
            (samples.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / samples.len() as f64)
                .sqrt()
        };
        // The ends hold the filter's start-up and tail, which say nothing about it.
        let margin = 2000 * CHANNELS;
        rms(&out[margin..out.len() - margin]) / rms(&input[margin..input.len() - margin])
    }

    #[test]
    fn lowering_the_rate_removes_what_lies_above_the_new_nyquist_frequency() {
        // 192 kHz down to 48 kHz: a 40 kHz tone is far above the 24 kHz limit and
        // would fold down to 8 kHz if it got through. A filter that kept the
        // same sixteen taps after narrowing its cutoff let a good part of it by.
        let stopband = tone_gain(192_000.0, 4.0, 40_000.0);
        assert!(stopband < 0.001, "40 kHz came through at {stopband:.4} (-60 dB is 0.001)");
        let passband = tone_gain(192_000.0, 4.0, 1_000.0);
        assert!((0.94..1.06).contains(&passband), "1 kHz changed level to {passband:.4}");
        // The common 96 kHz source. A window this short has a wide transition
        // band, so the tone is placed well past it rather than just above 24 kHz.
        let stopband = tone_gain(96_000.0, 2.0, 44_000.0);
        assert!(stopband < 0.001, "44 kHz came through at {stopband:.4}");
        let passband = tone_gain(96_000.0, 2.0, 10_000.0);
        assert!((0.94..1.06).contains(&passband), "10 kHz changed level to {passband:.4}");
    }

    #[test]
    fn the_window_grows_with_the_ratio_and_stays_bounded() {
        assert_eq!(half_window(1.0), TAPS / 2, "not lowering the rate keeps the window");
        assert_eq!(half_window(0.5), TAPS, "twice the ratio, twice the window");
        assert_eq!(half_window(0.25), TAPS * 2);
        assert_eq!(half_window(1e-6), MAX_HALF, "an extreme ratio cannot grow it without bound");
    }

    fn tone(rate: f64, samples: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(samples * CHANNELS);
        for i in 0..samples {
            let value = (2.0 * std::f64::consts::PI * rate * i as f64 / 48_000.0).sin() as f32;
            out.push(value);
            out.push(value);
        }
        out
    }

    #[test]
    fn a_unity_ratio_resampler_reproduces_its_input() {
        let mut resampler = SincResampler::new(1.0);
        resampler.push(&tone(440.0, 4096));
        let mut out = Vec::new();
        let produced = resampler.produce(2048, &mut out);
        assert_eq!(produced, 2048);
        for i in 0..1024 {
            let source = i;
            let expected =
                (2.0 * std::f64::consts::PI * 440.0 * source as f64 / 48_000.0).sin() as f32;
            let got = out[i * CHANNELS];
            assert!((got - expected).abs() < 1e-3, "frame {i}: {got} != {expected}");
        }
    }

    #[test]
    fn the_unity_shortcut_matches_the_filter_it_replaces() {
        // The shortcut is only allowed to skip work, not to change the result:
        // it has to agree with the windowed sinc it stands in for.
        let mut shortcut = SincResampler::new(1.0);
        shortcut.push(&tone(440.0, 4096));
        let mut fast = Vec::new();
        shortcut.produce(2048, &mut fast);

        let mut filtered = SincResampler::new(1.0);
        filtered.push(&tone(440.0, 4096));
        let mut slow = Vec::new();
        filtered.produce_filtered(2048, &mut slow);

        assert_eq!(fast.len(), slow.len());
        for (i, (fast, slow)) in fast.iter().zip(slow.iter()).enumerate() {
            assert!((fast - slow).abs() < 1e-6, "sample {i}: {fast} != {slow}");
        }
    }

    #[test]
    fn the_weight_table_matches_the_weights_worked_out_per_frame() {
        // 44.1 kHz is the rate the table exists for: its fractional read
        // positions come round every 160 output frames.
        let ratio = 44_100.0 / 48_000.0;
        let table = Phases::new(ratio, 1.0, TAPS / 2).expect("44100 is a ratio of whole numbers");
        assert_eq!(table.period, 160);

        let mut tabled = SincResampler::new(ratio);
        tabled.push(&tone(1000.0, 8192));
        let mut with_table = Vec::new();
        tabled.produce(4096, &mut with_table);

        let mut computed = SincResampler::new(ratio);
        computed.without_phases();
        computed.push(&tone(1000.0, 8192));
        let mut without_table = Vec::new();
        computed.produce(4096, &mut without_table);

        assert_eq!(with_table.len(), without_table.len());
        for (i, (tabled, computed)) in with_table.iter().zip(without_table.iter()).enumerate() {
            assert!((tabled - computed).abs() < 1e-5, "sample {i}: {tabled} != {computed}");
        }
    }

    #[test]
    fn a_ratio_with_no_short_period_has_no_weight_table() {
        // 0.9173 is 9173/10000, whose period is far longer than a table is
        // worth, so its weights are worked out frame by frame.
        assert!(Phases::new(0.9173, 1.0, TAPS / 2).is_none());
    }

    #[test]
    fn halving_the_rate_halves_the_number_of_frames() {
        let mut resampler = SincResampler::new(2.0);
        resampler.push(&tone(200.0, 8192));
        let mut out = Vec::new();
        let produced = resampler.produce(8192, &mut out);
        // Half of each side of the window is held back as lookahead until the
        // stream ends: 16 input frames at this ratio, so 8 fewer output frames
        // than the whole of the input would give.
        assert!((produced as i64 - 4088).abs() <= 2, "produced {produced}");
    }

    #[test]
    fn finishing_keeps_short_sources_and_exact_output_duration() {
        for rate in [8_000, 22_050, 44_100, 48_000, 96_000] {
            for input_frames in [1, 4, 17, rate as usize] {
                let ratio = rate as f64 / 48_000.0;
                let mut resampler = SincResampler::new(ratio);
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
        // Count upward zero crossings over a settled window: 1 kHz at 48 kHz
        // crosses zero a thousand times a second, so 200 frames hold about four.
        let window = &out[200 * CHANNELS..400 * CHANNELS];
        let crossings = window
            .as_chunks::<CHANNELS>()
            .0
            .iter()
            .collect::<Vec<_>>()
            .windows(2)
            .filter(|pair| pair[0][0] < 0.0 && pair[1][0] >= 0.0)
            .count();
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
        let crossings =
            window.windows(2).filter(|pair| pair[0][0] < 0.0 && pair[1][0] >= 0.0).count();
        let hertz = crossings as f64 * 48_000.0 / window.len() as f64;
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
        let out = stretcher.take_output();
        let frames = out.len() / CHANNELS;
        assert!((frames as i64 - 8192).abs() < 2048, "{frames} frames out of 8192");
    }

    #[test]
    fn a_faster_factor_shortens_the_output() {
        let input = tone(440.0, 48_000);
        let mut slow = TimeStretcher::new();
        slow.push(&input);
        slow.produce(1.0);
        let slow_out = slow.take_output();

        let mut fast = TimeStretcher::new();
        fast.push(&input);
        fast.produce(2.0);
        let fast_out = fast.take_output();

        assert!(fast_out.len() < slow_out.len() * 3 / 4, "2x was not shorter");
        assert!(fast_out.len() > slow_out.len() * 2 / 5, "2x was too short");
    }

    #[test]
    fn a_stretched_tone_stays_a_tone() {
        let mut stretcher = TimeStretcher::new();
        stretcher.push(&tone(440.0, 48_000));
        stretcher.produce(1.5);
        let out = stretcher.take_output();
        let window = &out[8000 * CHANNELS..16_000 * CHANNELS];
        let crossings = window
            .as_chunks::<CHANNELS>()
            .0
            .iter()
            .collect::<Vec<_>>()
            .windows(2)
            .filter(|pair| pair[0][0] < 0.0 && pair[1][0] >= 0.0)
            .count();
        // 440 Hz over 8000 frames is 73 cycles.
        assert!((crossings as i64 - 73).abs() < 6, "{crossings} cycles, expected about 73");
    }

    #[test]
    fn finish_flushes_short_clips_and_high_speed_tails() {
        // Short clip (< 1536 frames) still produces output instead of dropping everything.
        let short = tone(440.0, 600);
        let mut stretcher = TimeStretcher::new();
        let mut out = Vec::new();
        stretcher.push(&short);
        stretcher.produce(2.0);
        stretcher.take_output_into(&mut out);
        assert!(out.is_empty());
        stretcher.finish(2.0, &mut out);
        assert_eq!(out.len(), 300 * CHANNELS);

        // Full 1-second clip at 5x stays within one WSOLA synthesis hop (512 frames) of 9600.
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
            assert!(!out.is_empty());
            assert!(out.iter().all(|s| s.is_finite()));
        }
    }

    #[test]
    fn phases_table_uses_tight_snap_tolerance_u64_products_and_aligns_ratio() {
        assert!(Phases::new(1.0 + 1e-10, 1.0, TAPS / 2).is_none());
        let steep = Phases::new(10_000.0, 1e-4, MAX_HALF).expect("exact integer ratio tabulates");
        assert_eq!(steep.period, 1);
        assert_eq!(steep.ratio, 10_000.0);

        let snapped_half = SincResampler::new(0.5 + 1e-13);
        assert_eq!(snapped_half.ratio, 0.5);
        assert!(snapped_half.phases.is_some());

        let snapped_unity = SincResampler::new(1.0 + 1e-13);
        assert_eq!(snapped_unity.ratio, 1.0);
        assert!(snapped_unity.phases.is_none());
    }
}
