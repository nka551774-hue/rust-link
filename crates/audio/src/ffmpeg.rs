//! Child-process FFmpeg engine streaming piped input to Ogg Opus packets.

use std::io::{self, Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::engine::{Body, Frames};
use crate::error::AudioError;
use crate::filters::FilterSet;
use crate::ogg::OggOpusReader;

const STALL: Duration = Duration::from_secs(10);
const STDERR_LIMIT: usize = 32 << 10;
const STDERR_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
const PACKET_QUEUE: usize = 8;

pub struct FfmpegFrames {
    packets: Receiver<Result<Vec<u8>, AudioError>>,
    recycle: SyncSender<Vec<u8>>,
    child: Child,
    stderr: StderrCollector,
    input_error: Arc<Mutex<Option<io::Error>>>,
    finished: bool,
    wait: Duration,
}

fn first_packet_wait(position_ms: i64) -> Duration {
    const SKIP_SPEED: f64 = 20.0;
    const LONGEST: Duration = Duration::from_secs(600);
    let skipped = Duration::from_secs_f64(position_ms.max(0) as f64 / 1000.0 / SKIP_SPEED);
    (STALL + skipped).min(LONGEST)
}

fn spawn_io(
    name: &str,
    f: impl FnOnce() + Send + 'static,
) -> Result<thread::JoinHandle<()>, AudioError> {
    thread::Builder::new().name(name.into()).spawn(f).map_err(|e| AudioError::Ffmpeg(e.to_string()))
}

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
        let worker = spawn_io("rustlink-ffmpeg-err", move || {
            collect_stderr(stderr, collected, done_tx);
        })?;
        Ok(Self { errors, done, worker: Some(worker) })
    }

    /// Waits briefly for stderr collection to finish, detaching the reader on timeout.
    fn finish(&mut self) {
        let Some(worker) = self.worker.take() else { return };
        if !matches!(self.done.recv_timeout(STDERR_DRAIN_TIMEOUT), Err(RecvTimeoutError::Timeout)) {
            let _ = worker.join();
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
    let mut full = false;
    loop {
        let read = match stderr.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        if !full {
            if let Ok(mut guard) = errors.lock() {
                guard.truncate(STDERR_LIMIT);
                let remaining = STDERR_LIMIT.saturating_sub(guard.len());
                if remaining > 0 {
                    guard.extend_from_slice(&chunk[..read.min(remaining)]);
                }
                full = guard.len() >= STDERR_LIMIT;
            }
        }
    }
    let _ = done.send(());
}

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

fn output_seek_seconds(position_ms: i64, set: &FilterSet) -> String {
    const MAX_FFMPEG_SECONDS: f64 = 9_000_000_000_000.0;
    let factor = set.tempo_factor().max(f64::MIN_POSITIVE);
    let seconds = (position_ms.max(0) as f64 / 1000.0 / factor).min(MAX_FFMPEG_SECONDS);
    format!("{seconds:.3}")
}

impl FfmpegFrames {
    pub fn start(
        program: &str,
        mut body: Body,
        position_ms: i64,
        set: &FilterSet,
        bitrate: i32,
        complexity: i32,
    ) -> Result<Self, AudioError> {
        let mut command = Command::new(program);
        command.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-threads",
            "1",
            "-filter_threads",
            "1",
            "-probesize",
            "1048576",
            "-analyzeduration",
            "3000000",
            "-i",
            "pipe:0",
        ]);
        if position_ms > 0 {
            command.args(["-ss", &output_seek_seconds(position_ms, set)]);
        }
        let (bitrate_arg, complexity_arg, graph) =
            (bitrate.to_string(), complexity.to_string(), set.render_ffmpeg());
        command
            .args([
                "-map",
                "0:a:0",
                "-vn",
                "-sn",
                "-dn",
                "-af",
                &graph,
                "-ac",
                "2",
                "-ar",
                "48000",
                "-c:a",
                "libopus",
                "-b:a",
                &bitrate_arg,
                "-application",
                "audio",
                "-compression_level",
                &complexity_arg,
                "-vbr",
                "on",
                "-fec",
                "1",
                "-packet_loss",
                "10",
                "-frame_duration",
                "20",
                "-f",
                "opus",
                "-page_duration",
                "20000",
                "-flush_packets",
                "1",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut startup =
            StartupChild::new(command.spawn().map_err(|e| AudioError::Ffmpeg(e.to_string()))?);
        let child_ref = startup.child_mut();
        let mut stdin = child_ref.stdin.take().expect("stdin was piped");
        let stdout = child_ref.stdout.take().expect("stdout was piped");
        let stderr = child_ref.stderr.take().expect("stderr was piped");
        let stderr = StderrCollector::spawn(stderr, Arc::new(Mutex::new(Vec::new())))?;

        let (sender, packets) = mpsc::sync_channel(PACKET_QUEUE);
        let (recycle, recycled) = mpsc::sync_channel(PACKET_QUEUE + 2);
        spawn_io("rustlink-ffmpeg-out", move || {
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
        })?;

        let input_error = Arc::new(Mutex::new(None));
        let input_failure = Arc::clone(&input_error);
        spawn_io("rustlink-ffmpeg-in", move || {
            let mut buf = [0u8; 16 * 1024];
            loop {
                match body.read(&mut buf) {
                    Ok(0) => break,
                    Ok(read) => {
                        if stdin.write_all(&buf[..read]).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        if let Ok(mut guard) = input_failure.lock() {
                            *guard = Some(error);
                        }
                        break;
                    }
                }
            }
        })?;

        Ok(Self {
            packets,
            recycle,
            child: startup.commit(),
            stderr,
            input_error,
            finished: false,
            wait: first_packet_wait(position_ms),
        })
    }

    fn take_input_error(&self) -> Option<io::Error> {
        self.input_error.lock().ok().and_then(|mut guard| guard.take())
    }

    fn finish(&mut self) -> Result<(), AudioError> {
        self.finish_bounded(STALL)
    }

    fn finish_bounded(&mut self, timeout: Duration) -> Result<(), AudioError> {
        self.finish_with_wait(|child| wait_child_bounded(child, timeout))
    }

    fn finish_with_wait(
        &mut self,
        wait: impl FnOnce(&mut Child) -> io::Result<ExitStatus>,
    ) -> Result<(), AudioError> {
        let status = match wait(&mut self.child) {
            Ok(status) => status,
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                self.terminate();
                return Err(AudioError::Stuck);
            }
            Err(error) => {
                self.stderr.finish();
                return Err(AudioError::Ffmpeg(error.to_string()));
            }
        };
        self.finished = true;
        self.stderr.finish();
        if let Some(error) = self.take_input_error() {
            return Err(AudioError::Io(error));
        }
        if status.success() {
            return Ok(());
        }
        let message = self.stderr.message();
        Err(AudioError::Ffmpeg(if message.is_empty() {
            format!("ffmpeg exited with {status}")
        } else {
            message
        }))
    }

    fn terminate(&mut self) {
        let _ = self.child.kill();
        if self.child.wait().is_ok() {
            self.finished = true;
        }
        self.stderr.finish();
    }
}

fn wait_child_bounded(child: &mut Child, timeout: Duration) -> io::Result<ExitStatus> {
    let started = Instant::now();
    let mut poll = Duration::from_millis(1);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        let elapsed = started.elapsed();
        if elapsed >= timeout {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "ffmpeg did not exit within timeout",
            ));
        }
        thread::sleep(poll.min(timeout - elapsed));
        poll = (poll * 2).min(Duration::from_millis(20));
    }
}

impl Frames for FfmpegFrames {
    fn next_frame(&mut self, packet: &mut Vec<u8>) -> Result<bool, AudioError> {
        if self.finished {
            return Ok(false);
        }
        match self.packets.recv_timeout(self.wait) {
            Ok(Ok(mut next)) => {
                self.wait = STALL;
                std::mem::swap(packet, &mut next);
                let _ = self.recycle.try_send(next);
                Ok(true)
            }
            Ok(Err(error)) => {
                self.terminate();
                if let Some(io_error) = self.take_input_error() {
                    return Err(AudioError::Io(io_error));
                }
                let stderr = self.stderr.message();
                Err(if stderr.is_empty() {
                    error
                } else {
                    AudioError::Ffmpeg(format!("{error} ({stderr})"))
                })
            }
            Err(RecvTimeoutError::Timeout) => {
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
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn a_long_seek_is_given_time_to_skip_and_a_short_one_is_not() {
        assert_eq!(first_packet_wait(0), STALL);
        assert_eq!(first_packet_wait(-5), STALL);
        assert_eq!(first_packet_wait(7_200_000), STALL + Duration::from_secs(360));
        assert!(first_packet_wait(60_000) < STALL + Duration::from_secs(4));
        assert_eq!(first_packet_wait(i64::MAX), Duration::from_secs(600));
    }

    #[test]
    fn a_seek_is_made_in_source_time_whatever_the_speed() {
        let set = |filters: serde_json::Value| {
            let serde_json::Value::Object(filters) = filters else { panic!("not an object") };
            crate::filters::parse(&filters, 100).unwrap()
        };
        assert_eq!(output_seek_seconds(60_000, &set(serde_json::json!({}))), "60.000");
        assert_eq!(
            output_seek_seconds(60_000, &set(serde_json::json!({"timescale": {"speed": 2.0}}))),
            "30.000"
        );
        assert_eq!(
            output_seek_seconds(10_000, &set(serde_json::json!({"timescale": {"speed": 0.5}}))),
            "20.000"
        );
        assert_eq!(
            output_seek_seconds(60_000, &set(serde_json::json!({"timescale": {"pitch": 1.5}}))),
            "60.000"
        );
        assert_eq!(
            output_seek_seconds(60_000, &set(serde_json::json!({"timescale": {"rate": 1.5}}))),
            "40.000"
        );
    }

    fn marker_path(label: &str) -> PathBuf {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("rustlink-{label}-{}-{nonce}.marker", std::process::id()))
    }

    #[cfg(windows)]
    fn ps_escape(s: &str) -> String {
        s.replace('\'', "''")
    }

    #[cfg(windows)]
    fn spawn_gate(started: &Path, release: &Path, tail: &str, pipe_stderr: bool) -> Child {
        let (st, rl) =
            (ps_escape(&started.to_string_lossy()), ps_escape(&release.to_string_lossy()));
        Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!(
                    "$d=[DateTime]::UtcNow.AddSeconds(5); [IO.File]::WriteAllText('{st}','started'); while (-not [IO.File]::Exists('{rl}') -and [DateTime]::UtcNow -lt $d) {{ Start-Sleep -Milliseconds 10 }}; {tail}; exit 23"
                ),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(if pipe_stderr { Stdio::piped() } else { Stdio::null() })
            .spawn()
            .unwrap()
    }

    #[cfg(windows)]
    fn gated_marker(started: &Path, release: &Path, survived: &Path) -> Child {
        let (rl, sv) =
            (ps_escape(&release.to_string_lossy()), ps_escape(&survived.to_string_lossy()));
        spawn_gate(
            started,
            release,
            &format!(
                "if ([IO.File]::Exists('{rl}')) {{ [IO.File]::WriteAllText('{sv}','survived') }}"
            ),
            false,
        )
    }

    #[cfg(windows)]
    fn gated_stderr_child(started: &Path, release: &Path, marker: &str) -> Child {
        spawn_gate(
            started,
            release,
            &format!("[Console]::Error.WriteLine('{}')", ps_escape(marker)),
            true,
        )
    }

    #[cfg(not(windows))]
    fn sh_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }

    #[cfg(not(windows))]
    fn spawn_gate(started: &Path, release: &Path, tail: &str, pipe_stderr: bool) -> Child {
        let (st, rl) = (sh_quote(&started.to_string_lossy()), sh_quote(&release.to_string_lossy()));
        Command::new("sh")
            .args([
                "-c",
                &format!(
                    "printf started > {st}; i=0; while [ ! -e {rl} ] && [ \"$i\" -lt 500 ]; do sleep 0.01; i=$((i + 1)); done; {tail}; exit 23"
                ),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(if pipe_stderr { Stdio::piped() } else { Stdio::null() })
            .spawn()
            .unwrap()
    }

    #[cfg(not(windows))]
    fn gated_marker(started: &Path, release: &Path, survived: &Path) -> Child {
        let (rl, sv) =
            (sh_quote(&release.to_string_lossy()), sh_quote(&survived.to_string_lossy()));
        spawn_gate(
            started,
            release,
            &format!("if [ -e {rl} ]; then printf survived > {sv}; fi"),
            false,
        )
    }

    #[cfg(not(windows))]
    fn gated_stderr_child(started: &Path, release: &Path, marker: &str) -> Child {
        spawn_gate(started, release, &format!("printf '%s\\n' {} >&2", sh_quote(marker)), true)
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
            thread::sleep(Duration::from_millis(10));
        }
        true
    }

    fn failed_child() -> Child {
        #[cfg(windows)]
        let (prog, args) = ("cmd.exe", &["/d", "/c", "exit /b 23"][..]);
        #[cfg(not(windows))]
        let (prog, args) = ("sh", &["-c", "exit 23"][..]);
        Command::new(prog)
            .args(args)
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
        let (_packet_sender, packets) = mpsc::sync_channel(1);
        let (recycle, _recycled) = mpsc::sync_channel(1);
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
        let mut frames = frames_for_child(
            failed_child(),
            empty_stderr_collector(Arc::new(Mutex::new(Vec::new()))),
        );
        let error = frames
            .finish_with_wait(|_| Err(io::Error::other("injected wait failure")))
            .expect_err("an injected wait failure must be returned");
        let finished = frames.finished;
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

        let frames =
            frames_for_child(child, StderrCollector { errors, done, worker: Some(collector) });
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
        fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
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

        assert_eq!(*errors.lock().unwrap(), expected);
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

        for p in [started, release, survived] {
            let _ = std::fs::remove_file(p);
        }
        assert!(completed_before_release, "startup guard waited instead of killing the child");
        assert!(!child_survived, "child survived failed FFmpeg setup");
    }

    #[test]
    fn finish_kills_hung_child_and_reports_stuck_when_bounded_wait_expires() {
        let started = marker_path("ffmpeg-finish-started");
        let release = marker_path("ffmpeg-finish-release");
        let survived = marker_path("ffmpeg-finish-survived");
        let child = gated_marker(&started, &release, &survived);
        wait_for(&started);

        let mut frames =
            frames_for_child(child, empty_stderr_collector(Arc::new(Mutex::new(Vec::new()))));
        let begin = Instant::now();
        let err = frames
            .finish_bounded(Duration::from_millis(40))
            .expect_err("a hung child must time out instead of blocking forever");
        assert!(begin.elapsed() < Duration::from_secs(2));
        assert!(matches!(err, AudioError::Stuck));
        assert!(frames.finished);

        std::fs::write(&release, b"release").unwrap();
        let child_survived = appears_within(&survived, Duration::from_millis(100));
        for p in [started, release, survived] {
            let _ = std::fs::remove_file(p);
        }
        assert!(!child_survived, "hung child survived finish timeout");
    }
}
