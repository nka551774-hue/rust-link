//! End-to-end live YouTube -> pure-Rust decode -> Opus packet verification
//! with zero external processes (`ffmpeg: None`, no `yt-dlp`).
//!
//! Run manually with:
//! `cargo test -p rustlink-server --test live_youtube_audio -- --ignored --nocapture`

use rustlink_audio::{Body, Engine, Frames, Preference};
use rustlink_config::{apply_env, Config};
use rustlink_protocol::{LoadResult, Object};
use rustlink_source::Service;

fn decode_frames(frames: &mut dyn Frames, target: usize) -> (usize, usize) {
    let mut packet = Vec::with_capacity(1024);
    let (mut count, mut bytes) = (0usize, 0usize);
    while count < target && frames.next_frame(&mut packet).unwrap() {
        assert!(!packet.is_empty(), "empty Opus frame at index {count}");
        bytes += packet.len();
        count += 1;
    }
    (count, bytes)
}

#[test]
#[ignore = "talks to YouTube"]
fn youtube_vevo_link_decodes_to_opus_frames_past_seventy_seconds_in_pure_rust() {
    let mut config = Config::default();
    apply_env(&mut config);
    let source = Service::new(config);
    let LoadResult::Track(track) =
        source.load("https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap()
    else {
        panic!("YouTube link did not resolve to a track");
    };

    let engine = Engine { ffmpeg: None, in_process: true, bitrate: 192_000, complexity: 5 };
    let mut frames = engine
        .start(
            &track,
            Body::new(source.open(&track).unwrap()),
            0,
            100,
            &Object::new(),
            Preference::Auto,
        )
        .unwrap();

    // 3,750 frames * 20 ms = 75 seconds, past YouTube's ~60s / 900 KiB throttle cliff.
    let (count, bytes) = decode_frames(frames.as_mut(), 3_750);
    println!("decoded and encoded {count} Opus frames ({bytes} bytes) in pure Rust");
    assert_eq!(count, 3_750, "stream ended early before 75s ({count} frames)");

    let seek_start = std::time::Instant::now();
    let mut seek_frames = engine
        .start(
            &track,
            Body::new(source.open(&track).unwrap()),
            120_000,
            100,
            &Object::new(),
            Preference::Auto,
        )
        .unwrap();
    let (seek_count, _) = decode_frames(seek_frames.as_mut(), 250);
    println!("sought to 120s and emitted {seek_count} Opus frames in {:?}", seek_start.elapsed());
    assert_eq!(seek_count, 250);
}
