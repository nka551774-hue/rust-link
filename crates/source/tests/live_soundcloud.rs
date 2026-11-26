//! Live SoundCloud search and stream verification (`--ignored --nocapture`).

use std::io::Read;

use rustlink_config::{apply_env, Config};
use rustlink_protocol::LoadResult;
use rustlink_source::Service;

#[test]
#[ignore = "talks to SoundCloud"]
fn a_soundcloud_search_hit_streams_audio() {
    let mut config = Config::default();
    apply_env(&mut config);
    let service = Service::new(config);
    let LoadResult::Search(hits) =
        service.load("scsearch:Rick Astley Never Gonna Give You Up").unwrap()
    else {
        panic!("scsearch did not return search results");
    };
    let track = hits.first().expect("no SoundCloud results");
    println!(
        "{} by {} ({} ms)",
        track.metadata.title, track.metadata.author, track.metadata.length
    );
    let (mut audio, mut buf, mut total) = (service.open(track).unwrap(), vec![0u8; 64 << 10], 0);
    let mut kept = Vec::new();
    while total < 1 << 20 {
        match audio.read(&mut buf).unwrap() {
            0 => break,
            n => {
                kept.extend_from_slice(&buf[..n]);
                total += n;
            }
        }
    }
    let buf = kept;
    println!("read {total} bytes");
    if let Ok(path) = std::env::var("RUSTLINK_DUMP") {
        std::fs::write(path, &buf).unwrap();
    }
    assert!(total >= 1 << 20, "stream ended early at {total} bytes");
}
