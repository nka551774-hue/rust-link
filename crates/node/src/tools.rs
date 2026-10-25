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
    /// A `<sha256>  <file>` listing covering the asset, or none when the
    /// provider publishes no checksums.
    sums: Option<String>,
    /// The file name used by the checksum listing and staged download.
    asset: String,
    /// The base name to pull out of the zip archive.
    member: String,
}

/// Locates the media helpers and installs the ones that are missing.
pub struct Resolver {
    directory: String,
    download: bool,
    /// The directory downloads land in, removed on the way out.
    temp: Mutex<Option<PathBuf>>,
}

impl Resolver {
    pub fn new(directory: &str, download: bool) -> Resolver {
        Resolver { directory: directory.to_string(), download, temp: Mutex::new(None) }
    }

    /// Removes the archives fetched during this run.
    pub fn close(&self) {
        if let Some(directory) = self.temp.lock().unwrap().take() {
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
        let cached = Path::new(&self.directory).join(executable_name(name));
        if cached.is_file() {
            return Ok(absolute(&cached));
        }
        if !self.download {
            return Err(format!(
                "{name} was not found in {} and autoDownload is off; put a copy there or turn autoDownload on",
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
        make_executable(&target)?;
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

    /// The directory downloads land in, created on first use.
    fn work_directory(&self) -> Result<PathBuf, String> {
        let mut temp = self.temp.lock().unwrap();
        if let Some(directory) = temp.as_ref() {
            return Ok(directory.clone());
        }
        let directory = std::env::temp_dir().join(format!("rustlink-tools-{}", std::process::id()));
        fs::create_dir_all(&directory)
            .map_err(|error| format!("download directory {}: {error}", directory.display()))?;
        *temp = Some(directory.clone());
        Ok(directory)
    }
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
    if let Some(sums) = &source.sums {
        if let Err(error) = verify(&agent, sums, &source.asset, &file) {
            let _ = fs::remove_file(&file);
            return Err(error);
        }
    }
    Ok(file)
}

/// Copies a download into a file, reporting where it is every so often.
fn copy(
    reader: &mut impl Read,
    out: &mut impl Write,
    tool: &str,
    total: Option<u64>,
) -> Result<u64, String> {
    let mut buffer = vec![0u8; 64 << 10];
    let mut written = 0u64;
    let mut next = Instant::now() + PROGRESS;
    loop {
        let read = reader.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            return Ok(written);
        }
        out.write_all(&buffer[..read]).map_err(|error| error.to_string())?;
        written += read as u64;
        if total.is_some() && Instant::now() >= next {
            next = Instant::now() + PROGRESS;
            log::info!(
                "media helper download in progress tool={tool} receivedMiB={} totalMiB={}",
                written >> 20,
                total.unwrap_or(0) >> 20
            );
        }
    }
}

/// A client suited to a large file: the response deadline is generous because a
/// helper is hundreds of megabytes on a slow link.
fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .accept_encoding("identity")
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_global(None)
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .timeout_recv_body(Some(Duration::from_secs(60)))
            .http_status_as_error(false)
            .build(),
    )
}
/// Checks a download against a `<sha256>  <file>` listing.
fn verify(agent: &ureq::Agent, sums_url: &str, asset: &str, file: &Path) -> Result<(), String> {
    let list = agent
        .get(sums_url)
        .header("User-Agent", "rustlink")
        .call()
        .map_err(|error| format!("checksum list {sums_url}: {error}"))?
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

/// Finds the hash a listing gives for one file. A line is a hash, whitespace,
/// and a file name, which is the format every provider here publishes.
fn lookup_sum(list: &str, asset: &str) -> Option<String> {
    for line in list.lines() {
        let mut fields = line.split_whitespace();
        if let (Some(hash), Some(name), None) = (fields.next(), fields.next(), fields.next()) {
            if name == asset {
                return Some(hash.to_string());
            }
        }
    }
    None
}

/// The SHA-256 of a file, as lowercase hexadecimal.
fn file_sum(file: &Path) -> Result<String, String> {
    let mut handle =
        fs::File::open(file).map_err(|error| format!("{}: {error}", file.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 << 10];
    loop {
        let read = handle.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let mut out = String::with_capacity(64);
    for byte in hasher.finalize() {
        out.push_str(&format!("{byte:02x}"));
    }
    Ok(out)
}

/// Copies one member of a zip archive onto the target.
fn extract(archive: &Path, member: &str, target: &Path) -> Result<(), String> {
    let file =
        fs::File::open(archive).map_err(|error| format!("{}: {error}", archive.display()))?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|error| format!("{}: {error}", archive.display()))?;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|error| error.to_string())?;
        if entry.is_dir() {
            continue;
        }
        let name = Path::new(entry.name())
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name != member {
            continue;
        }
        return copy_into(&mut entry, target);
    }
    Err(format!("{member} is not part of {}", archive.display()))
}

/// Writes through a temporary file, so a target that is still in use is never
/// left half written.
fn copy_into(source: &mut impl Read, target: &Path) -> Result<(), String> {
    let temp = PathBuf::from(format!("{}.part", target.display()));
    let mut out =
        fs::File::create(&temp).map_err(|error| format!("{}: {error}", temp.display()))?;
    if let Err(error) = std::io::copy(source, &mut out) {
        drop(out);
        let _ = fs::remove_file(&temp);
        return Err(format!("{}: {error}", target.display()));
    }
    if let Err(error) = out.flush() {
        drop(out);
        let _ = fs::remove_file(&temp);
        return Err(format!("{}: {error}", target.display()));
    }
    drop(out);
    fs::rename(&temp, target).map_err(|error| {
        let _ = fs::remove_file(&temp);
        format!("{}: {error}", target.display())
    })
}

/// The file name a helper is installed under.
fn executable_name(name: &str) -> String {
    if cfg!(windows) {
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

/// Finds a program the way a shell would. A name with a separator in it is used
/// as a path; anything else is looked for on `PATH`.
fn look_up(program: &str) -> Option<String> {
    if program.contains('/') || program.contains('\\') {
        let path = Path::new(program);
        return path.is_file().then(|| absolute(path));
    }
    let search = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&search) {
        for name in executable_names(program) {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Some(absolute(&candidate));
            }
        }
    }
    None
}

/// The file names a program may have on this platform. Windows resolves a bare
/// name through `PATHEXT`, and every entry there starts with `.exe` in practice.
fn executable_names(program: &str) -> Vec<String> {
    if cfg!(windows) && !program.to_ascii_lowercase().ends_with(".exe") {
        vec![format!("{program}.exe"), program.to_string()]
    } else {
        vec![program.to_string()]
    }
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
                sums: Some(format!("{base}checksums.sha256")),
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
                sums: None,
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
