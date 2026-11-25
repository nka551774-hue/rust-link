//! Locating the optional FFmpeg codec helper and installing it when missing.
//!
//! Bare helper names are resolved only inside `toolsDirectory` (never `PATH`)
//! so every node runs the build it was given; missing helpers are downloaded,
//! SHA-256 verified, and unpacked atomically.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// Minimum valid executable size (1 MiB) to reject error pages and truncated transfers.
pub const MIN_SIZE: u64 = 1 << 20;

const MAX_ARCHIVE_BYTES: u64 = 512 << 20;
const PROGRESS: Duration = Duration::from_secs(10);

struct Source {
    tool: &'static str,
    urls: Vec<String>,
    sums: Sums,
    asset: String,
    member: String,
}

enum Sums {
    List(String),
    Beside,
}

pub struct Resolver {
    directory: String,
    download: bool,
    temp: Mutex<Option<PathBuf>>,
}

impl Drop for Resolver {
    fn drop(&mut self) {
        self.close();
    }
}

impl Resolver {
    pub fn new(directory: &str, download: bool) -> Resolver {
        Resolver { directory: directory.to_string(), download, temp: Mutex::new(None) }
    }

    pub fn close(&self) {
        let directory = self.temp.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(directory) = directory {
            let _ = fs::remove_dir_all(directory);
        }
    }

    pub fn resolve(&self, name: &str, configured: &str) -> Result<String, String> {
        let configured = match configured.trim() {
            "" => name,
            trimmed => trimmed,
        };
        if configured.contains('/') || configured.contains('\\') {
            return look_up(configured).ok_or_else(|| {
                format!("{name} is configured as {configured:?} but cannot be run")
            });
        }
        let cached = Path::new(&self.directory).join(executable_name(configured));
        if cached.is_file() {
            if !is_runnable(&cached) {
                let _ = make_executable(&cached);
            }
            return Ok(absolute(&cached));
        }
        let default_exe = executable_name(name);
        let is_default =
            configured.eq_ignore_ascii_case(name) || configured.eq_ignore_ascii_case(&default_exe);
        if !self.download || !is_default {
            return Err(format!(
                "{configured} was not found in {} and autoDownload is off; put a copy there or turn autoDownload on",
                self.directory
            ));
        }
        self.install(name)
    }

    fn install(&self, name: &str) -> Result<String, String> {
        let source = source_for(name)?;
        if self.directory.trim().is_empty() {
            return Err(format!("toolsDirectory is empty, so {name} has nowhere to be installed"));
        }
        fs::create_dir_all(&self.directory)
            .map_err(|e| format!("tools directory {}: {e}", self.directory))?;
        log::info!("downloading media helper tool={name} url={}", source.urls[0]);
        let started = Instant::now();
        let archive = self.fetch(&source)?;
        let target = Path::new(&self.directory).join(executable_name(name));
        extract(&archive, &source.member, &target)?;
        log::info!(
            "media helper installed tool={name} path={} durationMs={}",
            target.display(),
            started.elapsed().as_millis()
        );
        Ok(absolute(&target))
    }

    fn fetch(&self, source: &Source) -> Result<PathBuf, String> {
        let directory = self.work_directory()?;
        let mut last = None;
        for url in &source.urls {
            match download(url, &directory, source) {
                Ok(file) => return Ok(file),
                Err(error) => {
                    log::debug!("media helper download failed url={url} error={error}");
                    last = Some(error);
                }
            }
        }
        Err(last.unwrap_or_else(|| format!("{} has nowhere to be downloaded from", source.tool)))
    }

    fn work_directory(&self) -> Result<PathBuf, String> {
        let mut temp = self.temp.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(directory) = temp.as_ref() {
            return Ok(directory.clone());
        }
        let base = std::env::temp_dir();
        fs::create_dir_all(&base).map_err(|e| format!("temp directory {}: {e}", base.display()))?;
        let mut nonce = [0u8; 8];
        getrandom::fill(&mut nonce)
            .map_err(|e| format!("could not generate temporary directory nonce: {e}"))?;
        let directory =
            base.join(format!("rustlink-tools-{}-{}", std::process::id(), hex_encode(&nonce)));
        let mut builder = fs::DirBuilder::new();
        builder.recursive(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&directory)
            .map_err(|e| format!("download directory {}: {e}", directory.display()))?;
        *temp = Some(directory.clone());
        Ok(directory)
    }
}

fn is_https(url: &str) -> bool {
    url.as_bytes().get(..8).is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"https://"))
}

fn check_redirect_scheme(requested: &str, landed: &str) -> Result<(), String> {
    if is_https(requested) && !is_https(landed) {
        return Err(format!("refusing HTTPS downgrade redirect from {requested} to {landed}"));
    }
    Ok(())
}

fn create_exclusive_private(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn get_ok(
    agent: &ureq::Agent,
    url: &str,
    prefix: Option<&str>,
) -> Result<(String, ureq::http::Response<ureq::Body>), String> {
    let response =
        agent.get(url).header("User-Agent", "rustlink").call().map_err(|e| match prefix {
            Some(p) => format!("{p} {url}: {e}"),
            None => format!("{url} could not be reached: {e}"),
        })?;
    let landed = ureq::ResponseExt::get_uri(&response).to_string();
    check_redirect_scheme(url, &landed)?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(match prefix {
            Some(p) => format!("{p} {url} returned HTTP {status}"),
            None => format!("{url} returned HTTP {status}"),
        });
    }
    Ok((landed, response))
}

fn download(url: &str, directory: &Path, source: &Source) -> Result<PathBuf, String> {
    let agent = agent();
    let (landed, mut response) = get_ok(agent, url, None)?;
    let total = response.body().content_length();
    let name = if source.asset.is_empty() { "download.bin" } else { &source.asset };
    let file = directory.join(name);
    let mut out =
        create_exclusive_private(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut body = response.body_mut().as_reader();
    let (written, got_sum) = match copy(&mut body, &mut out, source.tool, total) {
        Ok(res) => res,
        Err(error) => {
            drop(out);
            let _ = fs::remove_file(&file);
            return Err(error);
        }
    };
    drop(out);
    if written < MIN_SIZE {
        let _ = fs::remove_file(&file);
        return Err(format!("{url} returned only {written} bytes"));
    }
    let (sums_url, allow_bare) = match &source.sums {
        Sums::List(list) => (list.clone(), false),
        Sums::Beside => {
            (format!("{}.sha256", landed.split(['?', '#']).next().unwrap_or(&landed)), true)
        }
    };
    if let Err(error) = verify(agent, &sums_url, &source.asset, &got_sum, allow_bare) {
        let _ = fs::remove_file(&file);
        return Err(error);
    }
    Ok(file)
}

fn copy(
    reader: &mut impl Read,
    out: &mut impl Write,
    tool: &str,
    total: Option<u64>,
) -> Result<(u64, String), String> {
    if let Some(total) = total.filter(|&t| t > MAX_ARCHIVE_BYTES) {
        return Err(format!(
            "refusing {tool} download of {} MiB (exceeds 512 MiB safety cap)",
            total >> 20
        ));
    }
    let mut buffer = vec![0u8; 64 << 10];
    let mut hasher = Sha256::new();
    let mut written = 0u64;
    let mut next = Instant::now() + PROGRESS;
    loop {
        let read = reader.read(&mut buffer).map_err(|e| e.to_string())?;
        if read == 0 {
            return Ok((written, hex_encode(&hasher.finalize())));
        }
        written += read as u64;
        if written > MAX_ARCHIVE_BYTES {
            return Err(format!(
                "{tool} download exceeded 512 MiB safety cap at {} MiB",
                written >> 20
            ));
        }
        hasher.update(&buffer[..read]);
        out.write_all(&buffer[..read]).map_err(|e| e.to_string())?;
        let now = Instant::now();
        if now >= next {
            next = now + PROGRESS;
            if let Some(total) = total {
                log::info!(
                    "media helper download in progress tool={tool} receivedMiB={} totalMiB={}",
                    written >> 20,
                    total >> 20
                );
            } else {
                log::info!(
                    "media helper download in progress tool={tool} receivedMiB={}",
                    written >> 20
                );
            }
        }
    }
}

#[derive(Debug)]
struct ToolIdleConnector;

impl<In: ureq::unversioned::transport::Transport> ureq::unversioned::transport::Connector<In>
    for ToolIdleConnector
{
    type Out = ToolIdleTransport<In>;

    fn connect(
        &self,
        _details: &ureq::unversioned::transport::ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        Ok(chained.map(ToolIdleTransport))
    }
}

#[derive(Debug)]
struct ToolIdleTransport<In>(In);

impl<In: ureq::unversioned::transport::Transport> ureq::unversioned::transport::Transport
    for ToolIdleTransport<In>
{
    fn buffers(&mut self) -> &mut dyn ureq::unversioned::transport::Buffers {
        self.0.buffers()
    }

    fn transmit_output(
        &mut self,
        amount: usize,
        mut timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<(), ureq::Error> {
        timeout.after = timeout.after.min(Duration::from_secs(60).into());
        self.0.transmit_output(amount, timeout)
    }

    fn await_input(
        &mut self,
        mut timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<bool, ureq::Error> {
        timeout.after = timeout.after.min(Duration::from_secs(60).into());
        self.0.await_input(timeout)
    }

    fn is_open(&mut self) -> bool {
        self.0.is_open()
    }

    fn is_tls(&self) -> bool {
        self.0.is_tls()
    }
}

fn agent() -> &'static ureq::Agent {
    use ureq::unversioned::transport::Connector as _;
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let config = ureq::Agent::config_builder()
            .accept_encoding("identity")
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_global(None)
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .timeout_recv_body(None)
            .http_status_as_error(false)
            .build();
        ureq::Agent::with_parts(
            config,
            ureq::unversioned::transport::DefaultConnector::default().chain(ToolIdleConnector),
            ureq::unversioned::resolver::DefaultResolver::default(),
        )
    })
}

fn verify(
    agent: &ureq::Agent,
    sums_url: &str,
    asset: &str,
    got: &str,
    allow_bare: bool,
) -> Result<(), String> {
    let (_, response) = get_ok(agent, sums_url, Some("checksum list"))?;
    let list = response
        .into_body()
        .with_config()
        .limit(1 << 20)
        .read_to_string()
        .map_err(|e| format!("checksum list {sums_url}: {e}"))?;
    let want = lookup_sum(&list, asset, allow_bare)
        .ok_or_else(|| format!("checksum list {sums_url} has no entry for {asset}"))?;
    if !want.eq_ignore_ascii_case(got) {
        return Err(format!("{asset} checksum mismatch: expected {want}, downloaded {got}"));
    }
    Ok(())
}

fn lookup_sum<'a>(list: &'a str, asset: &str, allow_bare: bool) -> Option<&'a str> {
    let is_hex = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit());
    list.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let hash = fields.next()?;
        match (fields.next(), fields.next()) {
            (Some(raw), None) => {
                let name = raw.strip_prefix('*').unwrap_or(raw);
                let name = name.strip_prefix("./").unwrap_or(name);
                (name == asset && is_hex(hash)).then_some(hash)
            }
            (None, None) if allow_bare && hash.len() == 64 && is_hex(hash) => Some(hash),
            _ => None,
        }
    })
}

#[cfg(test)]
fn file_sum(file: &Path) -> Result<String, String> {
    let handle = fs::File::open(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut reader = std::io::BufReader::with_capacity(64 << 10, handle);
    let mut hasher = Sha256::new();
    std::io::copy(&mut reader, &mut hasher).map_err(|e| e.to_string())?;
    Ok(hex_encode(&hasher.finalize()))
}

fn extract(archive: &Path, member: &str, target: &Path) -> Result<(), String> {
    let file = fs::File::open(archive).map_err(|e| format!("{}: {e}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("{}: {e}", archive.display()))?;
    let exact_bin = Path::new("bin").join(member);
    let mut selected: Option<(usize, u8)> = None;
    let mut saw_undersized = false;
    for index in 0..zip.len() {
        let entry = zip.by_index(index).map_err(|e| e.to_string())?;
        if entry.is_dir() || entry.is_symlink() {
            continue;
        }
        let Some(enclosed) = entry.enclosed_name() else {
            continue;
        };
        if enclosed.file_name() != Some(std::ffi::OsStr::new(member)) {
            continue;
        }
        if entry.size() < MIN_SIZE {
            saw_undersized = true;
            continue;
        }
        let rank = if enclosed == exact_bin {
            2
        } else if enclosed
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|dir| dir.eq_ignore_ascii_case("bin"))
        {
            1
        } else {
            0
        };
        if selected.is_none_or(|(_, best)| rank > best) {
            selected = Some((index, rank));
            if rank == 2 {
                break;
            }
        }
    }
    let Some((chosen, _)) = selected else {
        return Err(if saw_undersized {
            format!("{member} in {} is smaller than {MIN_SIZE} bytes", archive.display())
        } else {
            format!("{member} is not part of {}", archive.display())
        });
    };
    let mut entry = zip.by_index(chosen).map_err(|e| e.to_string())?;
    if entry.size() > MAX_ARCHIVE_BYTES {
        return Err(format!("{member} exceeds 512 MiB uncompressed safety cap"));
    }
    copy_into(&mut (&mut entry).take(MAX_ARCHIVE_BYTES + 1), target)
}

fn copy_into(source: &mut impl Read, target: &Path) -> Result<(), String> {
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).map_err(|e| format!("{}: {e}", target.display()))?;
    let temp = PathBuf::from(format!(
        "{}.part-{}-{}",
        target.display(),
        std::process::id(),
        hex_encode(&nonce)
    ));
    let result = (|| -> Result<(), String> {
        let io_err = |e: std::io::Error| format!("{}: {e}", target.display());
        let mut out = create_exclusive_private(&temp).map_err(io_err)?;
        let written = std::io::copy(source, &mut out).map_err(io_err)?;
        if written < MIN_SIZE {
            return Err(format!(
                "{} extracted only {written} bytes (minimum {MIN_SIZE})",
                target.display()
            ));
        }
        if written > MAX_ARCHIVE_BYTES {
            return Err(format!("{} exceeded 512 MiB uncompressed safety cap", target.display()));
        }
        out.sync_all().map_err(io_err)?;
        drop(out);
        make_executable(&temp)?;
        fs::rename(&temp, target).map_err(io_err)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn executable_name(name: &str) -> String {
    if cfg!(windows) && !Path::new(name).extension().is_some_and(|e| e.eq_ignore_ascii_case("exe"))
    {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms =
        fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn absolute(path: &Path) -> String {
    if path.is_absolute() {
        return path.display().to_string();
    }
    std::env::current_dir()
        .map(|dir| dir.join(path).display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

fn look_up(program: &str) -> Option<String> {
    let check = |path: &Path| (path.is_file() && is_runnable(path)).then(|| absolute(path));
    if cfg!(windows)
        && !Path::new(program).extension().is_some_and(|e| e.eq_ignore_ascii_case("exe"))
    {
        let with_exe = PathBuf::from(format!("{program}.exe"));
        if let Some(found) = check(&with_exe) {
            return Some(found);
        }
    }
    check(Path::new(program))
}

#[cfg(unix)]
fn is_runnable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(not(unix))]
fn is_runnable(_path: &Path) -> bool {
    true
}

fn source_for(name: &str) -> Result<Source, String> {
    match name {
        "ffmpeg" => ffmpeg_source(name),
        other => Err(format!("no download source is known for {other}")),
    }
}

fn ffmpeg_source(name: &str) -> Result<Source, String> {
    let os = std::env::consts::OS;
    let arch = match (os, std::env::consts::ARCH) {
        ("windows", "x86_64") => "win64",
        ("windows", "aarch64") => "winarm64",
        ("linux" | "macos", "x86_64") => "amd64",
        ("linux" | "macos", "aarch64") => "arm64",
        (_, arch) => return Err(format!("no FFmpeg build is known for {os}/{arch}")),
    };
    match os {
        "windows" => {
            let asset = format!("ffmpeg-master-latest-{arch}-gpl.zip");
            let base = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/";
            Ok(Source {
                tool: "ffmpeg",
                urls: vec![format!("{base}{asset}")],
                sums: Sums::List(format!("{base}checksums.sha256")),
                asset,
                member: format!("{name}.exe"),
            })
        }
        "linux" | "macos" => {
            let base = format!("https://ffmpeg.martin-riedl.de/redirect/latest/{os}/{arch}/");
            Ok(Source {
                tool: "ffmpeg",
                urls: vec![
                    format!("{base}release/{name}.zip"),
                    format!("{base}snapshot/{name}.zip"),
                ],
                sums: Sums::Beside,
                asset: format!("{name}.zip"),
                member: name.to_string(),
            })
        }
        other => Err(format!("no FFmpeg build is known for {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn serve(
        routes: Vec<(&'static str, Vec<u8>)>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let mut asked = Vec::new();
            for _ in 0..routes.len() {
                let (mut socket, _) = listener.accept().unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.windows(4).any(|b| b == b"\r\n\r\n") {
                    let count = socket.read(&mut buffer).unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&buffer[..count]);
                }
                let path =
                    String::from_utf8_lossy(&request).split_whitespace().nth(1).unwrap().to_owned();
                let (_, response) =
                    routes.iter().find(|(route, _)| *route == path).expect("an expected path");
                socket.write_all(response).unwrap();
                asked.push(path);
            }
            asked
        });
        (base, worker)
    }

    fn reply(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut bytes = format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    fn beside_source() -> Source {
        Source {
            tool: "ffmpeg",
            urls: Vec::new(),
            sums: Sums::Beside,
            asset: "ffmpeg.zip".into(),
            member: "ffmpeg".into(),
        }
    }

    fn work_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("rustlink-tools-test-{label}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_download_is_checked_against_the_checksum_beside_the_build_it_redirected_to() {
        let body = vec![7u8; MIN_SIZE as usize + 100];
        let sum = format!("{:x}  ffmpeg.zip\n", Sha256::digest(&body));
        let (base, server) = serve(vec![
            ("/latest/ffmpeg.zip", reply("302 Found", "Location: /build/1/ffmpeg.zip\r\n", b"")),
            ("/build/1/ffmpeg.zip", reply("200 OK", "", &body)),
            ("/build/1/ffmpeg.zip.sha256", reply("200 OK", "", sum.as_bytes())),
        ]);
        let directory = work_dir("good");
        let file = download(&format!("{base}/latest/ffmpeg.zip"), &directory, &beside_source())
            .expect("a matching checksum passes");
        assert!(file.is_file());
        assert_eq!(
            server.join().unwrap(),
            ["/latest/ffmpeg.zip", "/build/1/ffmpeg.zip", "/build/1/ffmpeg.zip.sha256"]
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn bad_or_missing_checksums_are_refused_and_cleaned_up() {
        let body = vec![7u8; MIN_SIZE as usize + 100];
        let wrong = format!("{}  ffmpeg.zip\n", "0".repeat(64));
        let (base, server) = serve(vec![
            ("/ffmpeg.zip", reply("200 OK", "", &body)),
            ("/ffmpeg.zip.sha256", reply("200 OK", "", wrong.as_bytes())),
        ]);
        let directory = work_dir("bad");
        let error =
            download(&format!("{base}/ffmpeg.zip"), &directory, &beside_source()).unwrap_err();
        server.join().unwrap();
        assert!(error.contains("checksum mismatch"), "{error}");
        assert!(!directory.join("ffmpeg.zip").exists());
        let _ = fs::remove_dir_all(directory);

        let (base, server) = serve(vec![
            ("/ffmpeg.zip", reply("200 OK", "", &body)),
            ("/ffmpeg.zip.sha256", reply("404 Not Found", "", b"gone")),
        ]);
        let directory = work_dir("missing");
        let error =
            download(&format!("{base}/ffmpeg.zip"), &directory, &beside_source()).unwrap_err();
        server.join().unwrap();
        assert!(error.contains("checksum"), "{error}");
        assert!(!directory.join("ffmpeg.zip").exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn checksum_lookup_and_sha256_hashing_work() {
        let list = "aaa  ffmpeg.zip\nbbb *./ffmpeg.exe\nnot a listing\nccc  one two\n";
        assert_eq!(lookup_sum(list, "ffmpeg.exe", false), Some("bbb"));
        assert_eq!(lookup_sum(list, "ffmpeg.zip", false), Some("aaa"));
        assert_eq!(lookup_sum(list, "one", false), None);
        assert_eq!(lookup_sum(list, "missing", false), None);
        let bare = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\n";
        assert_eq!(lookup_sum(bare, "ffmpeg.zip", false), None);
        assert_eq!(lookup_sum(bare, "ffmpeg.zip", true), Some(bare.trim()));

        let file = std::env::temp_dir().join(format!("rustlink-sum-{}", std::process::id()));
        fs::write(&file, b"abc").unwrap();
        assert_eq!(file_sum(&file).unwrap(), bare.trim());
        let _ = fs::remove_file(&file);
    }

    #[test]
    fn resolver_handles_cached_missing_and_explicit_paths() {
        let directory = work_dir("cached");
        let name = executable_name("ffmpeg");
        fs::write(directory.join(&name), b"stub").unwrap();
        let resolver = Resolver::new(&directory.display().to_string(), false);
        assert!(resolver.resolve("ffmpeg", "").unwrap().ends_with(&name));
        resolver.close();
        let _ = fs::remove_dir_all(&directory);

        let empty = std::env::temp_dir().join(format!("rustlink-empty-{}", std::process::id()));
        let error =
            Resolver::new(&empty.display().to_string(), false).resolve("ffmpeg", "").unwrap_err();
        assert!(error.contains("autoDownload is off"));
        assert!(Resolver::new(".", true)
            .resolve("ffmpeg", "/nowhere/ffmpeg")
            .unwrap_err()
            .contains("cannot be run"));

        let source = source_for("ffmpeg").unwrap();
        assert!(!source.urls.is_empty() && source.urls.iter().all(|u| u.starts_with("https://")));
        assert!(source_for("ffprobe").is_err());
        assert_eq!(executable_name("ffmpeg"), if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" });
    }

    #[test]
    fn https_redirect_downgrade_to_plain_http_is_refused() {
        assert!(check_redirect_scheme("https://example.com/a", "http://example.com/a").is_err());
        assert!(check_redirect_scheme("HTTPS://example.com/a", "HTTP://mirror.example/a").is_err());
        assert!(check_redirect_scheme("https://example.com/a", "HTTPS://cdn.example.com/b").is_ok());
        assert!(check_redirect_scheme("http://127.0.0.1:8080/a", "http://127.0.0.1:8080/b").is_ok());
    }

    #[test]
    fn extract_skips_undersized_entries_and_prefers_bin_member() {
        let directory = work_dir("extract");
        let archive = directory.join("bundle.zip");
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        {
            let mut writer = zip::ZipWriter::new(fs::File::create(&archive).unwrap());
            writer.start_file("bin/ffmpeg", options).unwrap();
            writer.write_all(b"tiny stub").unwrap();
            writer.start_file("ffmpeg", options).unwrap();
            writer.write_all(&vec![b'R'; MIN_SIZE as usize + 8]).unwrap();
            writer.start_file("release/bin/ffmpeg", options).unwrap();
            writer.write_all(&vec![b'B'; MIN_SIZE as usize + 16]).unwrap();
            writer.finish().unwrap();
        }
        let target = directory.join("ffmpeg-out");
        extract(&archive, "ffmpeg", &target).unwrap();
        let extracted = fs::read(&target).unwrap();
        assert_eq!(extracted.len(), MIN_SIZE as usize + 16);
        assert!(extracted.iter().all(|&b| b == b'B'));

        let tiny_archive = directory.join("tiny.zip");
        {
            let mut writer = zip::ZipWriter::new(fs::File::create(&tiny_archive).unwrap());
            writer.start_file("bin/ffmpeg", options).unwrap();
            writer.write_all(b"too short").unwrap();
            writer.finish().unwrap();
        }
        let tiny_target = directory.join("ffmpeg-tiny");
        assert!(extract(&tiny_archive, "ffmpeg", &tiny_target).is_err());
        assert!(!tiny_target.exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn staged_files_and_work_directory_use_exclusive_creation() {
        let resolver = Resolver::new(".tools", false);
        let work = resolver.work_directory().unwrap();
        assert!(work.is_dir());
        let existing = work.join("existing.bin");
        create_exclusive_private(&existing).unwrap();
        assert!(create_exclusive_private(&existing).is_err());
        resolver.close();
        assert!(!work.exists());
    }
}
