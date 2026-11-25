//! Locating the optional FFmpeg codec helper and installing it when missing.
//!
//! FFmpeg is an external program. A bare helper name is looked for in
//! the tools directory and never on `PATH`, so every node runs the build it was
//! given rather than whatever the machine happens to have installed; a missing
//! helper is downloaded for the current platform and reused from then on.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// The smallest download that is treated as a real executable. A shorter body
/// is an error page or a truncated transfer, never a helper.
pub const MIN_SIZE: u64 = 1 << 20;

/// How often a running download reports its position, so a slow link does not
/// look like a hang.
const PROGRESS: Duration = Duration::from_secs(10);

/// Where one helper comes from on this platform.
struct Source {
    /// The helper name, which is what a progress line names.
    tool: &'static str,
    /// Tried in order; the first that answers is used.
    urls: Vec<String>,
    /// Where the checksum of the asset is published.
    sums: Sums,
    /// The file name used by the checksum listing and staged download.
    asset: String,
    /// The base name to pull out of the zip archive.
    member: String,
}

/// How a download is checked. Either way the checksum comes from the same
/// provider as the file, so it catches a corrupt or truncated transfer and a
/// bad mirror, not a provider that is itself compromised.
enum Sums {
    /// A `<sha256>  <file>` listing at this URL.
    List(String),
    /// A `<sha256>  <file>` file named `<file>.sha256`, next to wherever the
    /// download landed. A "latest" URL redirects to a numbered build, and the
    /// checksum belongs to that build, not to the redirecting address.
    Beside,
}

/// Locates the media helpers and installs the ones that are missing.
pub struct Resolver {
    directory: String,
    download: bool,
    /// The directory downloads land in, removed on the way out.
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

    /// Removes the archives fetched during this run.
    pub fn close(&self) {
        let directory = self.temp.lock().unwrap().take();
        if let Some(directory) = directory {
            let _ = fs::remove_dir_all(directory);
        }
    }

    /// Returns something runnable for `name`.
    ///
    /// `configured` is what the configuration says: an explicit path, or a bare
    /// helper name that is looked for in the tools directory. A bare name is
    /// never looked up on `PATH`, which is what keeps every node on the same
    /// build of a helper.
    pub fn resolve(&self, name: &str, configured: &str) -> Result<String, String> {
        let configured = configured.trim();
        let configured = if configured.is_empty() { name } else { configured };
        // An explicit path is a deliberate choice, so it is never replaced by a
        // download: a broken path is reported rather than papered over.
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

    /// Downloads and unpacks a helper for this platform.
    fn install(&self, name: &str) -> Result<String, String> {
        let source = source_for(name)?;
        if self.directory.trim().is_empty() {
            return Err(format!("toolsDirectory is empty, so {name} has nowhere to be installed"));
        }
        fs::create_dir_all(&self.directory)
            .map_err(|error| format!("tools directory {}: {error}", self.directory))?;
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

    /// Downloads the archive from the first available source.
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

    /// The directory downloads land in, created on first use with private permissions.
    fn work_directory(&self) -> Result<PathBuf, String> {
        let mut temp = self.temp.lock().unwrap();
        if let Some(directory) = temp.as_ref() {
            return Ok(directory.clone());
        }
        let mut nonce = [0u8; 8];
        let _ = getrandom::fill(&mut nonce);
        let suffix = hex_encode(&nonce);
        let directory =
            std::env::temp_dir().join(format!("rustlink-tools-{}-{suffix}", std::process::id()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&directory)
                .map_err(|error| format!("download directory {}: {error}", directory.display()))?;
        }
        #[cfg(not(unix))]
        {
            fs::create_dir_all(&directory)
                .map_err(|error| format!("download directory {}: {error}", directory.display()))?;
        }
        *temp = Some(directory.clone());
        Ok(directory)
    }
}

/// Encodes bytes as lowercase hexadecimal in a single allocation.
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Streams one URL into the download directory and checks what arrived.
fn download(url: &str, directory: &Path, source: &Source) -> Result<PathBuf, String> {
    let agent = agent();
    let mut response = agent
        .get(url)
        .header("User-Agent", "rustlink")
        .call()
        .map_err(|error| format!("{url} could not be reached: {error}"))?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(format!("{url} returned HTTP {status}"));
    }
    // Where the download ended up, which is not `url` when that redirected.
    let landed = ureq::ResponseExt::get_uri(&response).to_string();
    let total = response.body().content_length();
    let name = if source.asset.is_empty() { "download.bin" } else { source.asset.as_str() };
    let file = directory.join(name);
    let mut out =
        fs::File::create(&file).map_err(|error| format!("{}: {error}", file.display()))?;
    let mut body = response.body_mut().as_reader();
    let written = match copy(&mut body, &mut out, source.tool, total) {
        Ok(written) => written,
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
    let sums_url = match &source.sums {
        Sums::List(list) => list.clone(),
        Sums::Beside => {
            format!("{}.sha256", landed.split(['?', '#']).next().unwrap_or(&landed))
        }
    };
    if let Err(error) = verify(agent, &sums_url, &source.asset, &file) {
        let _ = fs::remove_file(&file);
        return Err(error);
    }
    Ok(file)
}

const MAX_ARCHIVE_BYTES: u64 = 512 << 20;

/// Copies a download into a file, reporting where it is every so often.
fn copy(
    reader: &mut impl Read,
    out: &mut impl Write,
    tool: &str,
    total: Option<u64>,
) -> Result<u64, String> {
    if let Some(total) = total {
        if total > MAX_ARCHIVE_BYTES {
            return Err(format!(
                "refusing {tool} download of {} MiB (exceeds 512 MiB safety cap)",
                total >> 20
            ));
        }
    }
    let mut buffer = vec![0u8; 64 << 10];
    let mut written = 0u64;
    let mut next = Instant::now() + PROGRESS;
    loop {
        let read = reader.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            return Ok(written);
        }
        written += read as u64;
        if written > MAX_ARCHIVE_BYTES {
            return Err(format!(
                "{tool} download exceeded 512 MiB safety cap at {} MiB",
                written >> 20
            ));
        }
        out.write_all(&buffer[..read]).map_err(|error| error.to_string())?;
        let now = Instant::now();
        if now >= next {
            next = now + PROGRESS;
            match total {
                Some(total) => log::info!(
                    "media helper download in progress tool={tool} receivedMiB={} totalMiB={}",
                    written >> 20,
                    total >> 20
                ),
                None => log::info!(
                    "media helper download in progress tool={tool} receivedMiB={}",
                    written >> 20
                ),
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

/// A client suited to a large file: socket operations time out after 60 seconds
/// of inactivity, while an active download on a slow link is allowed to finish.
fn agent() -> &'static ureq::Agent {
    use ureq::unversioned::transport::Connector as _;
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
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
/// Checks a download against a `<sha256>  <file>` listing.
fn verify(agent: &ureq::Agent, sums_url: &str, asset: &str, file: &Path) -> Result<(), String> {
    let response = agent
        .get(sums_url)
        .header("User-Agent", "rustlink")
        .call()
        .map_err(|error| format!("checksum list {sums_url}: {error}"))?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(format!("checksum list {sums_url} returned HTTP {status}"));
    }
    let list = response
        .into_body()
        .with_config()
        .limit(1 << 20)
        .read_to_string()
        .map_err(|error| format!("checksum list {sums_url}: {error}"))?;
    let want = lookup_sum(&list, asset)
        .ok_or_else(|| format!("checksum list {sums_url} has no entry for {asset}"))?;
    let got = file_sum(file)?;
    if !want.eq_ignore_ascii_case(&got) {
        return Err(format!("{asset} checksum mismatch: expected {want}, downloaded {got}"));
    }
    Ok(())
}

/// Finds the hash a listing gives for one file. Accepts both `<sha256>  <file>`
/// and GNU `sha256sum -b` `<sha256> *<file>`.
fn lookup_sum(list: &str, asset: &str) -> Option<String> {
    list.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let (Some(hash), Some(raw_name), None) = (fields.next(), fields.next(), fields.next())
        else {
            return None;
        };
        let name = raw_name.strip_prefix('*').unwrap_or(raw_name);
        let name = name.strip_prefix("./").unwrap_or(name);
        (name == asset && !hash.is_empty() && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hash.to_owned())
    })
}

/// The SHA-256 of a file, as lowercase hexadecimal.
fn file_sum(file: &Path) -> Result<String, String> {
    let handle = fs::File::open(file).map_err(|error| format!("{}: {error}", file.display()))?;
    let mut reader = std::io::BufReader::with_capacity(64 << 10, handle);
    let mut hasher = Sha256::new();
    std::io::copy(&mut reader, &mut hasher).map_err(|error| error.to_string())?;
    Ok(hex_encode(&hasher.finalize()))
}

/// Copies one member of a zip archive onto the target.
fn extract(archive: &Path, member: &str, target: &Path) -> Result<(), String> {
    let file =
        fs::File::open(archive).map_err(|error| format!("{}: {error}", archive.display()))?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|error| format!("{}: {error}", archive.display()))?;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|error| error.to_string())?;
        if entry.is_dir() || entry.is_symlink() {
            continue;
        }
        let matched = entry
            .enclosed_name()
            .and_then(|path| path.file_name().map(|f| f == std::ffi::OsStr::new(member)))
            .unwrap_or(false);
        if matched {
            if entry.size() > MAX_ARCHIVE_BYTES {
                return Err(format!("{member} exceeds 512 MiB uncompressed safety cap"));
            }
            let mut bounded = (&mut entry).take(MAX_ARCHIVE_BYTES + 1);
            return copy_into(&mut bounded, target);
        }
    }
    Err(format!("{member} is not part of {}", archive.display()))
}

/// Writes through a temporary file with executable permissions applied before
/// atomic rename, so `target` is never half-written or non-executable.
fn copy_into(source: &mut impl Read, target: &Path) -> Result<(), String> {
    let temp = PathBuf::from(format!("{}.part-{}", target.display(), std::process::id()));
    let write_result = fs::File::create(&temp)
        .map_err(|error| format!("{}: {error}", target.display()))
        .and_then(|mut out| {
            let written = std::io::copy(source, &mut out)
                .map_err(|e| format!("{}: {e}", target.display()))?;
            if written > MAX_ARCHIVE_BYTES {
                return Err(format!(
                    "{} exceeded 512 MiB uncompressed safety cap",
                    target.display()
                ));
            }
            out.sync_all().map_err(|e| format!("{}: {e}", target.display()))
        });
    write_result
        .and_then(|()| make_executable(&temp))
        .and_then(|()| {
            fs::rename(&temp, target).map_err(|error| format!("{}: {error}", target.display()))
        })
        .inspect_err(|_| {
            let _ = fs::remove_file(&temp);
        })
}

/// The file name a helper is installed under.
fn executable_name(name: &str) -> String {
    if cfg!(windows)
        && !name
            .as_bytes()
            .get(name.len().wrapping_sub(4)..)
            .is_some_and(|ext| ext.eq_ignore_ascii_case(b".exe"))
    {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// Gives a downloaded helper the permissions it needs to be run.
#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions =
        fs::metadata(path).map_err(|error| format!("{}: {error}", path.display()))?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).map_err(|error| format!("{}: {error}", path.display()))
}

/// Nothing has to be done on a platform where an executable bit is not a thing.
#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// A path as an operator would write it: absolute, but without resolving the
/// symbolic links on the way.
fn absolute(path: &Path) -> String {
    if path.is_absolute() {
        return path.display().to_string();
    }
    std::env::current_dir()
        .map(|directory| directory.join(path).display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

/// Validates an explicit helper path configured by the operator.
fn look_up(program: &str) -> Option<String> {
    let check = |path: &Path| (path.is_file() && is_runnable(path)).then(|| absolute(path));
    if cfg!(windows)
        && !Path::new(program).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
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

/// Where a helper comes from on this platform.
fn source_for(name: &str) -> Result<Source, String> {
    match name {
        "ffmpeg" => ffmpeg_source(name),
        other => Err(format!("no download source is known for {other}")),
    }
}

/// FFmpeg comes from BtbN on Windows and martin-riedl.de everywhere else. Both
/// are GPL builds with libopus, which the encoder needs. The archive also holds
/// ffprobe, which this node does not use: containers are parsed in process, so
/// only the encoder is unpacked.
fn ffmpeg_source(name: &str) -> Result<Source, String> {
    match std::env::consts::OS {
        "windows" => {
            let asset = format!("ffmpeg-master-latest-{}-gpl.zip", windows_arch()?);
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
            // The provider uses the platform names `std` does, so the current
            // one is passed through unchanged.
            let base = format!(
                "https://ffmpeg.martin-riedl.de/redirect/latest/{}/{}/",
                std::env::consts::OS,
                posix_arch()?
            );
            Ok(Source {
                tool: "ffmpeg",
                urls: vec![
                    format!("{base}release/{name}.zip"),
                    format!("{base}snapshot/{name}.zip"),
                ],
                // martin-riedl.de publishes `ffmpeg.zip.sha256` beside each build.
                sums: Sums::Beside,
                asset: format!("{name}.zip"),
                member: name.to_string(),
            })
        }
        other => Err(format!("no FFmpeg build is known for {other}")),
    }
}

fn windows_arch() -> Result<&'static str, String> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("win64"),
        "aarch64" => Ok("winarm64"),
        arch => Err(format!("no FFmpeg build is known for windows/{arch}")),
    }
}

fn posix_arch() -> Result<&'static str, String> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("amd64"),
        "aarch64" => Ok("arm64"),
        arch => Err(format!("no FFmpeg build is known for {}/{arch}", std::env::consts::OS)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serves `routes` as `(path, full response)`, one connection each, in the
    /// order they are asked for, and reports the paths that were requested.
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
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let count = socket.read(&mut buffer).unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&buffer[..count]);
                }
                let line = String::from_utf8_lossy(&request);
                let path = line.split_whitespace().nth(1).unwrap().to_string();
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
        let directory = std::env::temp_dir()
            .join(format!("rustlink-tools-test-{label}-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        directory
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
        // The checksum asked for is the one for the numbered build, not for the
        // address that redirected to it, which publishes none.
        assert_eq!(
            server.join().unwrap(),
            ["/latest/ffmpeg.zip", "/build/1/ffmpeg.zip", "/build/1/ffmpeg.zip.sha256"]
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn a_download_that_does_not_match_its_checksum_is_refused_and_removed() {
        let body = vec![7u8; MIN_SIZE as usize + 100];
        let wrong = format!("{}  ffmpeg.zip\n", "0".repeat(64));
        let (base, server) = serve(vec![
            ("/ffmpeg.zip", reply("200 OK", "", &body)),
            ("/ffmpeg.zip.sha256", reply("200 OK", "", wrong.as_bytes())),
        ]);
        let directory = work_dir("bad");
        let error = download(&format!("{base}/ffmpeg.zip"), &directory, &beside_source())
            .expect_err("a different hash must not install");
        server.join().unwrap();
        assert!(error.contains("checksum mismatch"), "{error}");
        assert!(!directory.join("ffmpeg.zip").exists(), "the unchecked file is not left behind");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn a_missing_checksum_is_a_failure_and_not_a_pass() {
        let body = vec![7u8; MIN_SIZE as usize + 100];
        let (base, server) = serve(vec![
            ("/ffmpeg.zip", reply("200 OK", "", &body)),
            ("/ffmpeg.zip.sha256", reply("404 Not Found", "", b"gone")),
        ]);
        let directory = work_dir("missing");
        let error = download(&format!("{base}/ffmpeg.zip"), &directory, &beside_source())
            .expect_err("no checksum means no install");
        server.join().unwrap();
        assert!(error.contains("no entry for ffmpeg.zip") || error.contains("checksum"), "{error}");
        assert!(!directory.join("ffmpeg.zip").exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn a_listing_yields_the_hash_of_the_named_file() {
        let list = "aaa  ffmpeg.zip\nbbb  ffmpeg.exe\nnot a listing\nccc  one two\n";
        assert_eq!(lookup_sum(list, "ffmpeg.exe"), Some("bbb".to_string()));
        assert_eq!(lookup_sum(list, "ffmpeg.zip"), Some("aaa".to_string()));
        assert_eq!(lookup_sum(list, "one"), None);
        assert_eq!(lookup_sum(list, "missing"), None);
    }

    #[test]
    fn the_hash_of_a_file_is_its_sha256_in_lowercase_hexadecimal() {
        let file = std::env::temp_dir().join(format!("rustlink-sum-{}", std::process::id()));
        std::fs::write(&file, b"abc").unwrap();
        // The SHA-256 of "abc", which is the value every published listing uses.
        assert_eq!(
            file_sum(&file).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn a_helper_that_is_not_configured_is_looked_for_in_the_tools_directory() {
        let directory = std::env::temp_dir().join(format!("rustlink-tools-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let name = executable_name("ffmpeg");
        std::fs::write(directory.join(&name), b"stub").unwrap();
        let resolver = Resolver::new(&directory.display().to_string(), false);
        let resolved = resolver.resolve("ffmpeg", "").unwrap();
        assert!(resolved.ends_with(&name), "{resolved}");
        resolver.close();
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_missing_helper_is_refused_when_auto_download_is_off() {
        let directory = std::env::temp_dir().join(format!("rustlink-empty-{}", std::process::id()));
        let resolver = Resolver::new(&directory.display().to_string(), false);
        let error = resolver.resolve("ffmpeg", "").unwrap_err();
        assert!(error.contains("autoDownload is off"), "{error}");
        assert!(error.contains(&directory.display().to_string()));
    }

    #[test]
    fn an_explicit_path_that_cannot_be_run_is_reported_rather_than_replaced() {
        let resolver = Resolver::new(".", true);
        let error = resolver.resolve("ffmpeg", "/nowhere/ffmpeg").unwrap_err();
        assert!(error.contains("cannot be run"), "{error}");
    }

    #[test]
    fn every_helper_this_node_uses_has_a_source_on_this_platform() {
        let source = source_for("ffmpeg").unwrap();
        assert!(!source.urls.is_empty());
        assert!(source.urls.iter().all(|url| url.starts_with("https://")));
        assert!(source_for("ffprobe").is_err());
    }

    #[test]
    fn a_helper_name_carries_the_extension_of_this_platform() {
        let name = executable_name("ffmpeg");
        if cfg!(windows) {
            assert_eq!(name, "ffmpeg.exe");
        } else {
            assert_eq!(name, "ffmpeg");
        }
    }
}
