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
fn a_one_second_wav_includes_the_encoder_delayed_tail() {
    let frames = collect(wav(48_000, 2, 1.0, 440.0), serde_json::json!({}), 0);
    assert_eq!(frames.len(), 51, "fifty frames plus the encoder's delayed tail");
    for frame in &frames {
        assert!(!frame.is_empty());
        assert!(frame.len() < 1500);
    }
}

#[test]
fn a_source_at_another_rate_is_resampled_to_the_same_length() {
    for rate in [8_000, 22_050, 44_100, 96_000] {
        let frames = collect(wav(rate, 2, 1.0, 440.0), serde_json::json!({}), 0);
        assert_eq!(frames.len(), 51, "a second at {rate} Hz must include its delayed tail");
    }
}

#[test]
fn a_mono_source_is_folded_to_stereo() {
    let frames = collect(wav(48_000, 1, 0.5, 440.0), serde_json::json!({}), 0);
    assert_eq!(frames.len(), 26);
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
    assert_eq!(frames.len(), 51);
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
fn a_large_seek_cannot_wrap_back_to_the_start() {
    let wrapping_position = ((u64::MAX as u128 + 1).div_ceil(48_000)) as i64;
    let frames = collect(wav(48_000, 2, 1.0, 440.0), serde_json::json!({}), wrapping_position);
    assert!(frames.is_empty(), "a seek beyond EOF must not play from a wrapped sample count");
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
    ogg_opus_with_end(pre_skip, output_gain, None)
}

fn ogg_opus_with_end(pre_skip: u16, output_gain: i16, end: Option<u64>) -> Vec<u8> {
    fn page(sequence: u32, packet: &[u8], end: Option<u64>, out: &mut Vec<u8>) {
        out.extend_from_slice(b"OggS\0");
        out.push(if end.is_some() { 4 } else { 0 });
        let granule = end.unwrap_or(u64::from(sequence.saturating_sub(1)) * 960);
        out.extend_from_slice(&granule.to_le_bytes());
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
    page(0, &head, None, &mut out);
    page(1, b"OpusTags\0\0\0\0\0\0\0\0", None, &mut out);
    let mut encoder = rustlink_audio::Encoder::new(128_000, 10).unwrap();
    let pcm: Vec<i16> = (0..1920)
        .map(|i| {
            ((i / 2) as f64 * 440.0 * std::f64::consts::TAU / 48_000.0).sin().mul_add(12_000.0, 0.0)
                as i16
        })
        .collect();
    for sequence in 2..4 {
        page(
            sequence,
            encoder.encode(&pcm).unwrap(),
            if sequence == 3 { end } else { None },
            &mut out,
        );
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
fn ogg_end_granule_trims_padding_after_pre_skip() {
    for (skip, end) in [(312, 1500), (1272, 1500)] {
        let mut decoder = rustlink_audio::decode::Decoder::new(Body::new(Box::new(
            std::io::Cursor::new(ogg_opus_with_end(skip, 0, Some(end))),
        )))
        .unwrap();
        let mut samples = Vec::new();
        while decoder.decode(&mut samples).unwrap() {}
        assert_eq!(samples.len(), (end as usize - skip as usize) * 2);
    }
}

fn decode_packets(packets: Vec<Vec<u8>>) -> Vec<i16> {
    let mut decoder = rustlink_audio::opus_decode::Decoder::new().unwrap();
    let mut samples = Vec::new();
    let mut decoded = Vec::new();
    for packet in packets {
        decoder.decode(&packet, &mut decoded).unwrap();
        samples.extend_from_slice(&decoded);
    }
    samples
}

#[test]
fn the_last_audible_samples_survive_opus_lookahead() {
    let mut source = wav(48_000, 2, 0.02, 1000.0);
    // A short tone only at the end: omitting the encoder flush loses all of it.
    source[44..44 + 760 * 4].fill(0);
    let decoded = decode_packets(collect(source, serde_json::json!({}), 0));
    let last = &decoded[(312 + 832) * 2..(312 + 960) * 2];
    let energy =
        last.iter().map(|sample| (*sample as f64).powi(2)).sum::<f64>() / last.len() as f64;
    assert!(energy.sqrt() > 3000.0, "the final source tone must not be cut off");
}

#[test]
fn equalizer_frequency_is_independent_of_source_rate() {
    for rate in [8_000, 44_100, 48_000] {
        let rms = |filters| {
            let decoded = decode_packets(collect(wav(rate, 2, 1.0, 1000.0), filters, 0));
            let settled = &decoded[12_000 * 2..36_000 * 2];
            (settled.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / settled.len() as f64).sqrt()
        };
        let plain = rms(serde_json::json!({}));
        let filtered = rms(serde_json::json!({"equalizer": [{"band": 8, "gain": 0.25}]}));
        let ratio = filtered / plain;
        assert!((1.8..2.2).contains(&ratio), "1 kHz +6 dB at {rate} Hz gave gain {ratio}");
    }
}

#[test]
fn a_unity_conversion_preserves_even_a_very_short_source() {
    let frames = collect(wav(48_000, 2, 4.0 / 48_000.0, 440.0), serde_json::json!({}), 0);
    assert_eq!(frames.len(), 1);
}

#[test]
fn rate_conversion_preserves_even_a_very_short_source() {
    for rate in [8_000, 44_100, 96_000] {
        let frames = collect(wav(rate, 2, 4.0 / rate as f64, 440.0), serde_json::json!({}), 0);
        assert_eq!(frames.len(), 1, "short {rate} Hz source disappeared");
    }
}

#[test]
fn very_slow_playback_applies_backpressure_before_decoding_more_source() {
    use std::io::Read;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    struct Counted {
        inner: std::io::Cursor<Vec<u8>>,
        read: Arc<AtomicUsize>,
    }
    impl Read for Counted {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(bytes)?;
            self.read.fetch_add(n, Ordering::Relaxed);
            Ok(n)
        }
    }
    let read = Arc::new(AtomicUsize::new(0));
    let body = Body::new(Box::new(Counted {
        inner: std::io::Cursor::new(wav(8_000, 2, 60.0, 440.0)),
        read: read.clone(),
    }));
    let engine = Engine { ffmpeg: None, in_process: true, bitrate: 64_000, complexity: 0 };
    let filters = serde_json::json!({"timescale": {"speed": 0.05, "pitch": 0.05, "rate": 0.05}});
    let mut frames = engine
        .start(&track(), body, 0, 100, filters.as_object().unwrap(), Preference::Auto)
        .unwrap();
    let mut packet = Vec::new();
    for _ in 0..4000 {
        assert!(frames.next_frame(&mut packet).unwrap());
    }
    // 80 output seconds represent only 0.2 source seconds at this setting.
    // Allow Symphonia's read-ahead buffer, but not steadily growing decode-ahead.
    assert!(
        read.load(Ordering::Relaxed) <= 128 * 1024,
        "read {} bytes ahead",
        read.load(Ordering::Relaxed)
    );
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
    assert_eq!(faster.len(), 26);
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

#[test]
#[ignore = "requires RUSTLINK_TEST_FFMPEG pointing at a real FFmpeg with libopus"]
fn native_and_ffmpeg_music_profiles_preserve_duration_and_stereo() {
    use rustlink_audio::{ffmpeg::FfmpegFrames, FilterSet, Frames};
    let program = std::env::var("RUSTLINK_TEST_FFMPEG").expect("RUSTLINK_TEST_FFMPEG is required");
    for rate in [44_100, 48_000] {
        let mut source = wav(rate, 2, 1.0, 1000.0);
        for frame in source[44..].as_chunks_mut::<4>().0 {
            frame[2..4].fill(0); // Only the left channel carries the tone.
        }
        let engine = Engine { ffmpeg: None, in_process: true, bitrate: 192_000, complexity: 10 };
        let mut native = engine
            .start(
                &track(),
                Body::new(Box::new(std::io::Cursor::new(source.clone()))),
                0,
                100,
                &Default::default(),
                Preference::Auto,
            )
            .unwrap();
        let mut fallback = FfmpegFrames::start(
            &program,
            Body::new(Box::new(std::io::Cursor::new(source))),
            0,
            &FilterSet::default(),
            192_000,
            10,
        )
        .unwrap();
        for (name, frames) in [
            ("native", &mut *native as &mut dyn Frames),
            ("ffmpeg", &mut fallback as &mut dyn Frames),
        ] {
            let mut packets = Vec::new();
            let mut packet = Vec::new();
            while frames.next_frame(&mut packet).unwrap() {
                packets.push(packet.clone());
            }
            assert_eq!(packets.len(), 51, "{rate} Hz must retain the delayed final samples");
            let decoded = decode_packets(packets);
            let settled = &decoded[12_000 * 2..36_000 * 2];
            let left =
                settled.as_chunks::<2>().0.iter().map(|s| (s[0] as f64).powi(2)).sum::<f64>();
            let right =
                settled.as_chunks::<2>().0.iter().map(|s| (s[1] as f64).powi(2)).sum::<f64>();
            assert!(left > 1e10, "{name} rate={rate}: left channel energy={left}, right={right}");
            assert!(
                right / left < 0.001,
                "{name} rate={rate}: stereo leakage, left={left}, right={right}"
            );
        }
    }
}
