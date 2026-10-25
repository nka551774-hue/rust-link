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

/// Taps on each side of the interpolation point. Sixteen is enough that the
/// error sits below the noise floor of the Opus encoder that follows.
const TAPS: usize = 16;

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
    /// Input frames that have already been consumed by the read position.
    consumed: usize,
    /// Which row of the weight table the next output frame reads.
    phase: usize,
    /// The weights of every phase, when the ratio repeats them.
    phases: Option<Phases>,
    buffer: Vec<f32>,
}

impl SincResampler {
    /// `ratio` is input frames per output frame: 48000 in and 48000 out is 1.0,
    /// 44100 in and 48000 out is 0.91875.
    pub fn new(ratio: f64) -> Self {
        let ratio = ratio.max(1e-6);
        let cutoff = if ratio > 1.0 { 1.0 / ratio } else { 1.0 };
        SincResampler {
            ratio,
            // The first output frame needs TAPS/2 frames of history behind it,
            // so the read position starts that far in.
            position: (TAPS / 2) as f64,
            consumed: 0,
            phase: 0,
            // The unity ratio has its own path that needs no weights at all.
            phases: if ratio == 1.0 { None } else { Phases::new(ratio, cutoff) },
            buffer: Vec::new(),
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
        self.buffer.extend_from_slice(input);
    }

    /// How many whole output frames can be produced from what has been pushed.
    pub fn available(&self) -> usize {
        let frames = self.buffer.len() / CHANNELS;
        let last_needed = self.position + (TAPS / 2) as f64;
        if last_needed + 1.0 >= frames as f64 {
            return 0;
        }
        let count = ((frames as f64 - 1.0 - (TAPS / 2) as f64) - self.position) / self.ratio;
        if count < 0.0 {
            0
        } else {
            count.floor() as usize + 1
        }
    }

    /// Produces up to `limit` output frames, appending them to `out`.
    ///
    /// Returns how many frames were produced, which is zero when the caller has
    /// not pushed enough input yet.
    pub fn produce(&mut self, limit: usize, out: &mut Vec<f32>) -> usize {
        let produced = if self.ratio == 1.0 {
            self.produce_unity(limit, out)
        } else {
            self.produce_filtered(limit, out)
        };
        self.compact();
        produced
    }

    /// The unity-ratio case: every tap but the centre falls on a whole number
    /// of input frames, where the windowed sinc is zero, so the filter passes
    /// the input through untouched. Most sources are already 48 kHz, and this
    /// is what keeps them from paying for a filter that does not move them.
    fn produce_unity(&mut self, limit: usize, out: &mut Vec<f32>) -> usize {
        let frames = self.buffer.len() / CHANNELS;
        // The same window rule the filtered path uses, so both report the same
        // number of frames for the same input: the last frame that can be
        // produced has TAPS/2 frames ahead of it.
        let centre = self.position as usize;
        let count = limit.min(frames.saturating_sub(TAPS / 2).saturating_sub(centre));
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
        let frames = self.buffer.len() / CHANNELS;
        let mut produced = 0;
        // Downsampling needs the filter narrowed to the new Nyquist, or the
        // content above it folds back as aliasing.
        let cutoff = if self.ratio > 1.0 { 1.0 / self.ratio } else { 1.0 };
        while produced < limit {
            let centre = self.position.floor() as usize;
            let start = centre + 1 - TAPS / 2;
            let end = centre + TAPS / 2 + 1;
            if end > frames {
                break;
            }
            // The filter weights depend on the read position, not on the
            // channel, so they are worked out once and used for both. The
            // window is exactly TAPS wide, which is what lets them live in an
            // array.
            let mut weights = [0.0f64; TAPS];
            let fraction = self.position.fract();
            // The table's rows are indexed by the phase the read position is
            // expected to be on, and a ratio that repeats is what keeps a
            // 44.1 kHz source from paying for a sine and two cosines per tap.
            // The position only ever drifts by a hair, but a hair either side
            // of a whole frame pairs a row with the wrong samples, so a
            // position that has drifted over one is worked out frame by frame
            // instead.
            let row = self
                .phases
                .as_ref()
                .filter(|phases| (fraction - phases.fractions[self.phase]).abs() < 1e-3);
            let weight = match row {
                Some(phases) => {
                    let base = self.phase * TAPS;
                    weights.copy_from_slice(&phases.weights[base..base + TAPS]);
                    phases.sums[self.phase]
                }
                None => {
                    let mut weight = 0.0f64;
                    for (tap_index, w) in weights.iter_mut().enumerate() {
                        *w = tap(fraction, tap_offset(tap_index), cutoff);
                        weight += *w;
                    }
                    weight
                }
            };
            if let Some(phases) = &self.phases {
                self.phase = (self.phase + 1) % phases.period;
            }
            // Both channels are summed in one pass over the window: the input
            // is then read in order rather than once per channel, and each
            // weight is read once, which is what a source that is not at
            // 48 kHz pays for on every output frame.
            let mut sums = [0.0f64; CHANNELS];
            for (tap_index, index) in (start..end).enumerate() {
                let tap_weight = weights[tap_index];
                let at = index * CHANNELS;
                for (channel, sum) in sums.iter_mut().enumerate() {
                    *sum += self.buffer[at + channel] as f64 * tap_weight;
                }
            }
            for sum in sums {
                let value = if weight.abs() > 1e-9 { sum / weight } else { 0.0 };
                out.push(value as f32);
            }
            self.position += self.ratio;
            produced += 1;
        }
        produced
    }

    /// Drops the input behind the read position, which will never be read
    /// again because the filter only looks half a window back.
    fn compact(&mut self) {
        // Everything before the next read position's window is never read again.
        let keep_from = (self.position.floor() as usize).saturating_sub(TAPS / 2 + 1);
        if keep_from > self.consumed + 4096 {
            let drop = (keep_from - self.consumed) * CHANNELS;
            self.buffer.drain(..drop);
            self.position -= (keep_from - self.consumed) as f64;
            self.consumed = keep_from;
        }
    }
}

/// The filter weights of every fractional read position a ratio repeats.
///
/// A ratio that comes from a whole sample rate is a ratio of two whole numbers,
/// so the fractional position of the read pointer comes back to where it was
/// every so many output frames. Tabulating those positions turns a sine and two
/// cosines per tap into a lookup, which is what a 44.1 kHz source pays for on
/// every frame otherwise. A ratio whose positions do not come round within
/// `MAX_PHASES` is left to be worked out frame by frame.
#[derive(Debug, Clone)]
struct Phases {
    /// How many output frames the fractional position takes to come round.
    period: usize,
    /// The fractional read position of each row, which is what says whether a
    /// row still matches the position it is being read for.
    fractions: Vec<f64>,
    /// `period` sets of `TAPS` weights, in the order the taps are summed.
    weights: Vec<f64>,
    /// The sum of each set, which normalises it.
    sums: Vec<f64>,
}

impl Phases {
    /// Builds the table for `ratio`, or nothing when the fractional positions
    /// do not come round within a period worth tabulating.
    fn new(ratio: f64, cutoff: f64) -> Option<Phases> {
        let period = (1..=MAX_PHASES).find(|period| {
            let turns = ratio * *period as f64;
            (turns - turns.round()).abs() < 1e-9
        })?;
        let mut fractions = Vec::with_capacity(period);
        let mut weights = Vec::with_capacity(period * TAPS);
        let mut sums = Vec::with_capacity(period);
        for phase in 0..period {
            let fraction = (phase as f64 * ratio).fract();
            fractions.push(fraction);
            let mut sum = 0.0f64;
            for tap_index in 0..TAPS {
                let w = tap(fraction, tap_offset(tap_index), cutoff);
                weights.push(w);
                sum += w;
            }
            sums.push(sum);
        }
        Some(Phases { period, fractions, weights, sums })
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
fn tap_offset(tap_index: usize) -> i64 {
    tap_index as i64 + 1 - (TAPS / 2) as i64
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
    frame: usize,
    hop: usize,
    search: usize,
    input: Vec<f32>,
    /// At high tempos the analysis hop can pass the buffered input. Keep the
    /// remainder as a skip for the next push instead of draining past the end.
    skip: usize,
    /// Samples that are finished, less the tail the next frame still has to
    /// blend with. The stretcher owns them so that a caller cannot drain the
    /// tail out from under it.
    output: Vec<f32>,
    /// The tail of the last frame written, which the next frame aligns to.
    tail: [f32; TAIL],
    /// The summed channels of the frames the alignment search reads, kept
    /// between frames so that the search does not allocate one of its own.
    sums: Vec<f64>,
    /// The cross-fade of each frame of the overlap, which is the same curve on
    /// every frame and so is worked out once.
    fades: [f64; HOP],
    primed: bool,
}

/// Frame length in samples. At 48 kHz this is a little over 21 ms, which is long
/// enough to contain a pitch period and short enough to follow a transient.
pub const STRETCH_FRAME: usize = 1024;
/// The hop the output advances by, which is half a frame.
const HOP: usize = STRETCH_FRAME / 2;
/// The overlap between one frame and the next, which is also the hop the output
/// advances by, counted in samples.
const TAIL: usize = HOP * CHANNELS;

impl Default for TimeStretcher {
    fn default() -> Self {
        Self::new()
    }
}

impl TimeStretcher {
    pub fn new() -> Self {
        let frame = STRETCH_FRAME;
        let hop = HOP;
        let search = frame / 4;
        let mut fades = [0.0f64; HOP];
        for (offset, fade) in fades.iter_mut().enumerate() {
            *fade = 0.5 - 0.5 * (std::f64::consts::PI * (offset as f64 + 0.5) / hop as f64).cos();
        }
        TimeStretcher {
            frame,
            hop,
            search,
            input: Vec::new(),
            skip: 0,
            output: Vec::new(),
            tail: [0.0; TAIL],
            sums: vec![0.0; search * 2 + hop],
            fades,
            primed: false,
        }
    }

    pub fn push(&mut self, input: &[f32]) {
        let skipped = self.skip.min(input.len() / CHANNELS);
        self.skip -= skipped;
        self.input.extend_from_slice(&input[skipped * CHANNELS..]);
    }

    /// Stretches the buffered input by `factor`.
    ///
    /// A factor above one is faster: the output is shorter than the input by
    /// that factor, and the pitch is unchanged.
    pub fn produce(&mut self, factor: f64) {
        let factor = factor.clamp(0.05, 5.0);
        // A faster factor consumes more input per output frame, which is what
        // makes the output shorter.
        let analysis = (self.hop as f64 * factor).round().max(1.0) as usize;
        let frames = self.input.len() / CHANNELS;
        let mut read = 0usize;
        // The frame is taken from the read position plus an alignment offset,
        // and the offset reaches as far as twice the search window, so the
        // whole frame and the whole window have to be buffered before a frame
        // can be placed.
        while read + self.frame + self.search * 2 <= frames {
            let offset = if self.primed { self.best_offset(read) } else { 0 };
            let start = (read + offset) * CHANNELS;
            let segment = &self.input[start..start + self.frame * CHANNELS];
            if self.primed {
                // The frame overlaps what was just written by one hop, so that
                // much of the output is cross-faded in place rather than
                // appended: the output advances by the hop, not by the frame.
                let base = self.output.len() - TAIL;
                // The fade belongs to the frame, not to the sample, and the
                // curve is the same every time, so it is read from the table.
                for (frame, fade) in self.fades.iter().enumerate() {
                    for channel in 0..CHANNELS {
                        let i = frame * CHANNELS + channel;
                        self.output[base + i] = (self.output[base + i] as f64 * (1.0 - *fade)
                            + segment[i] as f64 * *fade)
                            as f32;
                    }
                }
                self.output.extend_from_slice(&segment[TAIL..]);
            } else {
                self.output.extend_from_slice(segment);
                self.primed = true;
            }
            self.tail.copy_from_slice(&segment[(self.frame - self.hop) * CHANNELS..]);
            read += analysis;
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
        out.extend_from_slice(&self.output[..ready]);
        self.output.drain(..ready);
    }

    /// Finds the offset within the search window where the next frame lines up
    /// best with the tail already written, judged on the summed channels.
    fn best_offset(&mut self, read: usize) -> usize {
        let mut best = self.search;
        let mut best_score = f64::MIN;
        // The tail is the same for every candidate, so its summed channels are
        // worked out once instead of once per offset.
        let mut tail = [0.0f64; HOP];
        for (i, sum) in tail.iter_mut().enumerate() {
            for channel in 0..CHANNELS {
                *sum += self.tail[i * CHANNELS + channel] as f64;
            }
        }
        // Neighbouring candidates share almost the whole of their window, so
        // the summed channels of the input are worked out once for the search
        // range rather than once per candidate.
        let frames = self.input.len() / CHANNELS;
        let last = frames.saturating_sub(read).min(self.sums.len());
        for (i, sum) in self.sums[..last].iter_mut().enumerate() {
            *sum = 0.0;
            for channel in 0..CHANNELS {
                *sum += self.input[(read + i) * CHANNELS + channel] as f64;
            }
        }
        for offset in 0..=self.search * 2 {
            if offset + self.hop > last {
                break;
            }
            let mut dot = 0.0f64;
            let mut energy = 0.0f64;
            for (i, tail_sum) in tail.iter().enumerate() {
                let a = self.sums[offset + i];
                dot += a * tail_sum;
                energy += a * a;
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
        // The filter reads half a window ahead, so output frame k is input frame
        // k + TAPS/2. The first frames sit inside the warm-up, so the check
        // starts after it.
        for i in 64..1024 {
            let source = i + TAPS / 2;
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
        let table = Phases::new(ratio, 1.0).expect("44100 is a ratio of whole numbers");
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
        assert!(Phases::new(0.9173, 1.0).is_none());
    }

    #[test]
    fn halving_the_rate_halves_the_number_of_frames() {
        let mut resampler = SincResampler::new(2.0);
        resampler.push(&tone(200.0, 8192));
        let mut out = Vec::new();
        let produced = resampler.produce(8192, &mut out);
        assert!((produced as i64 - 4088).abs() <= 2, "produced {produced}");
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
}
