//! Media continuation, ETag/Content-Range integrity, and redirect verification.

use std::collections::BTreeMap;
use std::io::{self, ErrorKind, Read, Write};
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
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "expected media request missing");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let (mut req, mut buf) = (Vec::new(), [0u8; 1024]);
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut buf).unwrap();
                assert!(n > 0 && req.len() < 16_384);
                req.extend_from_slice(&buf[..n]);
            }
            requests.push(String::from_utf8(req).unwrap());
            let _ = socket.write_all(response.as_bytes());
        }
        requests
    });
    (url, worker)
}

fn segment(range: &str, headers: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {range}\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{body}",
        body.len()
    )
}

fn redirect(status: &str, location: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

fn whole(body: &str) -> String {
    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

fn fault(status: &str) -> String {
    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
}

fn service() -> Service {
    let mut cfg = Config::default();
    cfg.allow_private_networks = true;
    Service::new(cfg)
}

fn read_all(url: &str, headers: &BTreeMap<String, String>) -> (io::Result<usize>, Vec<u8>) {
    let mut stream = service().open_seekable(url, headers).unwrap();
    let mut bytes = Vec::new();
    let res = stream.read_to_end(&mut bytes);
    (res, bytes)
}

#[test]
fn declared_slice_length_must_match_content_length() {
    let (url, server) = serve(vec![segment("0-1/6", "", "abc")]);
    let opened = service().open_seekable(&url, &BTreeMap::new());
    server.join().unwrap();
    assert!(opened.is_err());
}

#[test]
fn chunked_body_cannot_deliver_bytes_past_its_declared_range() {
    let (url, server) = serve(vec![
        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-2/3\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nabcXY\r\n0\r\n\r\n".into(),
    ]);
    let (res, bytes) = read_all(&url, &BTreeMap::new());
    server.join().unwrap();
    assert!(res.is_err() && bytes == b"abc");
}

#[test]
fn same_size_replacement_is_rejected_before_its_audio_is_exposed() {
    let (url, server) = serve(vec![
        segment("0-2/6", "ETag: \"first\"\r\n", "abc"),
        segment("3-5/6", "ETag: \"replacement\"\r\n", "XYZ"),
    ]);
    let (res, bytes) = read_all(&url, &BTreeMap::new());
    server.join().unwrap();
    assert!(res.is_err() && bytes == b"abc");
}

#[test]
fn unchanged_etag_is_sent_as_if_range_and_playback_completes() {
    let (url, server) = serve(vec![
        segment("0-2/6", "ETag: \"original\"\r\n", "abc"),
        segment("3-5/6", "ETag: \"original\"\r\n", "def"),
    ]);
    let (res, bytes) = read_all(&url, &BTreeMap::new());
    let requests = server.join().unwrap();
    res.unwrap();
    assert_eq!(bytes, b"abcdef");
    assert!(requests[1].to_ascii_lowercase().contains("\r\nif-range: \"original\"\r\n"));
}

#[test]
fn weak_etag_does_not_turn_last_modified_into_a_strong_validator() {
    let headers = "ETag: W/\"weak\"\r\nLast-Modified: Sun, 25 Oct 2026 00:00:00 GMT\r\n";
    let (url, server) =
        serve(vec![segment("0-2/6", headers, "abc"), segment("3-5/6", headers, "def")]);
    let (res, bytes) = read_all(&url, &BTreeMap::new());
    let requests = server.join().unwrap();
    res.unwrap();
    assert_eq!(bytes, b"abcdef");
    assert!(!requests[1].to_ascii_lowercase().contains("\r\nif-range:"));
}

#[test]
fn unknown_partial_total_is_not_treated_as_a_complete_recording() {
    let (url, server) = serve(vec![segment("0-2/*", "", "abc")]);
    let opened = service().open_seekable(&url, &BTreeMap::new());
    server.join().unwrap();
    assert!(opened.is_err());
}

#[test]
fn an_unknown_length_full_response_remains_a_valid_live_stream() {
    let (url, server) = serve(vec![
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n0\r\n\r\n".into(),
    ]);
    let (res, bytes) = read_all(&url, &BTreeMap::new());
    server.join().unwrap();
    res.unwrap();
    assert_eq!(bytes, b"abc");
}

#[test]
fn compressed_range_response_is_rejected_before_decoding() {
    let (url, server) = serve(vec![segment("0-2/3", "Content-Encoding: gzip\r\n", "abc")]);
    let opened = service().open_seekable(&url, &BTreeMap::new());
    server.join().unwrap();
    assert!(opened.is_err());
}

#[test]
fn a_redirect_to_the_real_file_is_followed() {
    let (url, server) = serve(vec![redirect("302 Found", "/real/file.mp3"), whole("abc")]);
    let (res, bytes) = read_all(&url, &BTreeMap::new());
    let requests = server.join().unwrap();
    res.unwrap();
    assert_eq!(bytes, b"abc");
    assert!(requests[1].starts_with("GET /real/file.mp3 "), "{}", requests[1]);
}

#[test]
fn every_redirect_status_is_followed_and_a_loop_is_not() {
    for status in [
        "301 Moved Permanently",
        "303 See Other",
        "307 Temporary Redirect",
        "308 Permanent Redirect",
    ] {
        let (url, server) = serve(vec![redirect(status, "/next"), whole("ok")]);
        let (res, bytes) = read_all(&url, &BTreeMap::new());
        server.join().unwrap();
        res.unwrap();
        assert_eq!(bytes, b"ok", "{status}");
    }
    let (url, server) = serve((0..6).map(|_| redirect("302 Found", "/again")).collect());
    let opened = service().open_seekable(&url, &BTreeMap::new());
    server.join().unwrap();
    assert!(opened.is_err());
}

#[test]
fn a_redirect_with_nowhere_to_go_is_the_original_error() {
    let (url, server) =
        serve(vec!["HTTP/1.1 302 Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()]);
    let error = service().open_seekable(&url, &BTreeMap::new()).err().unwrap();
    server.join().unwrap();
    assert!(error.to_string().contains("302"), "{error}");
}

#[test]
fn provider_headers_do_not_follow_a_redirect_to_another_host() {
    let (target, target_server) = serve(vec![whole("abc")]);
    let elsewhere = target.replace("127.0.0.1", "localhost");
    let (url, server) = serve(vec![redirect("302 Found", &elsewhere)]);
    let headers = BTreeMap::from([("Authorization".into(), "Bearer secret".into())]);
    let (res, bytes) = read_all(&url, &headers);
    let (first, second) = (server.join().unwrap(), target_server.join().unwrap());
    res.unwrap();
    assert_eq!(bytes, b"abc");
    assert!(first[0].to_ascii_lowercase().contains("authorization: bearer secret"));
    assert!(!second[0].to_ascii_lowercase().contains("authorization"), "{}", second[0]);
}

#[test]
fn a_server_fault_on_one_continuation_does_not_end_the_track() {
    let (url, server) = serve(vec![
        segment("0-2/6", "", "abc"),
        fault("503 Service Unavailable"),
        segment("3-5/6", "", "def"),
    ]);
    let (res, bytes) = read_all(&url, &BTreeMap::new());
    server.join().unwrap();
    res.unwrap();
    assert_eq!(bytes, b"abcdef");
}

#[test]
fn a_continuation_that_keeps_failing_ends_the_stream_after_the_retry_budget() {
    let mut responses = vec![segment("0-2/6", "", "abc")];
    responses.extend((0..4).map(|_| fault("503 Service Unavailable")));
    let (url, server) = serve(responses);
    let (res, bytes) = read_all(&url, &BTreeMap::new());
    server.join().unwrap();
    assert!(res.is_err() && bytes == b"abc");
}

#[test]
fn a_continuation_for_an_expired_url_is_not_retried() {
    let (url, server) = serve(vec![segment("0-2/6", "", "abc"), fault("404 Not Found")]);
    let (res, _) = read_all(&url, &BTreeMap::new());
    server.join().unwrap();
    assert!(res.is_err());
}
