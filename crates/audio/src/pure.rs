//! The in-process engine.
//!
//! Decode, filter, encode, all inside the node. The pipeline is
//! decoder → pre-filters → resample → stretch → post-filters → Opus, which is
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

/// Silence pushed through the stretcher to flush the end of a track: a whole
/// frame plus the search window the stretcher looks ahead by.
const FLUSH_SILENCE: usize = (crate::resample::STRETCH_FRAME + 512) * crate::dsp::CHANNELS;

pub struct PureFrames {
    decoder: Decoder,
    resampler: Option<SincResampler>,
    stretcher: Option<TimeStretcher>,
    stretch_factor: f64,
    dsp: DspChain,
    encoder: Encoder,
    /// Interleaved stereo at 48 kHz, ready to be cut into frames.
    staging: Vec<f32>,
    /// How much of `staging` has already been cut into frames. Frames are taken
    /// from behind this cursor and the buffer is moved down only when what is
    /// left no longer pays for the move, which is what keeps the steady state
    /// from shuffling the whole buffer on every frame.
    staging_at: usize,
    decoded: Vec<f32>,
    resampled: Vec<f32>,
    /// One frame of samples, reused for every packet so that the steady state
    /// allocates nothing.
    frame: [f32; FRAME_SAMPLES_STEREO],
    pcm: [i16; FRAME_SAMPLES_STEREO],
    /// The silence the flush at the end of a track pushes.
    silence: Vec<f32>,
    /// Source frames to discard before playback starts, which is how a seek is
    /// performed on a stream that cannot be seeked.
    skip: u64,
    drained: bool,
    flushed: bool,
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
        let time =
            set.time.unwrap_or(crate::filters::Timescale { speed: 1.0, pitch: 1.0, rate: 1.0 });
        let ratio = ratio_for(rate, time.pitch, time.rate);
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
        // Only a track that stretches ever needs the flush silence, so a track
        // that does not is not made to carry it.
        let silence = if stretcher.is_some() { vec![0.0; FLUSH_SILENCE] } else { Vec::new() };
        let encoder = Encoder::new(bitrate, complexity)?;
        let skip = if position_ms > 0 { position_ms as u64 * rate as u64 / 1000 } else { 0 };
        Ok(PureFrames {
            decoder,
            // A unity conversion needs neither a history window nor another
            // PCM buffer. This also preserves the first and last samples.
            resampler: (ratio != 1.0).then(|| SincResampler::new(ratio)),
            stretcher,
            stretch_factor,
            dsp: DspChain::new(set),
            encoder,
            staging: Vec::new(),
            staging_at: 0,
            decoded: Vec::new(),
            resampled: Vec::new(),
            frame: [0.0; FRAME_SAMPLES_STEREO],
            pcm: [0; FRAME_SAMPLES_STEREO],
            silence,
            skip,
            drained: false,
            flushed: false,
        })
    }

    /// Reads one step of the pipeline, reporting whether anything moved.
    fn pump(&mut self) -> Result<bool, AudioError> {
        let mut progressed = false;
        if !self.drained {
            self.decoded.clear();
            if self.decoder.decode(&mut self.decoded)? {
                // A chunk that falls entirely inside the seek is still progress:
                // the rest of the track is still to come, so the caller has to
                // keep pumping rather than read this as the end of the stream.
                progressed = true;
                if self.skip > 0 {
                    let frames = self.decoded.len() / 2;
                    let drop = (self.skip as usize).min(frames);
                    self.decoded.drain(..drop * 2);
                    self.skip -= drop as u64;
                }
                if !self.decoded.is_empty() {
                    self.dsp.process_pre(&mut self.decoded);
                    match (&mut self.resampler, &mut self.stretcher) {
                        (Some(resampler), _) => resampler.push(&self.decoded),
                        (None, Some(stretcher)) => stretcher.push(&self.decoded),
                        (None, None) => self.staging.extend_from_slice(&self.decoded),
                    }
                }
            } else {
                self.drained = true;
            }
        }

        match &mut self.stretcher {
            Some(stretcher) => {
                if let Some(resampler) = &mut self.resampler {
                    self.resampled.clear();
                    if resampler.produce(PRODUCE_LIMIT, &mut self.resampled) > 0 {
                        progressed = true;
                    }
                    if !self.resampled.is_empty() {
                        stretcher.push(&self.resampled);
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
                if self.drained && !self.flushed {
                    // The stretcher holds a frame and a search window back, so
                    // the tail of the track is still inside it. Pushing silence
                    // and producing once more flushes it.
                    stretcher.push(&self.silence);
                    stretcher.produce(self.stretch_factor);
                    let before = self.staging.len();
                    stretcher.take_output_into(&mut self.staging);
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
                break;
            }
        }
        let take = FRAME_SAMPLES_STEREO.min(self.staging.len() - self.staging_at);
        if take == 0 {
            return Ok(false);
        }
        // The sample buffer is reused: this runs every 20 ms for the life of a
        // track, so an allocation here would be an allocation forever. The last
        // frame of a track is usually short, and padding it with silence is what
        // every encoder does; it is also what keeps the final frame's duration
        // honest.
        let from = self.staging_at;
        if take == FRAME_SAMPLES_STEREO {
            let frame = &mut self.staging[from..from + take];
            self.dsp.process_post(frame);
            crate::opus::to_i16(frame, &mut self.pcm);
        } else {
            self.frame[..take].copy_from_slice(&self.staging[from..from + take]);
            self.frame[take..].fill(0.0);
            self.dsp.process_post(&mut self.frame);
            crate::opus::to_i16(&self.frame, &mut self.pcm);
        }
        self.staging_at += take;
        // Walk the cursor back once what has been used outweighs what is left,
        // so the move happens every many frames rather than on every frame.
        if self.staging_at >= FRAME_SAMPLES_STEREO && self.staging_at * 2 >= self.staging.len() {
            self.staging.drain(..self.staging_at);
            self.staging_at = 0;
        }
        // The packet goes into the caller's buffer, which is the one the
        // transport encrypts from, so no frame is allocated at all.
        packet.clear();
        packet.extend_from_slice(self.encoder.encode(&self.pcm)?);
        Ok(true)
    }
}
