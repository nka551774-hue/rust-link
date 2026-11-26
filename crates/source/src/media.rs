//! HTTP media bodies, validated byte ranges, conditional continuation, and prefetching.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{self, Read};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::time::Duration;

use crate::{deadline, host_matches, http_url, netguard, SourceError};

const GOOGLEVIDEO_CHUNK_BYTES: i64 = 512 * 1024;
const MAX_REDIRECTS: usize = 5;
const UNREACHABLE: &str = "could not be reached";
const USER_AGENT: &str = concat!("Rustlink/", env!("CARGO_PKG_VERSION"));
const RESUME_ATTEMPTS: usize = 3;
const RESUME_DELAY: Duration = Duration::from_millis(250);
const PREFETCH_CHUNK_BYTES: usize = 64 * 1024;
const PREFETCH_QUEUE_CHUNKS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MediaRange {
    OpenEnded,
    Chunked,
}

impl MediaRange {
    pub(super) fn for_url(url: &str) -> Self {
        if http_url(url)
            .is_some_and(|u| u.host_str().is_some_and(|h| host_matches(h, "googlevideo.com")))
        {
            Self::Chunked
        } else {
            Self::OpenEnded
        }
    }

    fn header(self, offset: i64) -> String {
        match self {
            Self::OpenEnded => format!("bytes={offset}-"),
            Self::Chunked => {
                format!("bytes={offset}-{}", offset.saturating_add(GOOGLEVIDEO_CHUNK_BYTES - 1))
            }
        }
    }
}

pub(crate) fn request(
    agent: &ureq::Agent,
    url: &str,
    headers: &BTreeMap<String, String>,
    offset: Option<i64>,
) -> Result<(Box<dyn Read + Send>, i64, bool), SourceError> {
    request_with_range(agent, url, headers, offset, MediaRange::for_url(url))
}

fn request_with_range(
    agent: &ureq::Agent,
    url: &str,
    headers: &BTreeMap<String, String>,
    offset: Option<i64>,
    range: MediaRange,
) -> Result<(Box<dyn Read + Send>, i64, bool), SourceError> {
    let r = request_media(agent, url, headers, true, offset, range, None)?;
    Ok((r.body, r.size, r.partial))
}

struct Response {
    body: Box<dyn Read + Send>,
    size: i64,
    partial: bool,
    validator: Option<String>,
    redirected_url: Option<String>,
    provider_headers: bool,
    range: MediaRange,
}

fn unique_header<'a>(
    headers: &'a ureq::http::HeaderMap,
    name: &str,
) -> Result<Option<&'a str>, SourceError> {
    let mut iter = headers.get_all(name).iter();
    match (iter.next(), iter.next()) {
        (None, None) => Ok(None),
        (Some(raw), None) => raw
            .to_str()
            .map(Some)
            .map_err(|_| SourceError::Http(format!("media server returned an invalid {name}"))),
        _ => Err(SourceError::Http(format!("media server returned multiple {name} headers"))),
    }
}

fn send_once(
    agent: &ureq::Agent,
    url: &str,
    headers: &BTreeMap<String, String>,
    provider_headers: bool,
    offset: Option<i64>,
    range: MediaRange,
    validator: Option<&str>,
) -> Result<ureq::http::Response<ureq::Body>, SourceError> {
    netguard::validate_url(url)?;
    let user_agent = headers
        .iter()
        .find_map(|(k, v)| {
            (k.eq_ignore_ascii_case("User-Agent")
                && !v.trim().is_empty()
                && !v.contains(['\r', '\n']))
            .then_some(v.as_str())
        })
        .unwrap_or(USER_AGENT);
    let mut builder = agent.get(url).header("User-Agent", user_agent);
    if provider_headers {
        for (name, value) in headers {
            if name.contains(['\r', '\n'])
                || value.contains(['\r', '\n'])
                || name.eq_ignore_ascii_case("User-Agent")
                || ureq::http::HeaderName::try_from(name.as_str()).is_err()
                || ureq::http::HeaderValue::try_from(value.as_str()).is_err()
                || (offset.is_some()
                    && ["Range", "Accept-Encoding", "If-Range"]
                        .iter()
                        .any(|h| name.eq_ignore_ascii_case(h)))
            {
                continue;
            }
            builder = builder.header(name, value);
        }
    }
    if let Some(offset) = offset {
        if offset < 0 {
            return Err(SourceError::Url("media byte offset cannot be negative".into()));
        }
        builder =
            builder.header("Range", &range.header(offset)).header("Accept-Encoding", "identity");
        if let Some(v) = validator {
            builder = builder.header("If-Range", v);
        }
    }
    let budget = deadline::timeout(Duration::from_secs(30))?;
    let cfg = builder.config().http_status_as_error(false);
    let cfg = if offset.is_some() {
        cfg.timeout_global(None)
            .timeout_per_call(None)
            .timeout_recv_body(None)
            .timeout_resolve(Some(budget))
            .timeout_connect(Some(budget.min(Duration::from_secs(10))))
            .timeout_send_request(Some(budget))
            .timeout_recv_response(Some(budget))
    } else {
        cfg.timeout_global(Some(budget))
    };
    cfg.build().call().map_err(|error| {
        let retryable = matches!(
            error,
            ureq::Error::Io(_)
                | ureq::Error::Timeout(_)
                | ureq::Error::ConnectionFailed
                | ureq::Error::ConnectProxyFailed(_)
                | ureq::Error::Protocol(_)
        );
        SourceError::Http(if retryable {
            format!("{url} {UNREACHABLE}: {error}")
        } else {
            format!("{url} failed: {error}")
        })
    })
}

fn transient(error: &SourceError) -> bool {
    match error {
        SourceError::Media { status } => *status == 429 || *status >= 500,
        SourceError::Http(msg) => msg.contains(UNREACHABLE),
        _ => false,
    }
}

fn request_media(
    agent: &ureq::Agent,
    url: &str,
    headers: &BTreeMap<String, String>,
    mut provider_headers: bool,
    offset: Option<i64>,
    mut range: MediaRange,
    validator: Option<&str>,
) -> Result<Response, SourceError> {
    let mut current = Cow::Borrowed(url);
    let mut hops = 0;
    let response = loop {
        let resp = send_once(agent, &current, headers, provider_headers, offset, range, validator)?;
        let status = resp.status().as_u16();
        if !matches!(status, 301 | 302 | 303 | 307 | 308) {
            break resp;
        }
        hops += 1;
        if hops > MAX_REDIRECTS {
            return Err(SourceError::Http("media URL redirected too many times".into()));
        }
        let Some(location) = unique_header(resp.headers(), "Location")? else {
            return Err(SourceError::Media { status });
        };
        let base = url::Url::parse(&current).map_err(|e| SourceError::Url(e.to_string()))?;
        let next = base
            .join(location)
            .map_err(|_| SourceError::Url("media redirect names an invalid URL".into()))?;
        netguard::validate_url(next.as_str())?;
        if base.scheme() == "https" && next.scheme() != "https" {
            return Err(SourceError::Http("media redirect would step down to plain HTTP".into()));
        }
        if (next.host_str(), next.port_or_known_default())
            != (base.host_str(), base.port_or_known_default())
        {
            provider_headers = false;
        }
        if MediaRange::for_url(next.as_str()) == MediaRange::Chunked {
            range = MediaRange::Chunked;
        }
        current = Cow::Owned(String::from(next));
    };

    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(SourceError::Media { status });
    }
    let partial = status == 206;
    if offset.is_some_and(|o| o > 0) && !partial {
        return Err(SourceError::Http(
            "media server ignored the requested resume byte range".into(),
        ));
    }
    if offset.is_some() && range == MediaRange::Chunked && !partial {
        return Err(SourceError::Http("bounded media ranges require a 206 response".into()));
    }
    let hdrs = response.headers();
    if offset.is_some()
        && hdrs.get_all("Content-Encoding").iter().any(|v| {
            !v.to_str().is_ok_and(|s| {
                s.split(',')
                    .map(str::trim)
                    .all(|enc| enc.is_empty() || enc.eq_ignore_ascii_case("identity"))
            })
        })
    {
        return Err(SourceError::Http("media range response used content encoding".into()));
    }
    let etag = unique_header(hdrs, "ETag")?;
    if validator.is_some_and(|expected| etag != Some(expected)) {
        return Err(SourceError::Http("media representation changed while resuming".into()));
    }
    let next_validator = validator
        .is_none()
        .then(|| {
            etag.filter(|v| {
                v.strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .is_some_and(|s| !s.contains('"'))
            })
            .map(str::to_owned)
        })
        .flatten();

    let mut size = match unique_header(hdrs, "Content-Length")? {
        None => -1,
        Some(raw) => parse_digits_i64(raw.trim()).ok_or_else(|| {
            SourceError::Http("media server returned an invalid Content-Length".into())
        })?,
    };
    let mut slice_length = None;
    if partial {
        let (start, end, total) = unique_header(hdrs, "Content-Range")
            .ok()
            .flatten()
            .and_then(parse_content_range)
            .ok_or_else(|| {
                SourceError::Http("media server returned an invalid Content-Range".into())
            })?;
        if start != offset.unwrap_or(0) {
            return Err(SourceError::Http(
                "media server returned the wrong resume byte offset".into(),
            ));
        }
        if offset.is_some() && total.is_none() {
            return Err(SourceError::Http(
                "partial media responses require a known total in Content-Range".into(),
            ));
        }
        let length = end - start + 1;
        if size >= 0 && size != length {
            return Err(SourceError::Http(
                "Content-Length disagrees with media Content-Range".into(),
            ));
        }
        if offset.is_some() && range == MediaRange::Chunked && length > GOOGLEVIDEO_CHUNK_BYTES {
            return Err(SourceError::Http("media server exceeded the requested byte range".into()));
        }
        slice_length = Some(length as u64);
        size = total.unwrap_or(-1);
    }
    let reader = response.into_body().into_reader();
    let body: Box<dyn Read + Send> = match slice_length {
        Some(remaining) => Box::new(SliceReader { reader, remaining }),
        None => Box::new(reader),
    };
    Ok(Response {
        body,
        size,
        partial,
        validator: next_validator,
        redirected_url: match current {
            Cow::Borrowed(_) => None,
            Cow::Owned(redirected) => Some(redirected),
        },
        provider_headers,
        range,
    })
}

fn read_retry(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match r.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}

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
            return if read_retry(&mut self.reader, &mut [0u8; 1])? == 0 {
                Ok(0)
            } else {
                Err(io::Error::new(io::ErrorKind::InvalidData, "media exceeds Content-Range"))
            };
        }
        let count = self.remaining.min(buffer.len() as u64) as usize;
        let read = read_retry(&mut self.reader, &mut buffer[..count])?;
        if read == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "media range body truncated"));
        }
        self.remaining -= read as u64;
        Ok(read)
    }
}

fn parse_digits_i64(s: &str) -> Option<i64> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok())?
}

fn parse_content_range(value: &str) -> Option<(i64, i64, Option<i64>)> {
    let (unit, range) = value.trim().split_once(' ')?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let (range, total) = range.trim_start().split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end) = (parse_digits_i64(start)?, parse_digits_i64(end)?);
    if start < 0 || end < start || end.checked_sub(start)?.checked_add(1).is_none() {
        return None;
    }
    let total =
        if total == "*" { None } else { Some(parse_digits_i64(total).filter(|&t| t > end)?) };
    Some((start, end, total))
}

/// Playback stream with transparent byte-range continuation across drops and chunk limits.
pub struct MediaBody {
    agent: ureq::Agent,
    origin_url: String,
    url: String,
    headers: BTreeMap<String, String>,
    provider_headers: bool,
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
        url: impl Into<String>,
        headers: BTreeMap<String, String>,
        range: MediaRange,
    ) -> Result<Self, SourceError> {
        let origin_url = url.into();
        let r = request_media(&agent, &origin_url, &headers, true, Some(0), range, None)?;
        let url = r.redirected_url.unwrap_or_else(|| origin_url.clone());
        Ok(Self {
            agent,
            origin_url,
            url,
            headers,
            provider_headers: r.provider_headers,
            range: r.range,
            reader: Some(r.body),
            offset: 0,
            response_start: 0,
            size: r.size,
            attempts: RESUME_ATTEMPTS,
            validator: r.validator,
        })
    }

    pub fn position(&self) -> i64 {
        self.offset
    }

    pub fn len(&self) -> i64 {
        self.size
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    fn resume(&mut self) -> Result<(), SourceError> {
        let mut used_origin = false;
        let response = match request_media(
            &self.agent,
            &self.url,
            &self.headers,
            self.provider_headers,
            Some(self.offset),
            self.range,
            self.validator.as_deref(),
        ) {
            Ok(resp) => resp,
            Err(SourceError::Media { status: 403 | 404 | 410 }) if self.url != self.origin_url => {
                used_origin = true;
                request_media(
                    &self.agent,
                    &self.origin_url,
                    &self.headers,
                    true,
                    Some(self.offset),
                    MediaRange::for_url(&self.origin_url),
                    self.validator.as_deref(),
                )?
            }
            Err(e) => return Err(e),
        };
        if self.size >= 0 && response.size >= 0 && self.size != response.size {
            return Err(SourceError::Http("media size changed while resuming the stream".into()));
        }
        if self.size < 0 {
            self.size = response.size;
        }
        if self.validator.is_none() {
            self.validator = response.validator;
        }
        if let Some(redirected) = response.redirected_url {
            self.url = redirected;
        } else if used_origin {
            self.url.clone_from(&self.origin_url);
        }
        self.provider_headers = response.provider_headers;
        self.range = response.range;
        self.reader = Some(response.body);
        self.response_start = self.offset;
        Ok(())
    }

    fn resume_with_retries(&mut self) -> Result<(), SourceError> {
        loop {
            match self.resume() {
                Ok(()) => return Ok(()),
                Err(e) if transient(&e) && self.attempts > 0 => {
                    self.attempts -= 1;
                    std::thread::sleep(RESUME_DELAY);
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl Read for MediaBody {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            let Some(reader) = self.reader.as_mut() else { return Ok(0) };
            let (read_error, stalled) = match reader.read(buffer) {
                Ok(0) if self.size < 0 || self.offset >= self.size => return Ok(0),
                Ok(0) => (None, self.offset == self.response_start),
                Ok(read) => {
                    self.offset += read as i64;
                    self.attempts = RESUME_ATTEMPTS;
                    return Ok(read);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                    self.reader = None;
                    return Err(e);
                }
                Err(e) => (Some(e), self.offset == self.response_start),
            };
            if stalled {
                if self.attempts == 0 {
                    self.reader = None;
                    return Err(read_error.unwrap_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            format!("media ended at {} of {} bytes", self.offset, self.size),
                        )
                    }));
                }
                self.attempts -= 1;
                std::thread::sleep(RESUME_DELAY);
            }
            if let Err(resume_error) = self.resume_with_retries() {
                self.reader = None;
                return Err(io::Error::other(resume_error));
            }
        }
    }
}

impl Drop for MediaBody {
    fn drop(&mut self) {
        crate::catalog::wipe_string(&mut self.origin_url);
        crate::catalog::wipe_string(&mut self.url);
    }
}

enum PrefetchItem {
    Data(Vec<u8>, usize),
    Error(io::Error),
}

/// 2 MiB zero-`memset` recycling prefetcher decoupling HTTP range continuations from playback.
pub(crate) struct PrefetchReader {
    data_rx: Receiver<PrefetchItem>,
    recycle_tx: SyncSender<Vec<u8>>,
    current: Vec<u8>,
    len: usize,
    pos: usize,
    done: bool,
}

impl PrefetchReader {
    pub(crate) fn boxed(body: MediaBody) -> Box<dyn Read + Send> {
        if body.is_empty() {
            return Box::new(io::empty());
        }
        Self::spawn(body)
    }

    pub(crate) fn spawn<R: Read + Send + 'static>(body: R) -> Box<dyn Read + Send> {
        let (body_tx, body_rx) = sync_channel::<R>(1);
        let (data_tx, data_rx) = sync_channel(PREFETCH_QUEUE_CHUNKS);
        let (recycle_tx, recycle_rx) = sync_channel::<Vec<u8>>(PREFETCH_QUEUE_CHUNKS);
        let spawned =
            std::thread::Builder::new().name("rustlink-prefetch".into()).spawn(move || {
                let Ok(mut body) = body_rx.recv() else { return };
                let mut first_chunk = true;
                loop {
                    let mut chunk =
                        recycle_rx.try_recv().unwrap_or_else(|_| vec![0u8; PREFETCH_CHUNK_BYTES]);
                    if chunk.len() != PREFETCH_CHUNK_BYTES {
                        chunk.resize(PREFETCH_CHUNK_BYTES, 0);
                    }
                    let mut filled = 0usize;
                    let mut pending_err = None;
                    while filled < PREFETCH_CHUNK_BYTES {
                        match body.read(&mut chunk[filled..]) {
                            Ok(0) => break,
                            Ok(read) => {
                                filled += read;
                                if first_chunk {
                                    first_chunk = false;
                                    break;
                                }
                            }
                            Err(e) => {
                                pending_err = Some(e);
                                break;
                            }
                        }
                    }
                    if filled > 0 && data_tx.send(PrefetchItem::Data(chunk, filled)).is_err() {
                        break;
                    }
                    if let Some(e) = pending_err {
                        let _ = data_tx.send(PrefetchItem::Error(e));
                        break;
                    }
                    if filled == 0 {
                        break;
                    }
                }
            });
        match spawned {
            Ok(_) => {
                let _ = body_tx.send(body);
                Box::new(Self {
                    data_rx,
                    recycle_tx,
                    current: Vec::new(),
                    len: 0,
                    pos: 0,
                    done: false,
                })
            }
            Err(_) => Box::new(body),
        }
    }
}

impl Read for PrefetchReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() || self.done {
            return Ok(0);
        }
        if self.pos >= self.len {
            let prev = std::mem::take(&mut self.current);
            if prev.len() == PREFETCH_CHUNK_BYTES {
                let _ = self.recycle_tx.try_send(prev);
            }
            self.len = 0;
            self.pos = 0;
            match self.data_rx.recv() {
                Ok(PrefetchItem::Data(chunk, len)) => {
                    self.current = chunk;
                    self.len = len;
                }
                Ok(PrefetchItem::Error(e)) => {
                    self.done = true;
                    return Err(e);
                }
                Err(_) => {
                    self.done = true;
                    return Ok(0);
                }
            }
        }
        let avail = &self.current[self.pos..self.len];
        let count = avail.len().min(buffer.len());
        buffer[..count].copy_from_slice(&avail[..count]);
        self.pos += count;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_http, Service};
    use rustlink_config::Config;
    use std::io::Write;
    use std::time::Instant;

    fn dummy_body(r: impl Read + Send + 'static, size: i64) -> MediaBody {
        MediaBody {
            agent: netguard::agent(true),
            origin_url: "http://127.0.0.1:1/unused".into(),
            url: "http://127.0.0.1:1/unused".into(),
            headers: BTreeMap::new(),
            provider_headers: false,
            range: MediaRange::OpenEnded,
            reader: Some(Box::new(r)),
            offset: 0,
            response_start: 0,
            size,
            attempts: 0,
            validator: None,
        }
    }

    fn ranged_fixture(status: &str, content_range: Option<&str>) -> Result<i64, SourceError> {
        ranged_fixture_with_strategy(status, content_range, MediaRange::OpenEnded)
    }

    fn ranged_fixture_with_strategy(
        status: &str,
        content_range: Option<&str>,
        range: MediaRange,
    ) -> Result<i64, SourceError> {
        let cr = content_range.map(|v| format!("Content-Range: {v}\r\n")).unwrap_or_default();
        let resp =
            format!("HTTP/1.1 {status}\r\nContent-Length: 3\r\n{cr}Connection: close\r\n\r\nabc")
                .into_bytes();
        let (base, server) = test_http::serve_raw(1, move |_, head, _| {
            let lower = head.to_ascii_lowercase();
            let expected = format!("range: {}", range.header(5));
            assert_eq!(
                lower.lines().filter(|l| l.starts_with("range:")).collect::<Vec<_>>(),
                [expected.as_str()]
            );
            assert_eq!(
                lower.lines().filter(|l| l.starts_with("accept-encoding:")).collect::<Vec<_>>(),
                ["accept-encoding: identity"]
            );
            resp.clone()
        });
        let headers = BTreeMap::from([
            ("Range".into(), "bytes=0-".into()),
            ("rAnGe".into(), "bytes=1-".into()),
            ("Accept-Encoding".into(), "gzip".into()),
            ("aCcEpT-EnCoDiNg".into(), "br".into()),
        ]);
        let res = request_with_range(
            &netguard::agent(true),
            &format!("{base}/audio"),
            &headers,
            Some(5),
            range,
        )
        .map(|(_, size, _)| size);
        server.join().unwrap();
        res
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
        let count = chunks.len();
        let mut iter = chunks.into_iter();
        let mut start = 0usize;
        let (base, server) = test_http::serve_raw(count, move |_, head, _| {
            let chunk = iter.next().unwrap();
            let lower = head.to_ascii_lowercase();
            assert!(lower.contains(&format!("\r\nrange: {}\r\n", range.header(start as i64))));
            assert!(lower.contains("\r\naccept-encoding: identity\r\n"));
            let end = start + chunk.len().saturating_sub(1);
            let mut resp = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{total}\r\nConnection: close\r\n\r\n",
                chunk.len().max(1)
            )
            .into_bytes();
            start += chunk.len();
            resp.extend_from_slice(&chunk);
            resp
        });
        let mut cfg = Config::default();
        cfg.allow_private_networks = true;
        let service = Service::new(cfg);
        let headers = BTreeMap::from([
            ("Range".into(), "bytes=0-999".into()),
            ("rAnGe".into(), "bytes=0-".into()),
        ]);
        (service.media_body(&format!("{base}/audio"), &headers, range).unwrap(), server)
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
        assert_eq!(MediaRange::Chunked.header(0), "bytes=0-524287");
        assert_eq!(MediaRange::Chunked.header(524_288), "bytes=524288-1048575");
        assert_eq!(MediaRange::Chunked.header(123), "bytes=123-524410");
        assert_eq!(MediaRange::Chunked.header(i64::MAX), format!("bytes={0}-{0}", i64::MAX));
        assert_eq!(MediaRange::OpenEnded.header(524_288), "bytes=524288-");
    }

    #[test]
    fn bounded_media_ranges_reject_unknown_totals_instead_of_truncating_audio() {
        assert!(ranged_fixture_with_strategy(
            "206 Partial Content",
            Some("bytes 5-7/*"),
            MediaRange::Chunked,
        )
        .unwrap_err()
        .to_string()
        .contains("known total"));
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
        assert!(elapsed < RESUME_DELAY * 3, "normal chunks incurred retry delays: {elapsed:?}");
    }

    #[test]
    fn media_reads_more_segments_than_the_failure_retry_limit() {
        let (mut body, server) = segmented_fixture(
            [b"ab", b"cd", b"ef", b"gh", b"ij"].iter().map(|c| c.to_vec()).collect(),
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
        let (base, listener) = test_http::bind();
        let url = format!("{base}/audio");
        let (release, released) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _, _) = test_http::accept_request(&listener);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\n")
                .unwrap();
            released.recv_timeout(Duration::from_secs(2)).unwrap();
            socket.write_all(b"abc").unwrap();
        });
        let agent = netguard::agent(true);
        let mut body = {
            let _scope = deadline::enter(Instant::now() + Duration::from_millis(100));
            request(&agent, &url, &BTreeMap::new(), Some(0)).unwrap().0
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
        assert_eq!(parse_content_range("bytes   0-99/1234"), Some((0, 99, Some(1234))));
        assert_eq!(parse_content_range("bytes 100-199/*"), Some((100, 199, None)));
        for bad in
            ["bytes */1234", "nonsense", "bytes 100-99/1234", "bytes 0-99/99", "items 0-99/1234"]
        {
            assert_eq!(parse_content_range(bad), None, "{bad}");
        }
    }

    #[test]
    fn interrupted_reads_retry_in_place_without_tearing_down_the_stream() {
        struct InterruptedOnce {
            data: &'static [u8],
            pos: usize,
            interrupt_next: bool,
        }
        impl Read for InterruptedOnce {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if std::mem::replace(&mut self.interrupt_next, false) {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "eintr"));
                }
                let n = (self.data.len() - self.pos).min(buf.len());
                buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
                self.pos += n;
                Ok(n)
            }
        }
        let mut slice = SliceReader {
            reader: InterruptedOnce { data: b"abc", pos: 0, interrupt_next: true },
            remaining: 3,
        };
        let mut out = Vec::new();
        slice.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"abc");

        let mut body =
            dummy_body(InterruptedOnce { data: b"xyz", pos: 0, interrupt_next: true }, 3);
        out.clear();
        body.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"xyz");
    }

    #[test]
    fn duplicate_location_and_etag_headers_are_rejected() {
        for raw_resp in [
            "HTTP/1.1 302 Found\r\nLocation: /a\r\nLocation: /b\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-2/3\r\nContent-Length: 3\r\nETag: \"a\"\r\nETag: \"b\"\r\nConnection: close\r\n\r\nabc",
        ] {
            let bytes = raw_resp.as_bytes().to_vec();
            let (base, server) = test_http::serve_raw(1, move |_, _, _| bytes.clone());
            let result = request(
                &netguard::agent(true),
                &format!("{base}/audio"),
                &BTreeMap::new(),
                Some(0),
            );
            server.join().unwrap();
            assert!(result.is_err());
        }
    }

    #[test]
    fn prefetch_reader_hides_segment_latency_and_propagates_midstream_errors() {
        let chunks = vec![vec![7u8; 32 * 1024], vec![9u8; 32 * 1024], vec![11u8; 1024]];
        let expected: Vec<u8> = chunks.iter().flatten().copied().collect();
        let (body, server) = segmented_fixture(chunks, expected.len());
        let mut reader = PrefetchReader::boxed(body);
        let mut actual = Vec::with_capacity(expected.len());
        let mut small = [0u8; 4096];
        loop {
            let n = reader.read(&mut small).unwrap();
            if n == 0 {
                break;
            }
            actual.extend_from_slice(&small[..n]);
        }
        server.join().unwrap();
        assert_eq!(actual, expected);

        struct FailAfterFirst(bool);
        impl Read for FailAfterFirst {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if !std::mem::replace(&mut self.0, true) {
                    buf[..4].copy_from_slice(b"data");
                    return Ok(4);
                }
                Err(io::Error::new(io::ErrorKind::InvalidData, "corrupt"))
            }
        }
        let mut prefetched = PrefetchReader::boxed(dummy_body(FailAfterFirst(false), 100));
        let mut head = [0u8; 4];
        prefetched.read_exact(&mut head).unwrap();
        assert_eq!(&head, b"data");
        assert_eq!(prefetched.read(&mut head).unwrap_err().kind(), io::ErrorKind::InvalidData);

        // Subsequent reads coalesce small TLS-sized 8 KiB reads into full 64 KiB prefetch chunks.
        struct SmallReads(usize);
        impl Read for SmallReads {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.0 == 0 {
                    return Ok(0);
                }
                let n = self.0.min(buf.len()).min(8192);
                buf[..n].fill(0x5a);
                self.0 -= n;
                Ok(n)
            }
        }
        let total = 8192 + PREFETCH_CHUNK_BYTES;
        let mut coalesced = PrefetchReader::boxed(dummy_body(SmallReads(total), total as i64));
        let mut first = vec![0u8; PREFETCH_CHUNK_BYTES];
        assert_eq!(coalesced.read(&mut first).unwrap(), 8192);
        assert_eq!(coalesced.read(&mut first).unwrap(), PREFETCH_CHUNK_BYTES);
        assert_eq!(coalesced.read(&mut first).unwrap(), 0);
    }
}
