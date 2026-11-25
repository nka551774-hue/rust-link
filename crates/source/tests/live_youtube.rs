//! Live YouTube track, search, and Mix playlist verification (`--ignored --nocapture`).

use std::io::Read;

use rustlink_config::{apply_env, Config};
use rustlink_protocol::LoadResult;
use rustlink_source::{innertube, Service};

fn live_service() -> Service {
    let mut config = Config::default();
    apply_env(&mut config);
    if config.youtube_po_token_file.trim().is_empty() {
        if let Some(found) = rustlink_config::discover_potoken(None) {
            config.youtube_po_token_file = found.to_string_lossy().into_owned();
        }
    }
    Service::new(config)
}

fn read_up_to(mut audio: Box<dyn Read + Send>, cap: usize) -> usize {
    let (mut total, mut buf) = (0usize, vec![0u8; 64 << 10]);
    while total < cap {
        match audio.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) => panic!("stopped after {total} bytes: {e}"),
        }
    }
    total
}

#[test]
#[ignore = "talks to YouTube"]
fn a_youtube_track_is_read_past_the_first_minute() {
    let service = live_service();
    let LoadResult::Track(track) = service.load("https://youtu.be/f2u9F-IT9wY").unwrap() else {
        panic!("the video did not resolve to a track");
    };
    let total = read_up_to(service.open(&track).unwrap(), usize::MAX);
    println!("read {total} bytes of f2u9F-IT9wY");
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
    let total = read_up_to(service.open(&track).unwrap(), 1536 << 10);
    println!("read {total} bytes of dQw4w9WgXcQ");
    assert!(total >= 1536 << 10, "VEVO stream ended early at {total} bytes");

    let LoadResult::Search(results) =
        service.load("ytsearch:Rick Astley Never Gonna Give You Up").unwrap()
    else {
        panic!("ytsearch did not return search results");
    };
    assert!(!results.is_empty());

    let LoadResult::Playlist(mix) = service.load("youtube:playlist:RDdQw4w9WgXcQ").unwrap() else {
        panic!("RD Mix playlist did not resolve");
    };
    assert!(!mix.tracks.is_empty());

    let cfg = service.config();
    let agent = service.youtube_agent().unwrap_or(service.agent());
    let primary = &innertube::clients(cfg)[0];
    assert_eq!(primary.name, "VISIONOS");
    let format = innertube::Catalog::new(agent, cfg)
        .player(primary, "dQw4w9WgXcQ")
        .unwrap()
        .expect("VISIONOS returned no playable format");
    println!("VISIONOS selected mime={} bitrate={}", format.mime_type, format.bitrate);
    assert!(format.mime_type.to_ascii_lowercase().contains("opus"), "{}", format.mime_type);
}
