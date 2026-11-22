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
/// Do not let shutdown wait forever for a broken stderr reader.
const STDERR_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
/// Bound decode-ahead to 160 ms. Backpressure reaches FFmpeg's pipe, including
/// while paused, so a long track cannot accumulate in an unbounded channel.
const PACKET_QUEUE: usize = 8;

pub struct FfmpegFrames {
    packets: Receiver<Result<Vec<u8>, AudioError>>,
    recycle: SyncSender<Vec<u8>>,
    child: Child,
    stderr: StderrCollector,
    input_error: Arc<Mutex<Option<std::io::Error>>>,
    finished: bool,
    /// How long the next packet may take. It is wider than [`STALL`] for the
    /// first packet of a seek, and `STALL` from then on.
    wait: Duration,
}

/// How long the first packet may take after a seek to `position_ms`. A pipe
/// cannot be seeked, so FFmpeg decodes and throws away everything before the
/// target, and a position two hours in produces nothing for a long while that is
/// not a hang. The allowance assumes the skipped audio goes by at twenty times
/// real time, which a source over a modest connection manages, and is capped so
/// that a decoder that really is stuck is still found.
fn first_packet_wait(position_ms: i64) -> Duration {
    const SKIP_SPEED: f64 = 20.0;
    const LONGEST: Duration = Duration::from_secs(600);
    let skipped = Duration::from_secs_f64(position_ms.max(0) as f64 / 1000.0 / SKIP_SPEED);
    (STALL + skipped).min(LONGEST)
}

/// Owns the stderr reader and its bounded shutdown signal.
///
/// The reader is deliberately not joined after a timeout: a platform pipe can
/// remain blocked even after the child has been reaped, and joining here would
/// turn an error path into an unbounded wait. Dropping the handle detaches the
/// reader; its `Arc` keeps the diagnostic buffer alive until the reader exits.
struct StderrCollector {
    errors: Arc<Mutex<Vec<u8>>>,
    done: Receiver<()>,
    worker: Option<thread::JoinHandle<()>>,
}

impl StderrCollector {
    fn spawn(
        stderr: impl Read + Send + 'static,
        errors: Arc<Mutex<Vec<u8>>>,
    ) -> Result<Self, AudioError> {
        let (done_tx, done) = mpsc::sync_channel(1);
        let collected = Arc::clone(&errors);
        let worker = thread::Builder::new()
            .name("rustlink-ffmpeg-err".into())
            .spawn(move || collect_stderr(stderr, collected, done_tx))
            .map_err(|e| AudioError::Ffmpeg(e.to_string()))?;
        Ok(Self { errors, done, worker: Some(worker) })
    }

    /// Wait for the reader only for a bounded interval, then detach it.
    fn finish(&mut self) {
        if self.worker.is_none() {
            return;
        }
        match self.done.recv_timeout(STDERR_DRAIN_TIMEOUT) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                if let Some(worker) = self.worker.take() {
                    let _ = worker.join();
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                // Dropping JoinHandle detaches the reader. Never join an
                // untrusted pipe reader on the lifecycle/error path.
                drop(self.worker.take());
            }
        }
    }

    fn message(&self) -> String {
        self.errors
            .lock()
            .map(|guard| String::from_utf8_lossy(&guard).trim().to_string())
            .unwrap_or_default()
    }
}

fn collect_stderr<R: Read>(mut stderr: R, errors: Arc<Mutex<Vec<u8>>>, done: SyncSender<()>) {
    let mut chunk = [0u8; 4096];
    while let Ok(read) = stderr.read(&mut chunk) {
        if read == 0 {
            break;
        }
        if let Ok(mut guard) = errors.lock() {
            // Keep the invariant true even if a test or future caller seeded
            // the shared buffer before collection began.
            guard.truncate(STDERR_LIMIT);
            let remaining = STDERR_LIMIT.saturating_sub(guard.len());
            if remaining > 0 {
                guard.extend_from_slice(&chunk[..read.min(remaining)]);
            }
        }
    }
    let _ = done.send(());
}

/// Owns a spawned FFmpeg child until all worker threads have been created.
///
/// The guard is intentionally kept separate from `FfmpegFrames`: setup has
/// several fallible thread-spawn points, and an early return must not leave a
/// process running after its pipes have been detached.
struct StartupChild {
    child: Option<Child>,
}

impl StartupChild {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("startup child already committed")
    }

    fn commit(mut self) -> Child {
        self.child.take().expect("startup child already committed")
    }
}

impl Drop for StartupChild {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The `-ss` value for a source position. Placed after `-i` it counts output
/// time, which the filters have already sped up or slowed down, so a source
/// position is divided by the tempo factor: at speed 2, a seek to 60 s of the
/// source is 30 s of output. The pure engine seeks in source time, and this
/// keeps the two engines on the same sample.
fn output_seek_seconds(position_ms: i64, set: &FilterSet) -> String {
    let factor = set.tempo_factor().max(f64::MIN_POSITIVE);
    format!("{:.3}", position_ms as f64 / 1000.0 / factor)
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
            command.arg("-ss").arg(output_seek_seconds(position_ms, set));
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

        let mut startup =
            StartupChild::new(command.spawn().map_err(|e| AudioError::Ffmpeg(e.to_string()))?);
        let mut stdin = startup.child_mut().stdin.take().expect("stdin was piped");
        let stdout = startup.child_mut().stdout.take().expect("stdout was piped");
        let stderr = startup.child_mut().stderr.take().expect("stderr was piped");
        let errors = Arc::new(Mutex::new(Vec::new()));
        // Start pipe readers before the body writer. If a later thread spawn
        // fails, StartupChild can kill the process and these readers exit on
        // EOF instead of leaving an unobserved pipe blocked.
        let stderr = StderrCollector::spawn(stderr, Arc::clone(&errors))?;

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

        let child = startup.commit();
        Ok(FfmpegFrames {
            packets,
            recycle,
            child,
            stderr,
            input_error,
            finished: false,
            wait: first_packet_wait(position_ms),
        })
    }

    /// Waits for the child and turns a failure into the message FFmpeg printed.
    fn finish(&mut self) -> Result<(), AudioError> {
        self.finish_with_wait(Child::wait)
    }

    fn finish_with_wait(
        &mut self,
        wait: impl FnOnce(&mut Child) -> std::io::Result<std::process::ExitStatus>,
    ) -> Result<(), AudioError> {
        let status = match wait(&mut self.child) {
            Ok(status) => status,
            Err(error) => {
                self.stderr.finish();
                return Err(AudioError::Ffmpeg(error.to_string()));
            }
        };
        self.finished = true;
        self.stderr.finish();
        if status.success() {
            if let Some(error) = self.input_error.lock().ok().and_then(|mut guard| guard.take()) {
                return Err(AudioError::Io(error));
            }
            return Ok(());
        }
        let message = self.stderr.message();
        Err(AudioError::Ffmpeg(if message.is_empty() {
            format!("ffmpeg exited with {status}")
        } else {
            message
        }))
    }

    /// Stop FFmpeg and drain diagnostics without making cleanup unbounded.
    fn terminate(&mut self) {
        let _ = self.child.kill();
        let wait_result = self.child.wait();
        if wait_result.is_ok() {
            self.finished = true;
        }
        self.stderr.finish();
    }
}

impl Frames for FfmpegFrames {
    fn next_frame(&mut self, packet: &mut Vec<u8>) -> Result<bool, AudioError> {
        if self.finished {
            return Ok(false);
        }
        match self.packets.recv_timeout(self.wait) {
            Ok(Ok(mut next)) => {
                // Audio is flowing, so the seek is over and a stall is a stall.
                self.wait = STALL;
                std::mem::swap(packet, &mut next);
                // Return the caller's previous buffer without blocking playback.
                // After the small queue warms up, packets reuse its allocations.
                let _ = self.recycle.try_send(next);
                Ok(true)
            }
            Ok(Err(error)) => {
                self.terminate();
                Err(error)
            }
            Err(RecvTimeoutError::Timeout) => {
                // Nothing for ten seconds. The decoder is not coming back, so it
                // is killed and the track is reported as stuck, which is the
                // event a client shows for it.
                self.terminate();
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
            self.terminate();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn a_long_seek_is_given_time_to_skip_and_a_short_one_is_not() {
        assert_eq!(first_packet_wait(0), STALL, "no seek, no extra time");
        assert_eq!(first_packet_wait(-5), STALL);
        // Two hours in at twenty times real time is six minutes of skipping.
        assert_eq!(first_packet_wait(7_200_000), STALL + Duration::from_secs(360));
        assert!(first_packet_wait(60_000) < STALL + Duration::from_secs(4));
        assert_eq!(first_packet_wait(i64::MAX), Duration::from_secs(600), "and it is capped");
    }

    #[test]
    fn a_seek_is_made_in_source_time_whatever_the_speed() {
        let set = |filters: serde_json::Value| {
            let serde_json::Value::Object(filters) = filters else { panic!("not an object") };
            crate::filters::parse(&filters, 100).unwrap()
        };
        let plain = set(serde_json::json!({}));
        assert_eq!(output_seek_seconds(60_000, &plain), "60.000");
        // At twice the speed one second of output holds two of source, so the
        // 60th second of the source is the 30th of the output. Passing 60 as the
        // old code did starts at the 120th second of the source.
        let fast = set(serde_json::json!({"timescale": {"speed": 2.0}}));
        assert_eq!(output_seek_seconds(60_000, &fast), "30.000");
        let slow = set(serde_json::json!({"timescale": {"speed": 0.5}}));
        assert_eq!(output_seek_seconds(10_000, &slow), "20.000");
        // Pitch alone leaves the length alone; rate scales it like speed does.
        let pitched = set(serde_json::json!({"timescale": {"pitch": 1.5}}));
        assert_eq!(output_seek_seconds(60_000, &pitched), "60.000");
        let rated = set(serde_json::json!({"timescale": {"rate": 1.5}}));
        assert_eq!(output_seek_seconds(60_000, &rated), "40.000");
    }

    fn marker_path(label: &str) -> PathBuf {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("rustlink-{label}-{}-{nonce}.marker", std::process::id()))
    }

    #[cfg(windows)]
    fn gated_marker(started: &Path, release: &Path, survived: &Path) -> Child {
        let started = started.to_string_lossy().replace('\'', "''");
        let release = release.to_string_lossy().replace('\'', "''");
        let survived = survived.to_string_lossy().replace('\'', "''");
        Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!(
                    "$deadline=[DateTime]::UtcNow.AddSeconds(5); [IO.File]::WriteAllText('{started}','started'); while (-not [IO.File]::Exists('{release}') -and [DateTime]::UtcNow -lt $deadline) {{ Start-Sleep -Milliseconds 10 }}; if ([IO.File]::Exists('{release}')) {{ [IO.File]::WriteAllText('{survived}','survived') }}; exit 23"
                ),
            ])
            .spawn()
            .unwrap()
    }

    #[cfg(not(windows))]
    fn gated_marker(started: &Path, release: &Path, survived: &Path) -> Child {
        fn quote(path: &Path) -> String {
            format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
        }
        Command::new("sh")
            .args([
                "-c",
                &format!(
                    "printf started > {}; i=0; while [ ! -e {} ] && [ \"$i\" -lt 500 ]; do sleep 0.01; i=$((i + 1)); done; if [ -e {} ]; then printf survived > {}; fi; exit 23",
                    quote(started),
                    quote(release),
                    quote(release),
                    quote(survived)
                ),
            ])
            .spawn()
            .unwrap()
    }

    #[cfg(windows)]
    fn gated_stderr_child(started: &Path, release: &Path, marker: &str) -> Child {
        let started = started.to_string_lossy().replace('\'', "''");
        let release = release.to_string_lossy().replace('\'', "''");
        let marker = marker.replace('\'', "''");
        Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!(
                    "$deadline=[DateTime]::UtcNow.AddSeconds(5); [IO.File]::WriteAllText('{started}','started'); while (-not [IO.File]::Exists('{release}') -and [DateTime]::UtcNow -lt $deadline) {{ Start-Sleep -Milliseconds 10 }}; [Console]::Error.WriteLine('{marker}'); exit 23"
                ),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    #[cfg(not(windows))]
    fn gated_stderr_child(started: &Path, release: &Path, marker: &str) -> Child {
        fn quote(value: &str) -> String {
            format!("'{}'", value.replace('\'', "'\\''"))
        }
        Command::new("sh")
            .args([
                "-c",
                &format!(
                    "printf started > {}; i=0; while [ ! -e {} ] && [ \"$i\" -lt 500 ]; do sleep 0.01; i=$((i + 1)); done; printf '%s\\n' {} >&2; exit 23",
                    quote(&started.to_string_lossy()),
                    quote(&release.to_string_lossy()),
                    quote(marker)
                ),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn wait_for(path: &Path) {
        assert!(appears_within(path, Duration::from_secs(5)), "helper process did not start");
    }

    fn appears_within(path: &Path, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !path.exists() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    #[cfg(windows)]
    fn failed_child() -> Child {
        Command::new("cmd.exe")
            .args(["/d", "/c", "exit /b 23"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    #[cfg(not(windows))]
    fn failed_child() -> Child {
        Command::new("sh")
            .args(["-c", "exit 23"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn empty_stderr_collector(errors: Arc<Mutex<Vec<u8>>>) -> StderrCollector {
        let (done_tx, done) = mpsc::sync_channel(1);
        done_tx.send(()).unwrap();
        StderrCollector { errors, done, worker: None }
    }

    fn frames_for_child(child: Child, stderr: StderrCollector) -> FfmpegFrames {
        let (packet_sender, packets) = mpsc::sync_channel(1);
        drop(packet_sender);
        let (recycle, recycled) = mpsc::sync_channel(1);
        drop(recycled);
        FfmpegFrames {
            packets,
            recycle,
            child,
            stderr,
            input_error: Arc::new(Mutex::new(None)),
            finished: false,
            wait: STALL,
        }
    }

    #[test]
    fn finish_remains_unfinished_when_wait_fails() {
        let errors = Arc::new(Mutex::new(Vec::new()));
        let mut frames = frames_for_child(failed_child(), empty_stderr_collector(errors));
        let error = frames
            .finish_with_wait(|_| Err(std::io::Error::other("injected wait failure")))
            .expect_err("an injected wait failure must be returned");
        let finished = frames.finished;
        // Preserve the observed value but allow the red case to clean up the
        // real helper instead of suppressing Drop a second time.
        frames.finished = false;
        drop(frames);

        assert!(matches!(error, AudioError::Ffmpeg(_)));
        assert!(error.to_string().contains("injected wait failure"));
        assert!(!finished, "a failed wait suppressed Drop cleanup");
    }

    #[test]
    fn finish_waits_for_a_ready_stderr_collector() {
        const MARKER: &str = "rustlink-ffmpeg-delayed-stderr";

        let child_started = marker_path("ffmpeg-error-started");
        let child_release = marker_path("ffmpeg-error-release");
        let mut child = gated_stderr_child(&child_started, &child_release, MARKER);
        let mut stderr = child.stderr.take().expect("stderr was piped");
        wait_for(&child_started);

        let errors = Arc::new(Mutex::new(Vec::new()));
        let collected = Arc::clone(&errors);
        let (collector_read_tx, collector_read_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (done_tx, done) = mpsc::sync_channel(1);
        let collector = thread::spawn(move || {
            let mut buffer = Vec::new();
            stderr.read_to_end(&mut buffer).unwrap();
            collector_read_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            *collected.lock().unwrap() = buffer;
            done_tx.send(()).unwrap();
        });

        let stderr = StderrCollector { errors, done, worker: Some(collector) };
        let frames = frames_for_child(child, stderr);
        let (finisher_ready_tx, finisher_ready_rx) = mpsc::sync_channel(1);
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let finisher = thread::spawn(move || {
            let mut frames = frames;
            finisher_ready_tx.send(()).unwrap();
            result_tx.send(frames.finish()).unwrap();
        });
        finisher_ready_rx.recv().unwrap();
        std::fs::write(&child_release, b"release").unwrap();
        collector_read_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let result = match result_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(result) => {
                release_tx.send(()).unwrap();
                result
            }
            Err(RecvTimeoutError::Timeout) => {
                release_tx.send(()).unwrap();
                result_rx.recv_timeout(Duration::from_secs(5)).unwrap()
            }
            Err(RecvTimeoutError::Disconnected) => panic!("finish worker disconnected"),
        };
        finisher.join().unwrap();

        let _ = std::fs::remove_file(child_started);
        let _ = std::fs::remove_file(child_release);

        let error = result.unwrap_err();
        assert!(error.to_string().contains(MARKER), "lost FFmpeg stderr: {error}");
    }

    struct ChunkedReader {
        data: Vec<u8>,
        offset: usize,
        chunk: usize,
    }

    impl Read for ChunkedReader {
        fn read(&mut self, target: &mut [u8]) -> std::io::Result<usize> {
            if self.offset == self.data.len() {
                return Ok(0);
            }
            let count = self.chunk.min(target.len()).min(self.data.len() - self.offset);
            target[..count].copy_from_slice(&self.data[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        }
    }

    #[test]
    fn stderr_collector_clamps_each_chunk_to_the_limit() {
        let data: Vec<u8> = (0..(STDERR_LIMIT + 3001)).map(|index| (index % 251) as u8).collect();
        let expected = data[..STDERR_LIMIT].to_vec();
        let errors = Arc::new(Mutex::new(Vec::new()));
        let (done_tx, done_rx) = mpsc::sync_channel(1);

        collect_stderr(
            ChunkedReader { data, offset: 0, chunk: 3001 },
            Arc::clone(&errors),
            done_tx,
        );
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let actual = errors.lock().unwrap().clone();
        assert_eq!(actual.len(), STDERR_LIMIT);
        assert_eq!(actual, expected);
    }

    #[test]
    fn stderr_collector_wait_is_bounded() {
        let errors = Arc::new(Mutex::new(Vec::new()));
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (exited_tx, exited_rx) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            release_rx.recv().unwrap();
            exited_tx.send(()).unwrap();
            drop(done_tx);
        });
        let mut collector = StderrCollector { errors, done: done_rx, worker: Some(worker) };

        let started = Instant::now();
        collector.finish();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "stderr cleanup waited beyond its bounded timeout"
        );

        release_tx.send(()).unwrap();
        exited_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn startup_guard_actively_kills_instead_of_only_waiting() {
        let started = marker_path("ffmpeg-started");
        let release = marker_path("ffmpeg-release");
        let survived = marker_path("ffmpeg-survived");
        let guard = StartupChild::new(gated_marker(&started, &release, &survived));
        wait_for(&started);

        let (drop_done_tx, drop_done_rx) = mpsc::sync_channel(1);
        let dropper = thread::spawn(move || {
            drop(guard);
            let _ = drop_done_tx.send(());
        });
        let completed_before_release = match drop_done_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(()) => true,
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => panic!("guard drop worker disconnected"),
        };
        std::fs::write(&release, b"release").unwrap();
        let child_survived = appears_within(&survived, Duration::from_secs(1));
        if !completed_before_release {
            drop_done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        dropper.join().unwrap();

        let _ = std::fs::remove_file(started);
        let _ = std::fs::remove_file(release);
        let _ = std::fs::remove_file(survived);
        assert!(completed_before_release, "startup guard waited instead of killing the child");
        assert!(!child_survived, "child survived failed FFmpeg setup");
    }
}
