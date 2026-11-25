//! The in-process engine.
//!
//! Decode, filter, encode, all inside the node. The pipeline is
//! decoder → 48 kHz → pre-filters → timescale → post-filters → Opus, which is
//! the order the FFmpeg graph applies the same settings in.

use rustlink_protocol::Track;

use crate::decode::{Decoder, PassthroughStep};
use crate::dsp::DspChain;
use crate::engine::{Body, Frames};
use crate::error::AudioError;
use crate::filters::FilterSet;
use crate::opus::{Encoder, FRAME_SAMPLES_STEREO};
use crate::resample::{ratio_for, SincResampler, TimeStretcher};

/// How many output frames one resample call may produce. The buffer is drained
/// after every call, so this only bounds a single step.
const PRODUCE_LIMIT: usize = 1 << 16;

/// Minimum configured output bitrate at which 48 kHz Opus sources (`itag 251`,
/// Ogg Opus) stream directly without re-encoding; lower operator caps transcode
/// to honour explicit bandwidth limits.
const MIN_PASSTHROUGH_BITRATE: i32 = 96_000;

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
    /// Source frames to discard before playback starts, which is how a seek is
    /// performed on a stream that cannot be seeked.
    skip: u64,
    /// True while 48 kHz Opus packets are forwarded directly without decode+encode.
    passthrough: bool,
    passthrough_count: u64,
    passthrough_ring: [Vec<u8>; 2],
    handoff_pending: bool,
    drained: bool,
    flushed: bool,
    encoder_flushed: bool,
    encoded_any: bool,
}

#[inline]
fn skip_frames(skip: &mut u64, buf: &mut Vec<f32>, start: usize) {
    if *skip > 0 {
        let frames = (buf.len() - start) / 2;
        let drop = (*skip).min(frames as u64) as usize;
        buf.drain(start..start + drop * 2);
        *skip -= drop as u64;
    }
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
        let dsp = DspChain::new(set);
        let passthrough = bitrate >= MIN_PASSTHROUGH_BITRATE
            && skip == 0
            && active_time.is_none()
            && dsp.is_empty()
            && decoder.supports_opus_passthrough();
        Ok(PureFrames {
            decoder,
            source_resampler: (rate != 48_000).then(|| SincResampler::new(rate as f64 / 48_000.0)),
            // A unity conversion needs neither a history window nor another
            // PCM buffer. This also preserves the first and last samples.
            resampler: (ratio != 1.0).then(|| SincResampler::new(ratio)),
            stretcher,
            stretch_factor,
            time: active_time,
            dsp,
            encoder,
            staging: Vec::new(),
            staging_at: 0,
            scratch: Vec::new(),
            skip,
            passthrough,
            passthrough_count: 0,
            passthrough_ring: [Vec::new(), Vec::new()],
            handoff_pending: false,
            drained: false,
            flushed: false,
            encoder_flushed: false,
            encoded_any: false,
        })
    }

    fn passthrough_history(count: u64, ring: &[Vec<u8>; 2]) -> (Option<&[u8]>, Option<&[u8]>) {
        match count {
            0 => (None, None),
            1 => (None, Some(ring[0].as_slice())),
            n => {
                let older = ring[(n % 2) as usize].as_slice();
                let latest = ring[((n - 1) % 2) as usize].as_slice();
                (Some(older), Some(latest))
            }
        }
    }

    /// Primes `self.encoder`'s lookahead and MDCT overlap state from the final
    /// samples of packet `K` (`k_tail`) and the leading `lookahead` samples of
    /// packet `K+1` (`next_pcm`) using a discarded 10 ms (480-sample) encode so
    /// live filter changes or final-packet trimming transition with zero sample
    /// offset and zero discontinuity.
    fn finish_passthrough_handoff(
        &mut self,
        k_tail: &mut [f32],
        next_pcm: &mut [f32],
    ) -> Result<(), AudioError> {
        if !next_pcm.is_empty() {
            self.dsp.process_pre(next_pcm);
        }
        if k_tail.is_empty() {
            self.staging.extend_from_slice(next_pcm);
            return Ok(());
        }
        if next_pcm.is_empty() {
            self.drained = true;
            self.encoder_flushed = true;
            return Ok(());
        }
        let lookahead = (self.encoder.lookahead().max(0) as usize).min(480);
        let hist_frames = 480 - lookahead;
        let k_frames = k_tail.len() / 2;
        let take_k = hist_frames.min(k_frames);
        let pad_k = hist_frames - take_k;

        let mut prime = [0.0f32; 480 * 2];
        let k_src = &mut k_tail[(k_frames - take_k) * 2..];
        self.dsp.process_pre(k_src);
        prime[pad_k * 2..hist_frames * 2].copy_from_slice(k_src);

        let next_frames = next_pcm.len() / 2;
        let take_next = lookahead.min(next_frames);
        prime[hist_frames * 2..(hist_frames + take_next) * 2]
            .copy_from_slice(&next_pcm[..take_next * 2]);
        self.dsp.process_post(&mut prime);

        let mut discard = Vec::new();
        self.encoder.encode_float_frame_size(&mut prime, 480, &mut discard)?;
        self.encoded_any = true;

        if next_frames > take_next {
            self.staging.extend_from_slice(&next_pcm[take_next * 2..]);
        }
        Ok(())
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
            if !self.decoder.decode_with_skip(&mut self.skip, &mut self.staging)? {
                self.drained = true;
                return Ok(false);
            }
            skip_frames(&mut self.skip, &mut self.staging, start);
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
            if self.decoder.decode_with_skip(&mut self.skip, &mut self.scratch)? {
                // A chunk that falls entirely inside the seek is still progress:
                // the rest of the track is still to come, so the caller has to
                // keep pumping rather than read this as the end of the stream.
                progressed = true;
                skip_frames(&mut self.skip, &mut self.scratch, 0);
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
        if self.passthrough {
            let mut k_tail = Vec::new();
            let mut next_pcm = Vec::new();
            let step = {
                let (older, latest) =
                    Self::passthrough_history(self.passthrough_count, &self.passthrough_ring);
                self.decoder.try_passthrough_next(
                    self.passthrough_count,
                    packet,
                    older,
                    latest,
                    &mut k_tail,
                    &mut next_pcm,
                )?
            };
            match step {
                PassthroughStep::Packet => {
                    let slot = &mut self.passthrough_ring[(self.passthrough_count % 2) as usize];
                    slot.clear();
                    slot.extend_from_slice(packet);
                    self.passthrough_count += 1;
                    self.encoded_any = true;
                    return Ok(true);
                }
                PassthroughStep::Eof => {
                    packet.clear();
                    self.drained = true;
                    self.encoder_flushed = true;
                    return Ok(false);
                }
                PassthroughStep::FallbackToTranscode => {
                    self.passthrough = false;
                    self.finish_passthrough_handoff(&mut k_tail, &mut next_pcm)?;
                }
            }
        } else if self.handoff_pending {
            self.handoff_pending = false;
            let mut k_tail = Vec::new();
            {
                let (older, latest) =
                    Self::passthrough_history(self.passthrough_count, &self.passthrough_ring);
                self.decoder.warm_from_passthrough(older, latest, &mut k_tail)?;
            }
            let mut next_pcm = Vec::new();
            if !self.decoder.decode(&mut next_pcm)? {
                self.drained = true;
            }
            self.finish_passthrough_handoff(&mut k_tail, &mut next_pcm)?;
        }

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
        // Process post-DSP in place on `staging` and feed 32-bit float samples
        // straight into `opus_encode_float` (preserving full 24-bit dynamic range
        // and stateful soft-clipping without 16-bit quantisation loss).
        let from = self.staging_at;
        let slice = &mut self.staging[from..from + take];
        self.dsp.process_post(slice);
        if take == FRAME_SAMPLES_STEREO {
            self.encoder.encode_float_into(slice, packet)?;
        } else {
            let mut tail = [0.0f32; FRAME_SAMPLES_STEREO];
            tail[..take].copy_from_slice(slice);
            self.encoder.encode_float_into(&mut tail, packet)?;
        }
        self.staging_at += take;
        // Walk the cursor back once what has been used outweighs what is left,
        // so the move happens every many frames rather than on every frame.
        if self.staging_at >= FRAME_SAMPLES_STEREO && self.staging_at * 2 >= self.staging.len() {
            self.staging.drain(..self.staging_at);
            self.staging_at = 0;
        }
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
        if self.passthrough && !self.dsp.is_empty() {
            self.passthrough = false;
            if self.passthrough_count > 0 {
                self.handoff_pending = true;
            }
        }
        true
    }
}
