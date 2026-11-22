//! Reads a real YouTube track to its end. Ignored by default because it needs
//! the network and YouTube's answer changes; run it when clients are updated:
//! `cargo test -p rustlink-source --test live_youtube -- --ignored --nocapture`.
//! `RUSTLINK_YOUTUBE_PROXY` is honoured, so it can check a proxy as well.

use std::io::Read;

use rustlink_config::{apply_env, Config};
use rustlink_protocol::LoadResult;
use rustlink_source::Service;

#[test]
#[ignore = "talks to YouTube"]
fn a_youtube_track_is_read_past_the_first_minute() {
    let mut config = Config::default();
    apply_env(&mut config);
    let service = Service::new(config);
    let LoadResult::Track(track) = service.load("https://youtu.be/dQw4w9WgXcQ").unwrap() else {
        panic!("the video did not resolve to a track");
    };
    let mut audio = service.open(&track).unwrap();
    let (mut total, mut buffer) = (0usize, vec![0; 64 << 10]);
    loop {
        match audio.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => total += read,
            Err(error) => panic!("stopped after {total} bytes: {error}"),
        }
    }
    println!("read {total} bytes");
    // Without a trusted client YouTube stops at ~900 KiB, about one minute.
    assert!(total > 2 << 20, "only {total} bytes were served");
}
