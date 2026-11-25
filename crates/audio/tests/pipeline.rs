//! End-to-end checks of the in-process and FFmpeg pipelines over real media bytes.
#![cfg(feature = "native-opus")]

use std::f64::consts::TAU;
use std::io::{self, Cursor, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use rustlink_audio::{
    ffmpeg::FfmpegFrames, AudioError, Body, Engine, FilterSet, Frames, Preference,
};
use rustlink_protocol::{Track, TrackInfo};
use serde_json::json;

const ENGINE: Engine = Engine { ffmpeg: None, in_process: true, bitrate: 128_000, complexity: 10 };

fn body(bytes: Vec<u8>) -> Body {
    Body::new(Box::new(Cursor::new(bytes)))
}

fn ffmpeg_bin() -> String {
    std::env::var("RUSTLINK_TEST_FFMPEG").expect("RUSTLINK_TEST_FFMPEG is required")
}

fn wav(rate: u32, channels: u16, seconds: f64, frequency: f64) -> Vec<u8> {
    let frames = (rate as f64 * seconds) as u32;
    let block_align = channels * 2;
    let data_len = frames * u32::from(block_align);
    let mut out = Vec::with_capacity(data_len as usize + 44);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0");
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * u32::from(block_align)).to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for frame in 0..frames {
        let sample = ((TAU * frequency * frame as f64 / rate as f64).sin() * 12_000.0) as i16;
        for _ in 0..channels {
            out.extend_from_slice(&sample.to_le_bytes());
        }
    }
    out
}

fn track() -> Track {
    Track::new(
        "test".into(),
        TrackInfo {
            identifier: "test".into(),
            title: "test".into(),
            source_name: "http".into(),
            is_seekable: true,
            ..Default::default()
        },
    )
}

fn drain_frames(frames: &mut dyn Frames) -> Vec<Vec<u8>> {
    let (mut out, mut frame) = (Vec::new(), Vec::new());
    while frames.next_frame(&mut frame).expect("no frame should fail") {
        out.push(frame.clone());
    }
    out
}

fn collect(bytes: Vec<u8>, filters: serde_json::Value, position_ms: i64) -> Vec<Vec<u8>> {
    let serde_json::Value::Object(object) = filters else { panic!("not an object") };
    let mut frames = ENGINE
        .start(&track(), body(bytes), position_ms, 100, &object, Preference::Auto)
        .expect("the native pipeline should accept this audio");
    drain_frames(&mut *frames)
}

fn rms_i16(samples: &[i16]) -> f64 {
    (samples.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / samples.len() as f64).sqrt()
}

fn decode_packets(packets: Vec<Vec<u8>>) -> Vec<i16> {
    let mut decoder = rustlink_audio::opus_decode::Decoder::new().unwrap();
    let (mut samples, mut decoded) = (Vec::new(), Vec::new());
    for packet in packets {
        decoder.decode(&packet, &mut decoded).unwrap();
        samples.extend_from_slice(&decoded);
    }
    samples
}

fn decode_stream(bytes: Vec<u8>) -> Vec<f32> {
    let mut decoder = rustlink_audio::decode::Decoder::new(body(bytes)).unwrap();
    let mut samples = Vec::new();
    while decoder.decode(&mut samples).unwrap() {}
    samples
}

fn write_ogg_page(seq: u32, flags: u8, granule: u64, packet: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&[b'O', b'g', b'g', b'S', 0, flags]);
    out.extend_from_slice(&granule.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    let segments = packet.len() / 255 + 1;
    out.push(segments as u8);
    out.extend(std::iter::repeat_n(255u8, segments - 1));
    out.push((packet.len() % 255) as u8);
    out.extend_from_slice(packet);
}

fn write_ogg_headers(pre_skip: u16, output_gain: i16, out: &mut Vec<u8>) {
    let mut head = b"OpusHead\x01\x02".to_vec();
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&output_gain.to_le_bytes());
    head.push(0);
    write_ogg_page(0, 2, 0, &head, out);
    write_ogg_page(1, 0, 0, b"OpusTags\0\0\0\0\0\0\0\0", out);
}

#[test]
fn a_one_second_wav_includes_the_encoder_delayed_tail() {
    let frames = collect(wav(48_000, 2, 1.0, 440.0), json!({}), 0);
    assert_eq!(frames.len(), 51, "fifty frames plus the encoder's delayed tail");
    assert!(frames.iter().all(|f| !f.is_empty() && f.len() < 1500));
}

#[test]
fn a_source_at_another_rate_is_resampled_to_the_same_length() {
    for rate in [8_000, 22_050, 44_100, 96_000] {
        assert_eq!(collect(wav(rate, 2, 1.0, 440.0), json!({}), 0).len(), 51, "rate={rate}");
    }
}

#[test]
fn a_mono_source_is_folded_to_stereo() {
    assert_eq!(collect(wav(48_000, 1, 0.5, 440.0), json!({}), 0).len(), 26);
}

#[test]
fn filters_do_not_change_how_many_frames_come_out() {
    let frames = collect(
        wav(44_100, 2, 1.0, 440.0),
        json!({
            "volume": 0.5,
            "equalizer": [{"band": 8, "gain": 0.25}],
            "tremolo": {"frequency": 5, "depth": 0.5},
            "channelMix": {"leftToRight": 0.1}
        }),
        0,
    );
    assert_eq!(frames.len(), 51);
}

#[test]
fn a_timescale_changes_the_length_in_the_right_direction() {
    let plain = collect(wav(48_000, 2, 1.0, 440.0), json!({}), 0).len();
    let faster = collect(wav(48_000, 2, 1.0, 440.0), json!({"timescale": {"speed": 2.0}}), 0).len();
    assert!(faster < plain && faster > plain / 3, "{faster} vs {plain}");
}

#[test]
fn a_seek_starts_later_and_produces_less() {
    let whole = collect(wav(48_000, 2, 1.0, 440.0), json!({}), 0).len();
    let half = collect(wav(48_000, 2, 1.0, 440.0), json!({}), 500).len();
    assert!(half < whole && (24..=26).contains(&half), "got {half} vs {whole}");
}

#[test]
fn a_large_seek_cannot_wrap_back_to_the_start() {
    let wrapping_position = ((u64::MAX as u128 + 1).div_ceil(48_000)) as i64;
    assert!(collect(wav(48_000, 2, 1.0, 440.0), json!({}), wrapping_position).is_empty());
}

#[test]
fn an_unknown_container_is_declined_so_the_caller_can_use_ffmpeg() {
    let error = ENGINE
        .start(
            &track(),
            body(b"this is not audio".to_vec()),
            0,
            100,
            &Default::default(),
            Preference::Auto,
        )
        .err()
        .expect("an unknown container should not play");
    assert!(error.needs_ffmpeg(), "the caller should be told to retry with ffmpeg: {error}");
}

#[test]
fn a_zero_sample_rate_is_declined_instead_of_expanded_without_bound() {
    let mut bytes = wav(48_000, 2, 0.1, 440.0);
    bytes[24..32].fill(0);
    let error = ENGINE
        .start(&track(), body(bytes), 0, 100, &Default::default(), Preference::Auto)
        .err()
        .expect("a stream without a sample rate should not play natively");
    assert!(error.needs_ffmpeg(), "{error}");
}

fn ogg_opus(pre_skip: u16, output_gain: i16) -> Vec<u8> {
    ogg_opus_with_end(pre_skip, output_gain, None)
}

fn ogg_opus_with_end(pre_skip: u16, output_gain: i16, end: Option<u64>) -> Vec<u8> {
    let mut out = Vec::new();
    write_ogg_headers(pre_skip, output_gain, &mut out);
    let mut encoder = rustlink_audio::Encoder::new(128_000, 10).unwrap();
    let pcm: Vec<i16> = (0..1920)
        .map(|i| (((i / 2) as f64 * 440.0 * TAU / 48_000.0).sin() * 12_000.0) as i16)
        .collect();
    for seq in 2..4 {
        let eos_end = (seq == 3).then_some(end).flatten();
        let granule = eos_end.unwrap_or(u64::from(seq - 1) * 960);
        write_ogg_page(
            seq,
            if eos_end.is_some() { 4 } else { 0 },
            granule,
            encoder.encode(&pcm).unwrap(),
            &mut out,
        );
    }
    out
}

#[test]
fn ogg_retains_the_first_audio_packet_and_applies_pre_skip_once() {
    for skip in [0u16, 312, 1272] {
        let samples = decode_stream(ogg_opus(skip, 0));
        assert_eq!(samples.len(), (1920 - usize::from(skip)) * 2);
        assert!(samples.iter().any(|sample| sample.abs() > 0.1));
    }
}

#[test]
fn ogg_output_gain_is_applied_to_the_decoded_samples() {
    let plain = decode_stream(ogg_opus(312, 0));
    let louder = decode_stream(ogg_opus(312, 6 * 256));
    let gain = 10.0f32.powf(6.0 / 20.0);
    assert_eq!(plain.len(), louder.len());
    for (a, b) in plain.into_iter().zip(louder) {
        assert!((b - a * gain).abs() < 1e-6);
    }
}

#[test]
fn ogg_end_granule_trims_padding_after_pre_skip() {
    for (skip, end) in [(312u16, 1500u64), (1272, 1500)] {
        assert_eq!(
            decode_stream(ogg_opus_with_end(skip, 0, Some(end))).len(),
            (end as usize - usize::from(skip)) * 2
        );
    }
}

#[test]
fn the_last_audible_samples_survive_opus_lookahead() {
    let mut source = wav(48_000, 2, 0.02, 1000.0);
    source[44..44 + 760 * 4].fill(0);
    let decoded = decode_packets(collect(source, json!({}), 0));
    assert!(
        rms_i16(&decoded[(312 + 832) * 2..(312 + 960) * 2]) > 3000.0,
        "the final source tone must not be cut off"
    );
}

#[test]
fn equalizer_frequency_is_independent_of_source_rate() {
    for rate in [8_000, 44_100, 48_000] {
        let rms = |filters| {
            let decoded = decode_packets(collect(wav(rate, 2, 1.0, 1000.0), filters, 0));
            rms_i16(&decoded[12_000 * 2..36_000 * 2])
        };
        let ratio = rms(json!({"equalizer": [{"band": 8, "gain": 0.25}]})) / rms(json!({}));
        assert!((1.8..2.2).contains(&ratio), "1 kHz +6 dB at {rate} Hz gave gain {ratio}");
    }
}

#[test]
fn a_unity_conversion_preserves_even_a_very_short_source() {
    assert_eq!(collect(wav(48_000, 2, 4.0 / 48_000.0, 440.0), json!({}), 0).len(), 1);
}

#[test]
fn rate_conversion_preserves_even_a_very_short_source() {
    for rate in [8_000, 44_100, 96_000] {
        assert_eq!(collect(wav(rate, 2, 4.0 / rate as f64, 440.0), json!({}), 0).len(), 1);
    }
}

#[test]
fn very_slow_playback_applies_backpressure_before_decoding_more_source() {
    struct Counted {
        inner: Cursor<Vec<u8>>,
        read: Arc<AtomicUsize>,
    }
    impl Read for Counted {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(bytes)?;
            self.read.fetch_add(n, Ordering::Relaxed);
            Ok(n)
        }
    }
    let read = Arc::new(AtomicUsize::new(0));
    let counted = Body::new(Box::new(Counted {
        inner: Cursor::new(wav(8_000, 2, 60.0, 440.0)),
        read: Arc::clone(&read),
    }));
    let engine = Engine { ffmpeg: None, in_process: true, bitrate: 64_000, complexity: 0 };
    let filters = json!({"timescale": {"speed": 0.05, "pitch": 0.05, "rate": 0.05}});
    let mut frames = engine
        .start(&track(), counted, 0, 100, filters.as_object().unwrap(), Preference::Auto)
        .unwrap();
    let mut packet = Vec::new();
    for _ in 0..4000 {
        assert!(frames.next_frame(&mut packet).unwrap());
    }
    let total_read = read.load(Ordering::Relaxed);
    assert!(total_read <= 128 * 1024, "read {total_read} bytes ahead");
}

#[test]
fn pitch_preserves_duration_and_rate_changes_duration() {
    let pitched = collect(wav(48_000, 2, 1.0, 440.0), json!({"timescale": {"pitch": 2.0}}), 0);
    assert!((48..=52).contains(&pitched.len()), "pitch packets={}", pitched.len());
    let faster = collect(wav(48_000, 2, 1.0, 440.0), json!({"timescale": {"rate": 2.0}}), 0);
    assert_eq!(faster.len(), 26);
}

#[test]
fn extreme_combined_timescale_requests_the_bounded_fallback() {
    let filters = json!({"timescale": {"speed": 5.0, "pitch": 0.05}});
    let error = ENGINE
        .start(
            &track(),
            body(wav(48_000, 2, 1.0, 440.0)),
            0,
            100,
            filters.as_object().unwrap(),
            Preference::Auto,
        )
        .err()
        .expect("unsupported native ratio must not silently clamp");
    assert!(error.needs_ffmpeg());
}

#[test]
#[ignore = "set RUSTLINK_TEST_FFMPEG to an installed FFmpeg executable"]
fn ffmpeg_queue_resumes_after_backpressure_and_drop_closes_the_child() {
    let program = ffmpeg_bin();
    let mut frames = FfmpegFrames::start(
        &program,
        body(wav(48_000, 2, 1.0, 440.0)),
        0,
        &FilterSet::default(),
        128_000,
        10,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let mut packet = Vec::new();
    let mut count = 0;
    while frames.next_frame(&mut packet).unwrap() {
        assert!(!packet.is_empty());
        count += 1;
    }
    assert!((50..=52).contains(&count), "unexpected FFmpeg packet count: {count}");
    let (done, completion) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let frames = FfmpegFrames::start(
            &program,
            body(wav(48_000, 2, 5.0, 440.0)),
            0,
            &FilterSet::default(),
            128_000,
            10,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        drop(frames);
        let _ = done.send(());
    });
    completion
        .recv_timeout(Duration::from_secs(5))
        .expect("dropping a full queue hung the FFmpeg child");
}

#[test]
#[ignore = "requires RUSTLINK_TEST_FFMPEG pointing at a real FFmpeg with libopus"]
fn native_and_ffmpeg_music_profiles_preserve_duration_and_stereo() {
    let program = ffmpeg_bin();
    let engine = Engine { ffmpeg: None, in_process: true, bitrate: 192_000, complexity: 10 };
    for rate in [44_100, 48_000] {
        let mut source = wav(rate, 2, 1.0, 1000.0);
        for frame in source[44..].as_chunks_mut::<4>().0 {
            frame[2..4].fill(0);
        }
        let mut native = engine
            .start(&track(), body(source.clone()), 0, 100, &Default::default(), Preference::Auto)
            .unwrap();
        let mut fallback =
            FfmpegFrames::start(&program, body(source), 0, &FilterSet::default(), 192_000, 10)
                .unwrap();
        for (name, frames) in [
            ("native", &mut *native as &mut dyn Frames),
            ("ffmpeg", &mut fallback as &mut dyn Frames),
        ] {
            let packets = drain_frames(frames);
            assert_eq!(packets.len(), 51, "{rate} Hz must retain the delayed final samples");
            let decoded = decode_packets(packets);
            let pairs = decoded[12_000 * 2..36_000 * 2].as_chunks::<2>().0;
            let left: f64 = pairs.iter().map(|s| (s[0] as f64).powi(2)).sum();
            let right: f64 = pairs.iter().map(|s| (s[1] as f64).powi(2)).sum();
            assert!(left > 1e10 && right / left < 0.001, "{name} rate={rate}: L={left}, R={right}");
        }
    }
}

#[test]
#[ignore = "requires RUSTLINK_TEST_FFMPEG pointing at a real FFmpeg with libopus"]
fn ffmpeg_reports_real_diagnostics_for_an_invalid_media_stream() {
    let mut frames =
        FfmpegFrames::start(&ffmpeg_bin(), body(Vec::new()), 0, &FilterSet::default(), 192_000, 10)
            .unwrap();
    let error = frames.next_frame(&mut Vec::new()).unwrap_err();
    let AudioError::Ffmpeg(message) = error else {
        panic!("expected FFmpeg's own diagnostics, got {error}");
    };
    assert!(
        !message.starts_with("ffmpeg exited with")
            && message.to_ascii_lowercase().contains("invalid")
    );
}

#[test]
#[ignore = "requires RUSTLINK_TEST_FFMPEG pointing at a real FFmpeg with libopus"]
fn ffmpeg_does_not_report_a_failed_source_read_as_a_finished_track() {
    struct InterruptedSource(Cursor<Vec<u8>>);
    impl Read for InterruptedSource {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if out.is_empty() {
                return Ok(0);
            }
            match self.0.read(out)? {
                0 => Err(io::Error::other("simulated media connection reset")),
                read => Ok(read),
            }
        }
    }

    let mut source = wav(48_000, 2, 2.0, 440.0);
    source.truncate(44 + 48_000 * 4);
    let mut frames = FfmpegFrames::start(
        &ffmpeg_bin(),
        Body::new(Box::new(InterruptedSource(Cursor::new(source)))),
        0,
        &FilterSet::default(),
        192_000,
        10,
    )
    .unwrap();
    let mut packet = Vec::new();
    let error = loop {
        match frames.next_frame(&mut packet) {
            Ok(true) => continue,
            Ok(false) => panic!("source failure was reported as normal end of track"),
            Err(error) => break error,
        }
    };
    assert!(error.to_string().contains("simulated media connection reset"), "{error}");
}

#[test]
#[ignore = "requires RUSTLINK_TEST_FFMPEG pointing at a real FFmpeg with libopus"]
fn native_and_ffmpeg_equalizer_bandwidth_agree_at_high_frequencies() {
    let engine =
        Engine { ffmpeg: Some(ffmpeg_bin()), in_process: true, bitrate: 192_000, complexity: 10 };
    for (band, frequency) in [(8, 700.0), (13, 8_000.0), (14, 12_000.0)] {
        let filters = json!({"equalizer": [{"band": band, "gain": 0.25}]});
        let level_for = |preference| {
            let mut frames = engine
                .start(
                    &track(),
                    body(wav(48_000, 2, 1.0, frequency)),
                    0,
                    100,
                    filters.as_object().unwrap(),
                    preference,
                )
                .unwrap();
            let decoded = decode_packets(drain_frames(&mut *frames));
            rms_i16(&decoded[12_000 * 2..36_000 * 2])
        };
        let ratio = level_for(Preference::Auto) / level_for(Preference::Ffmpeg);
        assert!((0.95..=1.05).contains(&ratio), "band {band}, tone {frequency} Hz: ratio {ratio}");
    }
}

#[test]
fn native_does_not_report_a_source_unexpected_eof_as_a_finished_track() {
    struct TruncatedSource(Cursor<Vec<u8>>);
    impl Read for TruncatedSource {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if out.is_empty() {
                return Ok(0);
            }
            match self.0.read(out)? {
                0 => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "media retries exhausted")),
                count => Ok(count),
            }
        }
    }

    let mut source = wav(48_000, 2, 4.0, 440.0);
    source.truncate(44 + 48_000 * 4 * 2);
    let engine = Engine { ffmpeg: None, in_process: true, bitrate: 192_000, complexity: 10 };
    let mut frames = engine
        .start(
            &track(),
            Body::new(Box::new(TruncatedSource(Cursor::new(source)))),
            0,
            100,
            &Default::default(),
            Preference::Auto,
        )
        .unwrap();
    let mut packet = Vec::new();
    let error = loop {
        match frames.next_frame(&mut packet) {
            Ok(true) => continue,
            Ok(false) => panic!("native decoder reported source truncation as normal EOF"),
            Err(error) => break error,
        }
    };
    assert!(error.to_string().contains("media retries exhausted"), "{error}");
}

#[test]
#[ignore = "requires RUSTLINK_TEST_FFMPEG to generate real Ogg Vorbis/FLAC fixtures"]
fn native_ogg_vorbis_and_flac_do_not_require_a_fallback_process() {
    let program = ffmpeg_bin();
    for codec in ["libvorbis", "flac"] {
        let mut child = Command::new(&program)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-f",
                "wav",
                "-i",
                "pipe:0",
                "-c:a",
                codec,
                "-f",
                "ogg",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || input.write_all(&wav(48_000, 2, 1.0, 1000.0)));
        let output = child.wait_with_output().unwrap();
        writer.join().unwrap().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let decoded = decode_packets(collect(output.stdout, json!({}), 0));
        assert!(
            (48_000..=50_880).contains(&(decoded.len() / 2)),
            "{codec}: unexpected duration {} frames",
            decoded.len() / 2
        );
        let rms = rms_i16(&decoded[12_000 * 2..36_000 * 2]);
        assert!((7000.0..10000.0).contains(&rms), "{codec}: unexpected RMS {rms}");
    }
}

fn ogg_opus_packets(packet_count: u32, silence_prefix_packets: u32) -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut out = Vec::new();
    write_ogg_headers(312, 0, &mut out);
    let mut encoder = rustlink_audio::Encoder::new(160_000, 8).unwrap();
    let silence_pcm = [0i16; 1920];
    let mut raw_packets = Vec::with_capacity(packet_count as usize);
    for idx in 0..packet_count {
        let pkt = if idx < silence_prefix_packets {
            encoder.encode(&silence_pcm).unwrap().to_vec()
        } else {
            let base = idx as usize * 960;
            let pcm: Vec<i16> = (0..1920)
                .map(|s| {
                    ((((base + s / 2) as f64 / 48_000.0) * 440.0 * TAU).sin() * 14_000.0) as i16
                })
                .collect();
            encoder.encode(&pcm).unwrap().to_vec()
        };
        let flags = if idx + 1 == packet_count { 4 } else { 0 };
        write_ogg_page(idx + 2, flags, u64::from(idx + 1) * 960, &pkt, &mut out);
        raw_packets.push(pkt);
    }
    (out, raw_packets)
}

#[test]
fn ogg_opus_passthrough_preserves_exact_packets_and_switches_cleanly_on_live_volume_change() {
    let (stream, expected_packets) = ogg_opus_packets(25, 0);
    assert_eq!(
        collect(stream.clone(), json!({}), 0),
        expected_packets,
        "neutral 48 kHz Opus must stream bit-identically"
    );

    let engine = Engine { ffmpeg: None, in_process: true, bitrate: 192_000, complexity: 8 };
    let mut frames = engine
        .start(&track(), body(stream), 0, 100, &Default::default(), Preference::Auto)
        .unwrap();
    let (mut emitted, mut buf) = (Vec::new(), Vec::new());
    for expected in expected_packets.iter().take(6) {
        assert!(frames.next_frame(&mut buf).unwrap());
        assert_eq!(&buf, expected);
        emitted.push(buf.clone());
    }
    let half_vol = rustlink_audio::parse_filters(&Default::default(), 50).unwrap();
    assert!(frames.set_filters(&half_vol));
    while frames.next_frame(&mut buf).unwrap() {
        emitted.push(buf.clone());
    }
    assert!(emitted.len() >= 25);
    let decoded = decode_packets(emitted);
    let boundary_rms = rms_i16(&decoded[5 * 1920..8 * 1920]);
    let half_rms = rms_i16(&decoded[10 * 1920..20 * 1920]);
    assert!(boundary_rms > 3000.0, "warm handoff lost signal across boundary: {boundary_rms}");
    assert!((4000.0..6500.0).contains(&half_rms), "expected ~50% tone RMS, got {half_rms}");
}

#[test]
fn compressed_packet_fast_seek_matches_decoded_sample_offset() {
    let (stream, _) = ogg_opus_packets(250, 150);
    let sought = collect(stream, json!({}), 3_000);
    assert!((99..=101).contains(&sought.len()), "got {} frames", sought.len());
    let decoded = decode_packets(sought);
    let rms = rms_i16(&decoded[4 * 1920..50 * 1920]);
    assert!(rms > 8000.0, "fast-sought region did not land on the 440 Hz tone: rms={rms}");
}
