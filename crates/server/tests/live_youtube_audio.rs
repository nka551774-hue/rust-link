//! End-to-end live YouTube -> pure-Rust decode -> Opus packet verification
//! with zero external processes (`ffmpeg: None`, no `yt-dlp`).
//!
//! Run manually with:
//! `cargo test -p rustlink-server --test live_youtube_audio -- --ignored --nocapture`

use rustlink_audio::{Body, Engine, Preference};
use rustlink_config::{apply_env, Config};
use rustlink_protocol::{LoadResult, Object};
use rustlink_source::Service;

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

    let reader = source.open(&track).unwrap();
    let engine = Engine { ffmpeg: None, in_process: true, bitrate: 192_000, complexity: 5 };
    let mut frames =
        engine.start(&track, Body::new(reader), 0, 100, &Object::new(), Preference::Auto).unwrap();

    let mut packet = Vec::with_capacity(1024);
    let mut count = 0usize;
    let mut bytes = 0usize;
    // 3,750 frames * 20 ms = 75 seconds, well past YouTube's ~60s / 900 KiB throttle cliff.
    while count < 3_750 && frames.next_frame(&mut packet).unwrap() {
        assert!(!packet.is_empty(), "empty Opus frame at index {count}");
        bytes += packet.len();
        count += 1;
    }
    println!("decoded and encoded {count} Opus frames ({bytes} bytes) in pure Rust");
    assert_eq!(count, 3_750, "stream ended early before 75s ({count} frames)");
}
