//! End-to-end checks of the in-process pipeline, over real bytes.

use rustlink_audio::{Body, Engine, Preference};
use rustlink_protocol::{Track, TrackInfo};

/// Builds a WAV file holding a tone, at the given rate and channel count.
fn wav(rate: u32, channels: u16, seconds: f64, frequency: f64) -> Vec<u8> {
    let frames = (rate as f64 * seconds) as u32;
    let bits = 16u16;
    let block_align = channels * bits / 8;
    let data_len = frames * block_align as u32;
    let mut out = Vec::with_capacity(data_len as usize + 44);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * block_align as u32).to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for frame in 0..frames {
        let value = (2.0 * std::f64::consts::PI * frequency * frame as f64 / rate as f64).sin();
        let sample = (value * 12_000.0) as i16;
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

fn collect(bytes: Vec<u8>, filters: serde_json::Value, position_ms: i64) -> Vec<Vec<u8>> {
    let engine = Engine { ffmpeg: None, in_process: true, bitrate: 128_000, complexity: 10 };
    let object = match filters {
        serde_json::Value::Object(map) => map,
        _ => panic!("not an object"),
    };
    let mut frames = engine
        .start(
            &track(),
            Body::new(Box::new(std::io::Cursor::new(bytes))),
            position_ms,
            100,
            &object,
            Preference::Auto,
        )
        .expect("the pipeline should accept a wav");
    let mut out = Vec::new();
    let mut frame = Vec::new();
    while frames.next_frame(&mut frame).expect("no frame should fail") {
        out.push(frame.clone());
    }
    out
}

#[test]
fn a_one_second_wav_becomes_fifty_frames() {
    let frames = collect(wav(48_000, 2, 1.0, 440.0), serde_json::json!({}), 0);
    assert_eq!(frames.len(), 50, "a second at 20 ms a frame is fifty frames");
    for frame in &frames {
        assert!(!frame.is_empty());
        assert!(frame.len() < 1500);
    }
}

#[test]
fn a_source_at_another_rate_is_resampled_to_the_same_length() {
    for rate in [8_000, 22_050, 44_100, 96_000] {
        let frames = collect(wav(rate, 2, 1.0, 440.0), serde_json::json!({}), 0);
        assert_eq!(frames.len(), 50, "a second at {rate} Hz should still be fifty frames");
    }
}

#[test]
fn a_mono_source_is_folded_to_stereo() {
    let frames = collect(wav(48_000, 1, 0.5, 440.0), serde_json::json!({}), 0);
    assert_eq!(frames.len(), 25);
}

#[test]
fn filters_do_not_change_how_many_frames_come_out() {
    let frames = collect(
        wav(44_100, 2, 1.0, 440.0),
        serde_json::json!({
            "volume": 0.5,
            "equalizer": [{"band": 8, "gain": 0.25}],
            "tremolo": {"frequency": 5, "depth": 0.5},
            "channelMix": {"leftToRight": 0.1}
        }),
        0,
    );
    assert_eq!(frames.len(), 50);
}

#[test]
fn a_timescale_changes_the_length_in_the_right_direction() {
    let plain = collect(wav(48_000, 2, 1.0, 440.0), serde_json::json!({}), 0).len();
    let faster =
        collect(wav(48_000, 2, 1.0, 440.0), serde_json::json!({"timescale": {"speed": 2.0}}), 0)
            .len();
    assert!(faster < plain, "twice the speed should be fewer frames, {faster} vs {plain}");
    assert!(faster > plain / 3, "twice the speed should not be a third of the frames");
}

#[test]
fn a_seek_starts_later_and_produces_less() {
    let whole = collect(wav(48_000, 2, 1.0, 440.0), serde_json::json!({}), 0).len();
    let half = collect(wav(48_000, 2, 1.0, 440.0), serde_json::json!({}), 500).len();
    assert!(half < whole, "a seek should skip frames, {half} vs {whole}");
    assert!(
        (24..=26).contains(&half),
        "half a second in should leave about twenty five frames, got {half}"
    );
}

#[test]
fn an_unknown_container_is_declined_so_the_caller_can_use_ffmpeg() {
    let engine = Engine { ffmpeg: None, in_process: true, bitrate: 128_000, complexity: 10 };
    let error = engine
        .start(
            &track(),
            Body::new(Box::new(std::io::Cursor::new(b"this is not audio".to_vec()))),
            0,
            100,
            &Default::default(),
            Preference::Auto,
        )
        .err()
        .expect("an unknown container should not play");
    assert!(error.needs_ffmpeg(), "the caller should be told to retry with ffmpeg: {error}");
}

fn ogg_opus(pre_skip: u16, output_gain: i16) -> Vec<u8> {
    fn page(sequence: u32, packet: &[u8], out: &mut Vec<u8>) {
        out.extend_from_slice(b"OggS\0\0");
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&sequence.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        let segments = packet.len() / 255 + 1;
        out.push(segments as u8);
        out.extend(std::iter::repeat_n(255, segments - 1));
        out.push((packet.len() % 255) as u8);
        out.extend_from_slice(packet);
    }
    let mut out = Vec::new();
    let mut head = b"OpusHead\x01\x02".to_vec();
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&output_gain.to_le_bytes());
    head.push(0);
    page(0, &head, &mut out);
    page(1, b"OpusTags\0\0\0\0\0\0\0\0", &mut out);
    let mut encoder = rustlink_audio::Encoder::new(128_000, 10).unwrap();
    let pcm: Vec<i16> = (0..1920)
        .map(|i| {
            ((i / 2) as f64 * 440.0 * std::f64::consts::TAU / 48_000.0).sin().mul_add(12_000.0, 0.0)
                as i16
        })
        .collect();
    for sequence in 2..4 {
        page(sequence, encoder.encode(&pcm).unwrap(), &mut out);
    }
    out
}

#[test]
fn ogg_retains_the_first_audio_packet_and_applies_pre_skip_once() {
    for skip in [0u16, 312, 1272] {
        let mut decoder = rustlink_audio::decode::Decoder::new(Body::new(Box::new(
            std::io::Cursor::new(ogg_opus(skip, 0)),
        )))
        .unwrap();
        let mut samples = Vec::new();
        while decoder.decode(&mut samples).unwrap() {}
        assert_eq!(samples.len(), (1920 - skip as usize) * 2);
        assert!(samples.iter().any(|sample| sample.abs() > 0.1));
    }
}

#[test]
fn ogg_output_gain_is_applied_to_the_decoded_samples() {
    fn decode(gain: i16) -> Vec<f32> {
        let mut decoder = rustlink_audio::decode::Decoder::new(Body::new(Box::new(
            std::io::Cursor::new(ogg_opus(312, gain)),
        )))
        .unwrap();
        let mut samples = Vec::new();
        while decoder.decode(&mut samples).unwrap() {}
        samples
    }
    let plain = decode(0);
    let louder = decode(6 * 256);
    let gain = 10.0f32.powf(6.0 / 20.0);
    assert_eq!(plain.len(), louder.len());
    for (a, b) in plain.into_iter().zip(louder) {
        assert!((b - a * gain).abs() < 1e-6);
    }
}

#[test]
fn a_unity_conversion_preserves_even_a_very_short_source() {
    let frames = collect(wav(48_000, 2, 4.0 / 48_000.0, 440.0), serde_json::json!({}), 0);
    assert_eq!(frames.len(), 1);
}

#[test]
fn pitch_preserves_duration_and_rate_changes_duration() {
    let pitched =
        collect(wav(48_000, 2, 1.0, 440.0), serde_json::json!({"timescale": {"pitch": 2.0}}), 0);
    assert!(
        (48..=52).contains(&pitched.len()),
        "pitch must preserve duration: {} packets",
        pitched.len()
    );
    let faster =
        collect(wav(48_000, 2, 1.0, 440.0), serde_json::json!({"timescale": {"rate": 2.0}}), 0);
    assert_eq!(faster.len(), 25);
}

#[test]
fn extreme_combined_timescale_requests_the_bounded_fallback() {
    let engine = Engine { ffmpeg: None, in_process: true, bitrate: 128_000, complexity: 10 };
    let filters = serde_json::json!({"timescale": {"speed": 5.0, "pitch": 0.05}});
    let error = engine
        .start(
            &track(),
            Body::new(Box::new(std::io::Cursor::new(wav(48_000, 2, 1.0, 440.0)))),
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
    use rustlink_audio::{ffmpeg::FfmpegFrames, FilterSet, Frames};
    use std::time::Duration;

    let program = std::env::var("RUSTLINK_TEST_FFMPEG").expect("RUSTLINK_TEST_FFMPEG is required");
    let mut frames = FfmpegFrames::start(
        &program,
        Body::new(Box::new(std::io::Cursor::new(wav(48_000, 2, 1.0, 440.0)))),
        0,
        &FilterSet::default(),
        128_000,
        10,
    )
    .unwrap();
    // Let the bounded packet queue fill, just as it does when playback pauses.
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
            Body::new(Box::new(std::io::Cursor::new(wav(48_000, 2, 5.0, 440.0)))),
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
