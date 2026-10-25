//! HTTP media bodies, validated byte ranges and conditional continuation.

use std::io::{self, Read};
use std::time::Duration;

use crate::{deadline, host_matches, http_url, netguard, SourceError};

/// Google video CDNs reject open-ended or large byte ranges for otherwise
/// playable URLs. Keep their requests at the verified 128 KiB chunk size.
const GOOGLEVIDEO_CHUNK_BYTES: i64 = 128 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MediaRange {
    OpenEnded,
    Chunked,
}

impl MediaRange {
    pub(super) fn for_url(url: &str) -> Self {
        if http_url(url).is_some_and(|url| {
            url.host_str().is_some_and(|host| host_matches(host, "googlevideo.com"))
        }) {
            Self::Chunked
        } else {
            Self::OpenEnded
        }
    }

    fn header(self, offset: i64) -> String {
        match self {
            Self::OpenEnded => format!("bytes={offset}-"),
            Self::Chunked => {
                let end = offset.saturating_add(GOOGLEVIDEO_CHUNK_BYTES - 1);
                format!("bytes={offset}-{end}")
            }
        }
    }
}

/// Makes one request and returns its body, the length the server declared, and
/// whether the range was honoured.
pub(super) fn request(
    agent: &ureq::Agent,
    url: &str,
    headers: &std::collections::BTreeMap<String, String>,
    offset: Option<i64>,
) -> Result<(Box<dyn Read + Send>, i64, bool), SourceError> {
    request_with_range(agent, url, headers, offset, MediaRange::for_url(url))
}

fn request_with_range(
    agent: &ureq::Agent,
    url: &str,
    headers: &std::collections::BTreeMap<String, String>,
    offset: Option<i64>,
    range: MediaRange,
) -> Result<(Box<dyn Read + Send>, i64, bool), SourceError> {
    let response = request_media(agent, url, headers, offset, range, None)?;
    Ok((response.body, response.size, response.partial))
}

struct Response {
    body: Box<dyn Read + Send>,
    size: i64,
    partial: bool,
    validator: Option<String>,
}

fn request_media(
    agent: &ureq::Agent,
    url: &str,
    headers: &std::collections::BTreeMap<String, String>,
    offset: Option<i64>,
    range: MediaRange,
    validator: Option<&str>,
) -> Result<Response, SourceError> {
    netguard::validate_url(url)?;
    // Ureq appends headers instead of replacing them. Select the provider's
    // identity once so CDN requests cannot carry both Rustlink and client UAs.
    let user_agent = headers
        .iter()
        .find_map(|(name, value)| {
            (name.eq_ignore_ascii_case("User-Agent")
                && !value.trim().is_empty()
                && !value.contains(['\r', '\n']))
            .then_some(value.as_str())
        })
        .unwrap_or(USER_AGENT);
    let mut builder = agent.get(url).header("User-Agent", user_agent);
    for (name, value) in headers {
        // A header carrying a line break would let a source inject a second
        // request, so the pair is dropped rather than escaped.
        if name.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
            continue;
        }
        if name.eq_ignore_ascii_case("User-Agent") {
            continue;
        }
        // Playback owns the requested offset. Provider headers must not pin a
        // resumed request to an earlier range, regardless of header casing.
        if offset.is_some()
            && (name.eq_ignore_ascii_case("Range")
                || name.eq_ignore_ascii_case("Accept-Encoding")
                || name.eq_ignore_ascii_case("If-Range"))
        {
            continue;
        }
        builder = builder.header(name, value);
    }
    if let Some(offset) = offset {
        if offset < 0 {
            return Err(SourceError::Url("media byte offset cannot be negative".into()));
        }
        // A resume offset counts media bytes delivered to the decoder. Transfer
        // compression would make those differ from HTTP representation offsets.
        builder =
            builder.header("Range", &range.header(offset)).header("Accept-Encoding", "identity");
        if let Some(validator) = validator {
            builder = builder.header("If-Range", validator);
        }
    }
    let budget = deadline::timeout(Duration::from_secs(30))?;
    let config = builder.config().http_status_as_error(false);
    // Range requests become long-lived playback bodies. Their connection and
    // headers are bounded, while netguard's transport enforces a fresh idle
    // timeout for each read. A whole-body deadline would cut off long tracks.
    let config = if offset.is_some() {
        config
            .timeout_global(None)
            .timeout_per_call(None)
            .timeout_recv_body(None)
            .timeout_resolve(Some(budget))
            .timeout_connect(Some(budget.min(Duration::from_secs(10))))
            .timeout_send_request(Some(budget))
            .timeout_recv_response(Some(budget))
    } else {
        config.timeout_global(Some(budget))
    };
    let response = config
        .build()
        .call()
        .map_err(|error| SourceError::Http(format!("{url} could not be reached: {error}")))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(SourceError::Media { status });
    }
    let partial = status == 206;
    if offset.is_some_and(|offset| offset > 0) && !partial {
        return Err(SourceError::Http(
            "media server ignored the requested resume byte range".into(),
        ));
    }
    if offset.is_some() && range == MediaRange::Chunked && !partial {
        return Err(SourceError::Http("bounded media ranges require a 206 response".into()));
    }
    let header = |name: &str| response.headers().get(name).and_then(|value| value.to_str().ok());
    if offset.is_some()
        && header("Content-Encoding").is_some_and(|value| !value.eq_ignore_ascii_case("identity"))
    {
        return Err(SourceError::Http("media range response used content encoding".into()));
    }
    if let Some(validator) = validator {
        if header("ETag").is_some_and(|value| value != validator) {
            return Err(SourceError::Http("media representation changed while resuming".into()));
        }
    }
    // Weak tags and unqualified modification dates cannot prove byte-for-byte
    // equality. Only use a strong entity tag for conditional continuation.
    let next_validator = header("ETag")
        .filter(|value| value.starts_with('"') && value.ends_with('"'))
        .map(str::to_owned);
    let mut size = response
        .headers()
        .get("Content-Length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(-1);
    // A partial response describes the slice it returned, so the whole length
    // only comes from the range it states.
    let mut slice_length = None;
    if partial {
        let content_range = response
            .headers()
            .get("Content-Range")
            .and_then(|value| value.to_str().ok())
            .and_then(parse_content_range)
            .ok_or_else(|| {
                SourceError::Http("media server returned an invalid Content-Range".into())
            })?;
        if content_range.0 != offset.unwrap_or(0) {
            return Err(SourceError::Http(
                "media server returned the wrong resume byte offset".into(),
            ));
        }
        if offset.is_some() && content_range.2.is_none() {
            return Err(SourceError::Http(
                "partial media responses require a known total in Content-Range".into(),
            ));
        }
        let length = content_range.1 - content_range.0 + 1;
        if size >= 0 && size != length {
            return Err(SourceError::Http(
                "Content-Length disagrees with media Content-Range".into(),
            ));
        }
        if offset.is_some() && range == MediaRange::Chunked && length > GOOGLEVIDEO_CHUNK_BYTES {
            return Err(SourceError::Http("media server exceeded the requested byte range".into()));
        }
        slice_length = Some(length as u64);
        size = content_range.2.unwrap_or(-1);
    }
    let reader = response.into_body().into_reader();
    let body: Box<dyn Read + Send> = match slice_length {
        Some(remaining) => Box::new(SliceReader { reader, remaining }),
        None => Box::new(reader),
    };
    Ok(Response { body, size, partial, validator: next_validator })
}

/// A chunked transfer has no Content-Length to enforce the declared slice.
/// Never expose bytes beyond that slice, and distinguish a short body from EOF.
struct SliceReader<R> {
    reader: R,
    remaining: u64,
}

impl<R: Read> Read for SliceReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            return match self.reader.read(&mut [0])? {
                0 => Ok(0),
                _ => Err(io::Error::new(io::ErrorKind::InvalidData, "media exceeds Content-Range")),
            };
        }
        let count = self.remaining.min(buffer.len() as u64) as usize;
        let read = self.reader.read(&mut buffer[..count])?;
        if read == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "media range body truncated"));
        }
        self.remaining -= read as u64;
        Ok(read)
    }
}

/// The user agent the node makes its own requests with.
const USER_AGENT: &str = concat!("Rustlink/", env!("CARGO_PKG_VERSION"));

/// Consecutive continuation failures allowed without reading new media bytes.
const RESUME_ATTEMPTS: usize = 3;

/// Backoff for a stalled stream; normal chunk boundaries continue immediately.
const RESUME_DELAY: Duration = Duration::from_millis(250);

/// Validate a satisfied byte range and return its starting offset and total.
/// A range whose total is unknown must not use its slice length as file size.
fn parse_content_range(value: &str) -> Option<(i64, i64, Option<i64>)> {
    let (unit, range) = value.trim().split_once(' ')?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let (range, total) = range.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end): (i64, i64) = (start.parse().ok()?, end.parse().ok()?);
    if start < 0 || end < start || end.checked_sub(start)?.checked_add(1).is_none() {
        return None;
    }
    let total = if total == "*" {
        None
    } else {
        let total: i64 = total.parse().ok()?;
        if total <= end {
            return None;
        }
        Some(total)
    };
    Some((start, end, total))
}

/// A playback stream that keeps flowing when the connection behind it drops.
///
/// The decoder reads the media through this, so a dropped connection would
/// otherwise end the track. Both kinds of source the node streams from answer
/// byte ranges, so a read is continued by asking for the byte after the last one
/// delivered, and the decoder never notices.
pub struct MediaBody {
    agent: ureq::Agent,
    url: String,
    headers: std::collections::BTreeMap<String, String>,
    range: MediaRange,
    reader: Option<Box<dyn Read + Send>>,
    offset: i64,
    response_start: i64,
    size: i64,
    attempts: usize,
    validator: Option<String>,
}

impl MediaBody {
    pub(super) fn open(
        agent: ureq::Agent,
        url: &str,
        headers: &std::collections::BTreeMap<String, String>,
        range: MediaRange,
    ) -> Result<Self, SourceError> {
        let response = request_media(&agent, url, headers, Some(0), range, None)?;
        Ok(Self {
            agent,
            url: url.to_owned(),
            headers: headers.clone(),
            range,
            reader: Some(response.body),
            offset: 0,
            response_start: 0,
            size: response.size,
            attempts: RESUME_ATTEMPTS,
            validator: response.validator,
        })
    }

    /// The number of bytes read so far.
    pub fn position(&self) -> i64 {
        self.offset
    }

    /// The whole length, or -1 when the source did not say.
    pub fn len(&self) -> i64 {
        self.size
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Asks for the rest of the stream, starting at the byte after the last one
    /// delivered.
    fn resume(&mut self) -> Result<(), SourceError> {
        let response = request_media(
            &self.agent,
            &self.url,
            &self.headers,
            Some(self.offset),
            self.range,
            self.validator.as_deref(),
        )?;
        let size = response.size;
        if self.size >= 0 && size >= 0 && self.size != size {
            return Err(SourceError::Http("media size changed while resuming the stream".into()));
        }
        if self.size < 0 {
            self.size = size;
        }
        self.reader = Some(response.body);
        self.response_start = self.offset;
        Ok(())
    }
}

impl Read for MediaBody {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            let Some(reader) = self.reader.as_mut() else {
                return Ok(0);
            };
            let (error, stalled) = match reader.read(buffer) {
                Ok(0) if self.size < 0 || self.offset >= self.size => return Ok(0),
                Ok(0) => (
                    io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("media ended at {} of {} bytes", self.offset, self.size),
                    ),
                    self.offset == self.response_start,
                ),
                Ok(read) => {
                    self.offset += read as i64;
                    // The limit is for consecutive failures without progress.
                    // A server may legitimately answer an open range in many
                    // successful segments, each requiring another request.
                    self.attempts = RESUME_ATTEMPTS;
                    return Ok(read);
                }
                // Retrying corrupt framing could silently splice later bytes
                // onto an already invalid response. End this stream instead.
                Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                    self.reader = None;
                    return Err(error);
                }
                Err(error) => (error, true),
            };
            if stalled {
                if self.attempts == 0 {
                    return Err(error);
                }
                self.attempts -= 1;
                std::thread::sleep(RESUME_DELAY);
            }
            if let Err(resume_error) = self.resume() {
                return Err(io::Error::other(resume_error));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Service;
    use rustlink_config::Config;
    use std::time::Instant;

    fn ranged_fixture(status: &str, content_range: Option<&str>) -> Result<i64, SourceError> {
        ranged_fixture_with_strategy(status, content_range, MediaRange::OpenEnded)
    }

    fn ranged_fixture_with_strategy(
        status: &str,
        content_range: Option<&str>,
        range: MediaRange,
    ) -> Result<i64, SourceError> {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/audio", listener.local_addr().unwrap());
        let content_range =
            content_range.map(|value| format!("Content-Range: {value}\r\n")).unwrap_or_default();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: 3\r\n{content_range}Connection: close\r\n\r\nabc"
        );
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut received = Vec::new();
            let mut buffer = [0; 1024];
            while !received.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buffer).unwrap();
                assert!(read > 0);
                received.extend_from_slice(&buffer[..read]);
            }
            let headers = std::str::from_utf8(&received).unwrap().to_ascii_lowercase();
            let expected_range = format!("range: {}", range.header(5));
            assert_eq!(
                headers.lines().filter(|line| line.starts_with("range:")).collect::<Vec<_>>(),
                [expected_range.as_str()]
            );
            assert_eq!(
                headers
                    .lines()
                    .filter(|line| line.starts_with("accept-encoding:"))
                    .collect::<Vec<_>>(),
                ["accept-encoding: identity"]
            );
            socket.write_all(response.as_bytes()).unwrap();
        });
        let headers = std::collections::BTreeMap::from([
            ("Range".into(), "bytes=0-".into()),
            ("rAnGe".into(), "bytes=1-".into()),
            ("Accept-Encoding".into(), "gzip".into()),
            ("aCcEpT-EnCoDiNg".into(), "br".into()),
        ]);
        let result = request_with_range(&netguard::agent(true), &url, &headers, Some(5), range)
            .map(|(_, size, _)| size);
        server.join().unwrap();
        result
    }

    #[test]
    fn resumed_media_requires_the_exact_requested_byte_range() {
        assert!(ranged_fixture("200 OK", None).unwrap_err().to_string().contains("ignored"));
        assert!(ranged_fixture("206 Partial Content", Some("bytes 0-2/8"))
            .unwrap_err()
            .to_string()
            .contains("wrong"));
        assert!(ranged_fixture("206 Partial Content", None)
            .unwrap_err()
            .to_string()
            .contains("invalid"));
        assert_eq!(ranged_fixture("206 Partial Content", Some("bytes 5-7/8")).unwrap(), 8);
        assert!(ranged_fixture("206 Partial Content", Some("bytes 5-7/*"))
            .unwrap_err()
            .to_string()
            .contains("known total"));
    }

    fn segmented_fixture(
        chunks: Vec<Vec<u8>>,
        total: usize,
    ) -> (MediaBody, std::thread::JoinHandle<()>) {
        segmented_fixture_with_strategy(chunks, total, MediaRange::OpenEnded)
    }

    fn segmented_fixture_with_strategy(
        chunks: Vec<Vec<u8>>,
        total: usize,
        range: MediaRange,
    ) -> (MediaBody, std::thread::JoinHandle<()>) {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/audio", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut start = 0;
            for chunk in chunks {
                let until = Instant::now() + Duration::from_secs(5);
                let (mut socket, _) = loop {
                    match listener.accept() {
                        Ok(socket) => break socket,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < until, "segment was never requested");
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(error) => panic!("accept: {error}"),
                    }
                };
                socket.set_nonblocking(false).unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut received = Vec::new();
                let mut buffer = [0; 1024];
                while !received.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = socket.read(&mut buffer).unwrap();
                    assert!(read > 0);
                    received.extend_from_slice(&buffer[..read]);
                }
                let headers = std::str::from_utf8(&received).unwrap().to_ascii_lowercase();
                assert!(headers.contains(&format!("\r\nrange: {}\r\n", range.header(start as i64))));
                assert!(headers.contains("\r\naccept-encoding: identity\r\n"));
                let end = start + chunk.len().saturating_sub(1);
                write!(socket, "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{total}\r\nConnection: close\r\n\r\n", chunk.len().max(1)).unwrap();
                socket.write_all(&chunk).unwrap();
                start += chunk.len();
            }
        });
        let service = Service::new(Config { allow_private_networks: true, ..Config::default() });
        let headers = std::collections::BTreeMap::from([
            ("Range".into(), "bytes=0-999".into()),
            ("rAnGe".into(), "bytes=0-".into()),
        ]);
        let body = service.media_body(&url, &headers, range).unwrap();
        (body, server)
    }

    #[test]
    fn googlevideo_ranges_are_bounded_and_recomputed_from_each_offset() {
        for url in [
            "https://googlevideo.com/videoplayback",
            "https://rr1---sn-example.googlevideo.com/videoplayback?id=video",
            "https://RR1.GOOGLEVIDEO.COM:443/videoplayback",
        ] {
            assert_eq!(MediaRange::for_url(url), MediaRange::Chunked, "{url}");
        }
        for url in [
            "https://notgooglevideo.com/videoplayback",
            "https://googlevideo.com.evil.example/videoplayback",
            "https://googlevideo.com@evil.example/videoplayback",
            "https://evil.example/?host=googlevideo.com",
            "ftp://googlevideo.com/videoplayback",
            "https://example.com/audio.mp3",
        ] {
            assert_eq!(MediaRange::for_url(url), MediaRange::OpenEnded, "{url}");
        }
        assert_eq!(MediaRange::Chunked.header(0), "bytes=0-131071");
        assert_eq!(MediaRange::Chunked.header(131_072), "bytes=131072-262143");
        assert_eq!(MediaRange::Chunked.header(123), "bytes=123-131194");
        assert_eq!(MediaRange::Chunked.header(i64::MAX), format!("bytes={0}-{0}", i64::MAX));
        assert_eq!(MediaRange::OpenEnded.header(131_072), "bytes=131072-");
    }

    #[test]
    fn bounded_media_ranges_reject_unknown_totals_instead_of_truncating_audio() {
        let result = ranged_fixture_with_strategy(
            "206 Partial Content",
            Some("bytes 5-7/*"),
            MediaRange::Chunked,
        );
        assert!(result.unwrap_err().to_string().contains("known total"));
        assert_eq!(
            ranged_fixture_with_strategy(
                "206 Partial Content",
                Some("bytes 5-7/8"),
                MediaRange::Chunked,
            )
            .unwrap(),
            8
        );
    }

    #[test]
    fn bounded_media_chunks_continue_without_retry_delays_or_budget_loss() {
        let chunks = vec![
            vec![1; GOOGLEVIDEO_CHUNK_BYTES as usize],
            vec![2; GOOGLEVIDEO_CHUNK_BYTES as usize],
            vec![3; GOOGLEVIDEO_CHUNK_BYTES as usize],
            vec![4; GOOGLEVIDEO_CHUNK_BYTES as usize],
            vec![5; 17],
        ];
        let expected: Vec<u8> = chunks.iter().flatten().copied().collect();
        let (mut body, server) =
            segmented_fixture_with_strategy(chunks, expected.len(), MediaRange::Chunked);
        let start = Instant::now();
        let mut received = Vec::new();
        let result = body.read_to_end(&mut received);
        let elapsed = start.elapsed();
        server.join().unwrap();
        result.unwrap();
        assert_eq!(received, expected);
        assert_eq!(body.position(), expected.len() as i64);
        // Four ordinary chunk boundaries must not each incur the 250 ms
        // failure backoff. Allow ample slack for the local TCP fixture itself.
        assert!(elapsed < RESUME_DELAY * 3, "normal chunks incurred retry delays: {elapsed:?}");
    }

    #[test]
    fn media_reads_more_segments_than_the_failure_retry_limit() {
        let (mut body, server) = segmented_fixture(
            [b"ab", b"cd", b"ef", b"gh", b"ij"].iter().map(|chunk| chunk.to_vec()).collect(),
            10,
        );
        let mut received = Vec::new();
        let result = body.read_to_end(&mut received);
        server.join().unwrap();
        result.unwrap();
        assert_eq!(received, b"abcdefghij");
    }

    #[test]
    fn media_stops_after_bounded_retries_without_progress() {
        let (mut body, server) = segmented_fixture(vec![Vec::new(); RESUME_ATTEMPTS + 1], 10);
        let mut received = Vec::new();
        let result = body.read_to_end(&mut received);
        server.join().unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert!(received.is_empty());
    }

    #[test]
    fn playback_body_outlives_the_resolution_deadline() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/audio", listener.local_addr().unwrap());
        let (release, released) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut received = Vec::new();
            let mut buffer = [0; 1024];
            while !received.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buffer).unwrap();
                assert!(read > 0);
                received.extend_from_slice(&buffer[..read]);
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\n")
                .unwrap();
            released.recv_timeout(Duration::from_secs(2)).unwrap();
            socket.write_all(b"abc").unwrap();
        });
        let agent = netguard::agent(true);
        let mut body = {
            let _scope = deadline::enter(Instant::now() + Duration::from_millis(100));
            request(&agent, &url, &Default::default(), Some(0)).unwrap().0
        };
        std::thread::sleep(Duration::from_millis(150));
        release.send(()).unwrap();
        let mut received = Vec::new();
        body.read_to_end(&mut received).unwrap();
        server.join().unwrap();
        assert_eq!(received, b"abc");
    }

    #[test]
    fn a_total_is_read_out_of_a_content_range() {
        assert_eq!(parse_content_range("bytes 0-99/1234"), Some((0, 99, Some(1234))));
        assert_eq!(parse_content_range("bytes */1234"), None);
        assert_eq!(parse_content_range("nonsense"), None);
        assert_eq!(parse_content_range("bytes 100-199/*"), Some((100, 199, None)));
        assert_eq!(parse_content_range("bytes 100-99/1234"), None);
        assert_eq!(parse_content_range("bytes 0-99/99"), None);
        assert_eq!(parse_content_range("items 0-99/1234"), None);
    }
}
