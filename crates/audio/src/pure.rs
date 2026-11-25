//! The in-process engine.
//!
//! Decode, filter, encode, all inside the node. The pipeline is
//! decoder → 48 kHz → pre-filters → timescale → post-filters → Opus, which is
//! the order the FFmpeg graph applies the same settings in.

use rustlink_protocol::Track;

use crate::decode::Decoder;
use crate::dsp::DspChain;
use crate::engine::{Body, Frames};
use crate::error::AudioError;
use crate::filters::FilterSet;
use crate::opus::{Encoder, FRAME_SAMPLES_STEREO};
use crate::resample::{ratio_for, SincResampler, TimeStretcher};

/// How many output frames one resample call may produce. The buffer is drained
/// after every call, so this only bounds a single step.
const PRODUCE_LIMIT: usize = 1 << 16;

pub struct PureFrames {
    decoder: Decoder,
    source_resampler: Option<SincResampler>,
    resampler: Option<SincResampler>,
    stretcher: Option<TimeStretcher>,
    stretch_factor: f64,
    time: Option<crate::filters::Timescale>,
    dsp: DspChain,
    encoder: Encoder,
    /// Interleaved stereo at 48 kHz, ready to be cut into frames.
    staging: Vec<f32>,
    /// How much of `staging` has already been cut into frames. Frames are taken
    /// from behind this cursor and the buffer is moved down only when what is
    /// left no longer pays for the move, which is what keeps the steady state
    /// from shuffling the whole buffer on every frame.
    staging_at: usize,
    /// Shared temporary PCM buffer for resampling/stretching stages.
    scratch: Vec<f32>,
    pcm: [i16; FRAME_SAMPLES_STEREO],
    /// Source frames to discard before playback starts, which is how a seek is
    /// performed on a stream that cannot be seeked.
    skip: u64,
    drained: bool,
    flushed: bool,
    encoder_flushed: bool,
    encoded_any: bool,
}

impl PureFrames {
    pub(crate) fn supports_timescale(set: &FilterSet) -> bool {
        set.time.is_none_or(|time| (0.05..=5.0).contains(&(time.speed / time.pitch)))
    }

    pub fn start(
        _track: &Track,
        body: Body,
        position_ms: i64,
        set: &FilterSet,
        bitrate: i32,
        complexity: i32,
    ) -> Result<Self, AudioError> {
        let decoder = Decoder::new(body).map_err(|e| AudioError::NeedsFfmpeg(e.to_string()))?;
        let rate = decoder.rate();
        let active_time = set.effective_time();
        let time =
            active_time.unwrap_or(crate::filters::Timescale { speed: 1.0, pitch: 1.0, rate: 1.0 });
        let ratio = ratio_for(48_000, time.pitch, time.rate);
        let stretch_factor = time.speed / time.pitch;
        // Each API control is bounded, but their quotient can reach 100.
        // Keep the native stretcher's documented work bound and request the
        // FFmpeg path instead of silently clamping and playing the wrong tempo.
        if !Self::supports_timescale(set) {
            return Err(AudioError::NeedsFfmpeg(
                "combined speed/pitch is outside the native stretching range 0.05..5".into(),
            ));
        }
        let stretcher =
            if (stretch_factor - 1.0).abs() > 1e-9 { Some(TimeStretcher::new()) } else { None };
        let encoder = Encoder::new(bitrate, complexity)?;
        let skip = if position_ms > 0 {
            (position_ms as u128 * rate as u128 / 1000).min(u64::MAX as u128) as u64
        } else {
            0
        };
        Ok(PureFrames {
            decoder,
            source_resampler: (rate != 48_000).then(|| SincResampler::new(rate as f64 / 48_000.0)),
            // A unity conversion needs neither a history window nor another
            // PCM buffer. This also preserves the first and last samples.
            resampler: (ratio != 1.0).then(|| SincResampler::new(ratio)),
            stretcher,
            stretch_factor,
            time: active_time,
            dsp: DspChain::new(set),
            encoder,
            staging: Vec::new(),
            staging_at: 0,
            scratch: Vec::new(),
            pcm: [0; FRAME_SAMPLES_STEREO],
            skip,
            drained: false,
            flushed: false,
            encoder_flushed: false,
            encoded_any: false,
        })
    }

    /// Reads one step of the pipeline, reporting whether anything moved.
    fn pump(&mut self) -> Result<bool, AudioError> {
        // Zero-copy fast path for 48 kHz sources without timescale (>95% of tracks):
        // decode straight onto the tail of `self.staging` and run pre-DSP in place.
        if self.source_resampler.is_none() && self.resampler.is_none() && self.stretcher.is_none() {
            if self.drained {
                return Ok(false);
            }
            let start = self.staging.len();
            if !self.decoder.decode(&mut self.staging)? {
                self.drained = true;
                return Ok(false);
            }
            if self.skip > 0 {
                let frames = (self.staging.len() - start) / 2;
                let drop = self.skip.min(frames as u64) as usize;
                self.staging.drain(start..start + drop * 2);
                self.skip -= drop as u64;
            }
            if self.staging.len() > start {
                self.dsp.process_pre(&mut self.staging[start..]);
            }
            return Ok(true);
        }

        let mut progressed = false;
        // Drain the furthest downstream converter before fetching more input.
        // Slow combined pitch/rate can expand one chunk hundreds of times;
        // decoding another chunk on every pump would retain an entire song.
        let time_ready = self.resampler.as_ref().is_some_and(|r| r.available() > 0);
        let source_ready = self.source_resampler.as_ref().is_some_and(|r| r.available() > 0);
        self.scratch.clear();
        if !self.drained && !source_ready && !time_ready {
            if self.decoder.decode(&mut self.scratch)? {
                // A chunk that falls entirely inside the seek is still progress:
                // the rest of the track is still to come, so the caller has to
                // keep pumping rather than read this as the end of the stream.
                progressed = true;
                if self.skip > 0 {
                    let frames = self.scratch.len() / 2;
                    let drop = self.skip.min(frames as u64) as usize;
                    self.scratch.drain(..drop * 2);
                    self.skip -= drop as u64;
                }
            } else {
                self.drained = true;
            }
        }

        if let Some(resampler) = &mut self.source_resampler {
            if !self.scratch.is_empty() {
                resampler.push(&self.scratch);
                self.scratch.clear();
            }
            if self.drained {
                resampler.finish();
            }
            if !time_ready {
                progressed |= resampler.produce(PRODUCE_LIMIT, &mut self.scratch) > 0;
            }
        }

        if !self.scratch.is_empty() {
            self.dsp.process_pre(&mut self.scratch);
            match (&mut self.resampler, &mut self.stretcher) {
                (Some(resampler), _) => resampler.push(&self.scratch),
                (None, Some(stretcher)) => stretcher.push(&self.scratch),
                (None, None) => self.staging.extend_from_slice(&self.scratch),
            }
            self.scratch.clear();
        }

        let source_finished =
            self.drained && self.source_resampler.as_ref().is_none_or(SincResampler::is_drained);
        if source_finished {
            if let Some(resampler) = &mut self.resampler {
                resampler.finish();
            }
        }

        match &mut self.stretcher {
            Some(stretcher) => {
                if let Some(resampler) = &mut self.resampler {
                    self.scratch.clear();
                    if resampler.produce(PRODUCE_LIMIT, &mut self.scratch) > 0 {
                        progressed = true;
                        stretcher.push(&self.scratch);
                        self.scratch.clear();
                    }
                }
                stretcher.produce(self.stretch_factor);
                // The stretcher's output goes straight into the staging buffer,
                // which is what keeps a pump from allocating.
                let before = self.staging.len();
                stretcher.take_output_into(&mut self.staging);
                if self.staging.len() > before {
                    progressed = true;
                }
                if source_finished
                    && self.resampler.as_ref().is_none_or(SincResampler::is_drained)
                    && !self.flushed
                {
                    let before = self.staging.len();
                    stretcher.finish(self.stretch_factor, &mut self.staging);
                    if self.staging.len() > before {
                        progressed = true;
                    }
                    self.flushed = true;
                }
            }
            None => {
                // With no stretcher in the chain the resampler writes straight
                // into the buffer the frames are cut from, which saves copying
                // the whole stream one more time.
                if let Some(resampler) = &mut self.resampler {
                    if resampler.produce(PRODUCE_LIMIT, &mut self.staging) > 0 {
                        progressed = true;
                    }
                }
            }
        }
        Ok(progressed)
    }
}

impl Frames for PureFrames {
    fn next_frame(&mut self, packet: &mut Vec<u8>) -> Result<bool, AudioError> {
        while self.staging.len() - self.staging_at < FRAME_SAMPLES_STEREO {
            if !self.pump()? {
                // RTP has no Ogg pre-skip/granule metadata. Send the encoder's
                // delayed tail as well; otherwise a frame-aligned song loses
                // its final 312 samples instead of merely carrying codec delay.
                if !self.encoder_flushed {
                    if self.encoded_any || self.staging.len() > self.staging_at {
                        self.staging.resize(
                            self.staging.len() + self.encoder.lookahead() as usize * 2,
                            0.0,
                        );
                    }
                    self.encoder_flushed = true;
                }
                break;
            }
        }
        let take = FRAME_SAMPLES_STEREO.min(self.staging.len() - self.staging_at);
        if take == 0 {
            return Ok(false);
        }
        // Process post-DSP in place on `staging`, quantise directly into `pcm`,
        // and zero-pad the final short frame's remainder.
        let from = self.staging_at;
        let slice = &mut self.staging[from..from + take];
        self.dsp.process_post(slice);
        crate::opus::to_i16(slice, &mut self.pcm[..take]);
        self.pcm[take..].fill(0);
        self.staging_at += take;
        // Walk the cursor back once what has been used outweighs what is left,
        // so the move happens every many frames rather than on every frame.
        if self.staging_at >= FRAME_SAMPLES_STEREO && self.staging_at * 2 >= self.staging.len() {
            self.staging.drain(..self.staging_at);
            self.staging_at = 0;
        }
        // Encode directly into the caller's packet buffer with zero extra copies.
        self.encoder.encode_into(&self.pcm, packet)?;
        self.encoded_any = true;
        Ok(true)
    }

    fn supports_live_filters(&self) -> bool {
        true
    }

    fn set_filters(&mut self, set: &FilterSet) -> bool {
        if set.effective_time() != self.time {
            return false;
        }
        self.dsp.reconfigure(set);
        true
    }
}
