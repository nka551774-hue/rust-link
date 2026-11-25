//! Bounded HTTP container probing for non-faststart MP4, Ogg Opus, and live streams.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustlink_config::Config;
use rustlink_protocol::{LoadResult, Track};
use rustlink_source::Service;

fn serve(head: String, body: Vec<u8>, hold: Duration) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/audio", listener.local_addr().unwrap());
    let worker = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let (mut req, mut buf) = (Vec::new(), [0u8; 1024]);
        while !req.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = socket.read(&mut buf).unwrap();
            assert!(n > 0);
            req.extend_from_slice(&buf[..n]);
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

fn track_of(result: LoadResult) -> Track {
    let LoadResult::Track(track) = result else {
        panic!("expected a track, got {}", result.load_type());
    };
    *track
}

fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut bytes = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    bytes.extend_from_slice(kind);
    bytes.extend_from_slice(body);
    bytes
}

fn m4a_with_the_index_last(audio_bytes: usize, seconds: u32) -> Vec<u8> {
    let mut bytes = atom(b"ftyp", b"M4A \0\0\0\0");
    bytes.extend(atom(b"mdat", &vec![0u8; audio_bytes]));
    let mut mvhd = vec![0u8; 20];
    mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
    mvhd[16..20].copy_from_slice(&(seconds * 1000).to_be_bytes());
    let mut handler = vec![0u8; 8];
    handler.extend_from_slice(b"soun");
    handler.extend_from_slice(&[0; 12]);
    let mut moov = atom(b"mvhd", &mvhd);
    moov.extend(atom(b"trak", &atom(b"mdia", &atom(b"hdlr", &handler))));
    bytes.extend(atom(b"moov", &moov));
    bytes
}

fn ogg_page(flags: u8, granule: u64, payload: &[u8]) -> Vec<u8> {
    let mut page = b"OggS\0".to_vec();
    page.push(flags);
    page.extend_from_slice(&granule.to_le_bytes());
    page.extend_from_slice(&[0; 12]);
    page.extend_from_slice(&[1, payload.len() as u8]);
    page.extend_from_slice(payload);
    page
}

fn ogg_opus(packets: u64) -> Vec<u8> {
    let mut head = b"OpusHead\x01\x02\0\0".to_vec();
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
    assert!(file.len() > 16 << 10);
    let (url, server) = sized(file);
    let track = track_of(service().load(&url).unwrap());
    server.join().unwrap();
    assert_eq!(track.metadata.length, 180_000);
    assert!(track.metadata.is_seekable && !track.metadata.is_stream);
}

#[test]
fn an_ogg_longer_than_the_first_window_states_its_length() {
    let file = ogg_opus(1500);
    assert!(file.len() > 16 << 10);
    let (url, server) = sized(file);
    let track = track_of(service().load(&url).unwrap());
    server.join().unwrap();
    assert_eq!(track.metadata.length, 30_000);
    assert!(track.metadata.is_seekable);
}

#[test]
fn a_stream_with_no_length_is_not_read_on_past_its_first_window() {
    let mut body = atom(b"ftyp", b"M4A \0\0\0\0");
    body.extend(atom(b"mdat", &vec![0u8; 100_000]));
    let mut chunked = Vec::new();
    for part in body.chunks(8192) {
        chunked.extend_from_slice(format!("{:x}\r\n", part.len()).as_bytes());
        chunked.extend_from_slice(part);
        chunked.extend_from_slice(b"\r\n");
    }
    let head = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: audio/mp4\r\nConnection: close\r\n\r\n".into();
    let (url, server) = serve(head, chunked, Duration::from_secs(3));
    let started = Instant::now();
    let result = service().load(&url);
    let waited = started.elapsed();
    server.join().unwrap();
    assert!(result.is_err() && waited < Duration::from_secs(2));
}
