//! Local HTTP fixtures for native provider and media tests.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

pub(crate) fn bind() -> (String, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    (format!("http://{}", listener.local_addr().unwrap()), listener)
}

pub(crate) fn accept_request(listener: &TcpListener) -> (TcpStream, String, Vec<u8>) {
    let until = Instant::now() + Duration::from_secs(5);
    let (mut socket, _) = loop {
        match listener.accept() {
            Ok(conn) => break conn,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                assert!(Instant::now() < until, "mock did not receive its request");
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) => panic!("accept: {e}"),
        }
    };
    socket.set_nonblocking(false).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut req = Vec::new();
    let mut buf = [0u8; 4096];
    let (end, len) = loop {
        let n = socket.read(&mut buf).unwrap();
        assert!(n > 0 && req.len() < 65_536);
        req.extend_from_slice(&buf[..n]);
        if let Some(pos) = req.windows(4).position(|w| w == b"\r\n\r\n") {
            let size = std::str::from_utf8(&req[..pos])
                .unwrap()
                .lines()
                .filter_map(|l| l.split_once(':'))
                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                .map_or(0, |(_, v)| v.trim().parse::<usize>().unwrap());
            break (pos + 4, size);
        }
    };
    while req.len() < end + len {
        let n = socket.read(&mut buf).unwrap();
        assert!(n > 0 && req.len() < 65_536);
        req.extend_from_slice(&buf[..n]);
    }
    let body = req[end..end + len].to_vec();
    req.truncate(end);
    (socket, String::from_utf8(req).unwrap(), body)
}

pub(crate) fn serve_raw(
    count: usize,
    mut respond: impl FnMut(&str, &str, &[u8]) -> Vec<u8> + Send + 'static,
) -> (String, JoinHandle<()>) {
    let (base, listener) = bind();
    let origin = base.clone();
    let task = std::thread::spawn(move || {
        for _ in 0..count {
            let (mut socket, head, body) = accept_request(&listener);
            let raw = respond(&origin, &head, &body);
            let _ = socket.write_all(&raw);
        }
    });
    (base, task)
}

pub(crate) fn serve(
    count: usize,
    mut respond: impl FnMut(&str, &str, Value) -> Value + Send + 'static,
) -> (String, JoinHandle<()>) {
    serve_raw(count, move |origin, head, raw| {
        let req = if raw.is_empty() { Value::Null } else { serde_json::from_slice(raw).unwrap() };
        let out = serde_json::to_vec(&respond(origin, head, req)).unwrap();
        let mut resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            out.len()
        )
        .into_bytes();
        resp.extend_from_slice(&out);
        resp
    })
}
