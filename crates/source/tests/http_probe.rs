//! Loading a plain URL reads far enough into the file to find its length, but a
//! stream that never ends is not read on.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustlink_config::Config;
use rustlink_protocol::LoadResult;
use rustlink_source::Service;

/// Answers one request with `head` and `body`, then keeps the connection open
/// for `hold` as a live stream would.
fn serve(head: String, body: Vec<u8>, hold: Duration) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/audio", listener.local_addr().unwrap());
    let worker = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let count = socket.read(&mut buffer).unwrap();
            assert!(count > 0);
            request.extend_from_slice(&buffer[..count]);
        }
        let _ = socket.write_all(head.as_bytes());
        let _ = socket.write_all(&body);
        std::thread::sleep(hold);
    });
    (url, worker)
}

fn sized(body: Vec<u8>) -> (String, JoinHandle<()>) {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: audio/mp4\r\nConnection: close\r\n\r\n",
        body.len()
    );
    serve(head, body, Duration::ZERO)
}

fn service() -> Service {
    let mut cfg = Config::default();
    cfg.allow_private_networks = true;
    Service::new(cfg)
}

fn track_of(result: LoadResult) -> rustlink_protocol::Track {
    match result {
        LoadResult::Track(track) => *track,
        other => panic!("expected a track, got {}", other.load_type()),
    }
}

fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut bytes = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    bytes.extend_from_slice(kind);
    bytes.extend_from_slice(body);
    bytes
}

/// An M4A written without "fast start": the audio comes first and the movie
/// box that names the length comes after it.
fn m4a_with_the_index_last(audio_bytes: usize, seconds: u32) -> Vec<u8> {
    let mut bytes = atom(b"ftyp", b"M4A \0\0\0\0");
    bytes.extend(atom(b"mdat", &vec![0u8; audio_bytes]));
    let mut mvhd = vec![0u8; 20];
    mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
    mvhd[16..20].copy_from_slice(&(seconds * 1000).to_be_bytes());
    let mut handler = vec![0u8; 8];
    handler.extend_from_slice(b"soun");
    handler.extend_from_slice(&[0; 12]);
    let trak = atom(b"trak", &atom(b"mdia", &atom(b"hdlr", &handler)));
    let mut moov = atom(b"mvhd", &mvhd);
    moov.extend(trak);
    bytes.extend(atom(b"moov", &moov));
    bytes
}

fn ogg_page(flags: u8, granule: u64, payload: &[u8]) -> Vec<u8> {
    let mut page = b"OggS\0".to_vec();
    page.push(flags);
    page.extend_from_slice(&granule.to_le_bytes());
    page.extend_from_slice(&[0; 12]);
    page.push(1);
    page.push(payload.len() as u8);
    page.extend_from_slice(payload);
    page
}

/// An Ogg Opus file of `packets` 20 ms packets, long enough to run past the
/// first 16 KiB, whose length is only stated by its last page.
fn ogg_opus(packets: u64) -> Vec<u8> {
    let mut head = b"OpusHead\x01\x02".to_vec();
    head.extend_from_slice(&0u16.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&[0; 3]);
    let mut bytes = ogg_page(2, 0, &head);
    bytes.extend(ogg_page(0, 0, b"OpusTags\0\0\0\0\0\0\0\0"));
    for packet in 1..packets {
        bytes.extend(ogg_page(0, packet * 960, &[0xf8, 0xff, 0xfe]));
    }
    bytes.extend(ogg_page(4, packets * 960, &[0xf8, 0xff, 0xfe]));
    bytes
}

#[test]
fn an_m4a_whose_index_follows_its_audio_loads_with_its_length() {
    let file = m4a_with_the_index_last(200_000, 180);
    assert!(file.len() > 16 << 10, "the index must lie past the first window");
    let (url, server) = sized(file);
    let track = track_of(service().load(&url).expect("the index at the end is found"));
    server.join().unwrap();
    assert_eq!(track.metadata.length, 180_000);
    assert!(track.metadata.is_seekable && !track.metadata.is_stream);
}

#[test]
fn an_ogg_longer_than_the_first_window_states_its_length() {
    let file = ogg_opus(1500);
    assert!(file.len() > 16 << 10);
    let (url, server) = sized(file);
    let track = track_of(service().load(&url).expect("the last page is reached"));
    server.join().unwrap();
    assert_eq!(track.metadata.length, 30_000, "1500 packets of 20 ms");
    assert!(track.metadata.is_seekable, "a file with a known length can be sought");
}

#[test]
fn a_stream_with_no_length_is_not_read_on_past_its_first_window() {
    // An MP4-looking head with no movie box and no declared length, as a live
    // stream would be. Reading on for its index would wait for bytes that may
    // never come, so the load gives up after the first window instead.
    let mut body = atom(b"ftyp", b"M4A \0\0\0\0");
    body.extend(atom(b"mdat", &vec![0u8; 100_000]));
    let chunked = {
        let mut out = Vec::new();
        for part in body.chunks(8192) {
            out.extend(format!("{:x}\r\n", part.len()).into_bytes());
            out.extend(part);
            out.extend(b"\r\n");
        }
        out
    };
    let head = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: audio/mp4\r\nConnection: close\r\n\r\n".to_string();
    let (url, server) = serve(head, chunked, Duration::from_secs(3));
    let started = Instant::now();
    let result = service().load(&url);
    let waited = started.elapsed();
    server.join().unwrap();
    assert!(result.is_err(), "there is no movie box to find");
    assert!(waited < Duration::from_secs(2), "the load waited {waited:?} on an unfinished stream");
}
