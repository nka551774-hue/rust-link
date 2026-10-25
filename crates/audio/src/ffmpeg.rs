//! The FFmpeg engine.
//!
//! FFmpeg is handed the whole job: the stream on its standard input, the filter
//! graph, and Ogg Opus on its standard output. The node reads the packets back
//! out of the Ogg pages and sends them on.
//!
//! Nothing a client sends reaches the command line as text. The filter graph is
//! rendered from validated numbers, and FFmpeg only ever reads from a pipe, so
//! it cannot be pointed at a file or a URL.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::engine::{Body, Frames};
use crate::error::AudioError;
use crate::filters::FilterSet;
use crate::ogg::OggOpusReader;

/// How long a decoder may produce nothing before the track is declared stuck.
const STALL: Duration = Duration::from_secs(10);
/// How much of FFmpeg's diagnostics to keep for an error message.
const STDERR_LIMIT: usize = 32 << 10;
/// Bound decode-ahead to 160 ms. Backpressure reaches FFmpeg's pipe, including
/// while paused, so a long track cannot accumulate in an unbounded channel.
const PACKET_QUEUE: usize = 8;

pub struct FfmpegFrames {
    packets: Receiver<Result<Vec<u8>, AudioError>>,
    recycle: SyncSender<Vec<u8>>,
    child: Child,
    errors: Arc<Mutex<Vec<u8>>>,
    input_error: Arc<Mutex<Option<std::io::Error>>>,
    finished: bool,
}

impl FfmpegFrames {
    pub fn start(
        program: &str,
        body: Body,
        position_ms: i64,
        set: &FilterSet,
        bitrate: i32,
        complexity: i32,
    ) -> Result<Self, AudioError> {
        let mut command = Command::new(program);
        command
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            .arg("-threads")
            .arg("1")
            .arg("-filter_threads")
            .arg("1")
            .arg("-probesize")
            .arg("1048576")
            .arg("-analyzeduration")
            .arg("3000000")
            .arg("-i")
            .arg("pipe:0");
        if position_ms > 0 {
            // Seeking a pipe requires decoding and discarding earlier samples.
            command.arg("-ss").arg(format!("{:.3}", position_ms as f64 / 1000.0));
        }
        command
            .arg("-map")
            .arg("0:a:0")
            .arg("-vn")
            .arg("-sn")
            .arg("-dn")
            .arg("-af")
            .arg(set.render_ffmpeg())
            .arg("-ac")
            .arg("2")
            .arg("-ar")
            .arg("48000")
            .arg("-c:a")
            .arg("libopus")
            .arg("-b:a")
            .arg(bitrate.to_string())
            .arg("-application")
            .arg("audio")
            .arg("-compression_level")
            .arg(complexity.to_string())
            .arg("-vbr")
            .arg("on")
            .arg("-fec")
            .arg("1")
            .arg("-packet_loss")
            .arg("10");
        command
            .arg("-frame_duration")
            .arg("20")
            .arg("-f")
            .arg("opus")
            .arg("-page_duration")
            .arg("20000")
            .arg("-flush_packets")
            .arg("1")
            .arg("pipe:1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn().map_err(|e| AudioError::Ffmpeg(e.to_string()))?;
        let mut stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let mut stderr = child.stderr.take().expect("stderr was piped");

        // The body is written on its own thread, because the reader below is
        // what the caller waits on and a slow source must not block it.
        let input_error = Arc::new(Mutex::new(None));
        let input_failure = Arc::clone(&input_error);
        thread::Builder::new()
            .name("rustlink-ffmpeg-in".into())
            .spawn(move || {
                let mut body = body;
                if let Err(error) = std::io::copy(&mut body, &mut stdin) {
                    // Record the failure before closing stdin. FFmpeg can exit
                    // successfully after decoding a truncated input, but that
                    // must not turn a failed media connection into normal EOF.
                    if let Ok(mut guard) = input_failure.lock() {
                        *guard = Some(error);
                    }
                }
            })
            .map_err(|e| AudioError::Ffmpeg(e.to_string()))?;

        let errors = Arc::new(Mutex::new(Vec::new()));
        let collected = Arc::clone(&errors);
        thread::Builder::new()
            .name("rustlink-ffmpeg-err".into())
            .spawn(move || {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                while let Ok(read) = stderr.read(&mut chunk) {
                    if read == 0 {
                        break;
                    }
                    if buffer.len() < STDERR_LIMIT {
                        buffer.extend_from_slice(&chunk[..read]);
                    }
                }
                if let Ok(mut guard) = collected.lock() {
                    *guard = buffer;
                }
            })
            .map_err(|e| AudioError::Ffmpeg(e.to_string()))?;

        let (sender, packets) = mpsc::sync_channel(PACKET_QUEUE);
        let (recycle, recycled) = mpsc::sync_channel(PACKET_QUEUE + 2);
        thread::Builder::new()
            .name("rustlink-ffmpeg-out".into())
            .spawn(move || {
                let mut reader = OggOpusReader::new(stdout);
                loop {
                    let mut packet = recycled.try_recv().unwrap_or_default();
                    match reader.next_packet_into(&mut packet) {
                        Ok(true) => {
                            if sender.send(Ok(packet)).is_err() {
                                break;
                            }
                        }
                        Ok(false) => break,
                        Err(error) => {
                            let _ = sender.send(Err(AudioError::Decode(error.to_string())));
                            break;
                        }
                    }
                }
            })
            .map_err(|e| AudioError::Ffmpeg(e.to_string()))?;

        Ok(FfmpegFrames { packets, recycle, child, errors, input_error, finished: false })
    }

    /// Waits for the child and turns a failure into the message FFmpeg printed.
    fn finish(&mut self) -> Result<(), AudioError> {
        self.finished = true;
        let status = self.child.wait().map_err(|e| AudioError::Ffmpeg(e.to_string()))?;
        if status.success() {
            if let Some(error) = self.input_error.lock().ok().and_then(|mut guard| guard.take()) {
                return Err(AudioError::Io(error));
            }
            return Ok(());
        }
        let message = self
            .errors
            .lock()
            .map(|guard| String::from_utf8_lossy(&guard).trim().to_string())
            .unwrap_or_default();
        Err(AudioError::Ffmpeg(if message.is_empty() {
            format!("ffmpeg exited with {status}")
        } else {
            message
        }))
    }
}

impl Frames for FfmpegFrames {
    fn next_frame(&mut self, packet: &mut Vec<u8>) -> Result<bool, AudioError> {
        if self.finished {
            return Ok(false);
        }
        match self.packets.recv_timeout(STALL) {
            Ok(Ok(mut next)) => {
                std::mem::swap(packet, &mut next);
                // Return the caller's previous buffer without blocking playback.
                // After the small queue warms up, packets reuse its allocations.
                let _ = self.recycle.try_send(next);
                Ok(true)
            }
            Ok(Err(error)) => {
                let _ = self.child.kill();
                let _ = self.child.wait();
                self.finished = true;
                Err(error)
            }
            Err(RecvTimeoutError::Timeout) => {
                // Nothing for ten seconds. The decoder is not coming back, so it
                // is killed and the track is reported as stuck, which is the
                // event a client shows for it.
                let _ = self.child.kill();
                let _ = self.child.wait();
                self.finished = true;
                Err(AudioError::Stuck)
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.finish()?;
                Ok(false)
            }
        }
    }
}

impl Drop for FfmpegFrames {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.finished = true;
        }
    }
}
