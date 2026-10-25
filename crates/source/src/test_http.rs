//! Small bounded local fixtures for native provider integration tests.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

pub(super) fn serve(
    count: usize,
    mut respond: impl FnMut(&str, &str, Value) -> Value + Send + 'static,
) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let origin = base.clone();
    let task = std::thread::spawn(move || {
        for _ in 0..count {
            let until = Instant::now() + Duration::from_secs(5);
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < until, "mock did not receive its request");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            // Windows accepted sockets inherit the listener's nonblocking mode.
            socket.set_nonblocking(false).unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            let (end, size) = loop {
                let read = socket.read(&mut buffer).unwrap();
                assert!(read > 0 && request.len() < 65_536);
                request.extend_from_slice(&buffer[..read]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&request[..end]).unwrap();
                    let size = headers
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .map_or(0, |(_, value)| value.trim().parse::<usize>().unwrap());
                    break (end + 4, size);
                }
            };
            while request.len() < end + size {
                let read = socket.read(&mut buffer).unwrap();
                assert!(read > 0 && request.len() < 65_536);
                request.extend_from_slice(&buffer[..read]);
            }
            let head = std::str::from_utf8(&request[..end]).unwrap();
            let body = if size == 0 {
                Value::Null
            } else {
                serde_json::from_slice(&request[end..end + size]).unwrap()
            };
            let body = serde_json::to_vec(&respond(&origin, head, body)).unwrap();
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            socket.write_all(&body).unwrap();
        }
    });
    (base, task)
}
