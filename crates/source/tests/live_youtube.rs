//! Reads real YouTube tracks, searches, and Mix playlists against live InnerTube.
//! Ignored by default because it needs the network and YouTube's answer changes;
//! run it when clients are updated:
//! `cargo test -p rustlink-source --test live_youtube -- --ignored --nocapture`.
//! `RUSTLINK_YOUTUBE_PROXY` is honoured, so it can check a proxy as well.

use std::io::Read;

use rustlink_config::{apply_env, Config};
use rustlink_protocol::LoadResult;
use rustlink_source::Service;

fn live_service() -> Service {
    let mut config = Config::default();
    apply_env(&mut config);
    Service::new(config)
}

#[test]
#[ignore = "talks to YouTube"]
fn a_youtube_track_is_read_past_the_first_minute() {
    let service = live_service();
    let LoadResult::Track(track) = service.load("https://youtu.be/f2u9F-IT9wY").unwrap() else {
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
    println!("read {total} bytes of f2u9F-IT9wY");
    // Without a trusted client YouTube stops at ~900 KiB, about one minute.
    assert!(total > 2 << 20, "only {total} bytes were served");
}

#[test]
#[ignore = "talks to YouTube"]
fn vevo_track_search_and_mix_playlist_resolve_without_bot_blocks() {
    let service = live_service();

    let LoadResult::Track(track) =
        service.load("https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap()
    else {
        panic!("VEVO watch link did not resolve to a track");
    };
    let mut audio = service.open(&track).unwrap();
    let (mut total, mut buffer) = (0usize, vec![0; 64 << 10]);
    while total < (1536 << 10) {
        match audio.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => total += read,
            Err(error) => panic!("VEVO stream stopped after {total} bytes: {error}"),
        }
    }
    println!("read {total} bytes of dQw4w9WgXcQ");
    assert!(total >= 1536 << 10, "VEVO stream ended early at {total} bytes");

    let LoadResult::Search(results) =
        service.load("ytsearch:Rick Astley Never Gonna Give You Up").unwrap()
    else {
        panic!("ytsearch did not return search results");
    };
    assert!(!results.is_empty(), "ytsearch returned zero tracks");

    let LoadResult::Playlist(mix) = service.load("youtube:playlist:RDdQw4w9WgXcQ").unwrap() else {
        panic!("RD Mix playlist did not resolve");
    };
    assert!(!mix.tracks.is_empty(), "RD Mix playlist returned zero tracks");
}
