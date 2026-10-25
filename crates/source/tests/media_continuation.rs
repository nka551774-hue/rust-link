//! Playback must never splice another representation or unverified range bytes.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustlink_config::Config;
use rustlink_source::Service;

fn serve(responses: Vec<String>) -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/audio", listener.local_addr().unwrap());
    let worker = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for response in responses {
            let deadline = Instant::now() + Duration::from_secs(4);
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(socket) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "expected media request missing");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let count = socket.read(&mut buffer).unwrap();
                assert!(count > 0 && request.len() < 16_384);
                request.extend_from_slice(&buffer[..count]);
            }
            requests.push(String::from_utf8(request).unwrap());
            // Header validation may close the connection before the body is sent.
            let _ = socket.write_all(response.as_bytes());
        }
        requests
    });
    (url, worker)
}

fn segment(range: &str, headers: &str, body: &str) -> String {
    format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {range}\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{body}", body.len())
}

fn service() -> Service {
    Service::new(Config { allow_private_networks: true, ..Config::default() })
}

#[test]
fn declared_slice_length_must_match_content_length() {
    let (url, server) = serve(vec![segment("0-1/6", "", "abc")]);
    let opened = service().open_seekable(&url, &Default::default());
    server.join().unwrap();
    assert!(opened.is_err(), "a three-byte body cannot describe a two-byte slice");
}

#[test]
fn chunked_body_cannot_deliver_bytes_past_its_declared_range() {
    let (url, server) = serve(vec!["HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-2/3\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nabcXY\r\n0\r\n\r\n".into()]);
    let mut stream = service().open_seekable(&url, &Default::default()).unwrap();
    let mut bytes = Vec::new();
    let result = stream.read_to_end(&mut bytes);
    server.join().unwrap();
    assert!(result.is_err(), "overlong range body was treated as successful audio");
    assert_eq!(bytes, b"abc", "unverified bytes reached the decoder");
}

#[test]
fn same_size_replacement_is_rejected_before_its_audio_is_exposed() {
    let (url, server) = serve(vec![
        segment("0-2/6", "ETag: \"first\"\r\n", "abc"),
        segment("3-5/6", "ETag: \"replacement\"\r\n", "XYZ"),
    ]);
    let mut stream = service().open_seekable(&url, &Default::default()).unwrap();
    let mut bytes = Vec::new();
    let result = stream.read_to_end(&mut bytes);
    server.join().unwrap();
    assert!(result.is_err(), "mixed two different six-byte recordings");
    assert_eq!(bytes, b"abc");
}

#[test]
fn unchanged_etag_is_sent_as_if_range_and_playback_completes() {
    let (url, server) = serve(vec![
        segment("0-2/6", "ETag: \"original\"\r\n", "abc"),
        segment("3-5/6", "ETag: \"original\"\r\n", "def"),
    ]);
    let mut stream = service().open_seekable(&url, &Default::default()).unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    let requests = server.join().unwrap();
    assert_eq!(bytes, b"abcdef");
    assert!(requests[1].to_ascii_lowercase().contains("\r\nif-range: \"original\"\r\n"));
}

#[test]
fn weak_etag_does_not_turn_last_modified_into_a_strong_validator() {
    let headers = "ETag: W/\"weak\"\r\nLast-Modified: Sun, 25 Oct 2026 00:00:00 GMT\r\n";
    let (url, server) =
        serve(vec![segment("0-2/6", headers, "abc"), segment("3-5/6", headers, "def")]);
    let mut stream = service().open_seekable(&url, &Default::default()).unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    let requests = server.join().unwrap();
    assert_eq!(bytes, b"abcdef");
    assert!(!requests[1].to_ascii_lowercase().contains("\r\nif-range:"));
}

#[test]
fn unknown_partial_total_is_not_treated_as_a_complete_recording() {
    let (url, server) = serve(vec![segment("0-2/*", "", "abc")]);
    let opened = service().open_seekable(&url, &Default::default());
    server.join().unwrap();
    assert!(opened.is_err(), "cannot prove that a bounded partial body is the whole recording");
}

#[test]
fn an_unknown_length_full_response_remains_a_valid_live_stream() {
    let (url, server) = serve(vec!["HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n0\r\n\r\n".into()]);
    let mut stream = service().open_seekable(&url, &Default::default()).unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    server.join().unwrap();
    assert_eq!(bytes, b"abc");
}

#[test]
fn compressed_range_response_is_rejected_before_decoding() {
    let (url, server) = serve(vec![segment("0-2/3", "Content-Encoding: gzip\r\n", "abc")]);
    let opened = service().open_seekable(&url, &Default::default());
    server.join().unwrap();
    assert!(opened.is_err(), "encoded byte offsets cannot resume decoded media bytes");
}
