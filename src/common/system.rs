// The host: where build-script keeps its data, files and directories, processes, downloads and archives
use std::collections::hash_map::DefaultHasher;
use std::ffi::OsString;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{BufReader, IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use flate2::read::GzDecoder;
use lzma_rust2::XzReader;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use super::terminal::{self, Color};

const BUILD_SCRIPT_DIR_NAME: &str = "build-script";
/// In the user's home, when neither BUILD_SCRIPT_HOME nor XDG is set
const DEFAULT_BUILD_SCRIPT_HOME_DIR_NAME: &str = ".build-script";

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HostOperatingSystem {
    MacOs,
    Linux,
    Windows,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HostArchitecture {
    Aarch64,
    X86_64,
}

pub fn host_operating_system() -> Result<HostOperatingSystem, String> {
    match std::env::consts::OS {
        "macos" => return Ok(HostOperatingSystem::MacOs),
        "linux" => return Ok(HostOperatingSystem::Linux),
        "windows" => return Ok(HostOperatingSystem::Windows),
        other => return Err(format!("the operating system {other} is not supported")),
    }
}

pub fn host_architecture() -> Result<HostArchitecture, String> {
    match std::env::consts::ARCH {
        "aarch64" => return Ok(HostArchitecture::Aarch64),
        "x86_64" => return Ok(HostArchitecture::X86_64),
        other => return Err(format!("the CPU architecture {other} is not supported")),
    }
}

/// build/ of a module, .build/ where a file is in its way: BUILD of Bazel, on a file system that
/// does not tell cases apart, like that of macOS
pub fn build_dir(module_dir: &Path) -> PathBuf {
    // a module in a checkout of git() builds in the project
    if let Some(build_dir) = super::git::project_build_dir(module_dir) {
        return build_dir;
    }

    let build_dir = module_dir.join("build");

    if build_dir.exists() && !build_dir.is_dir() {
        return module_dir.join(".build");
    }

    return build_dir;
}

/// A program by its path, or by its name in the PATH
pub fn find_program(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        return Some(PathBuf::from(program)).filter(|path| path.is_file());
    }

    let path = std::env::var_os("PATH")?;
    return std::env::split_paths(&path).map(|dir| dir.join(program)).find(|candidate| candidate.is_file());
}

/// The home of the user: HOME, on Windows USERPROFILE
pub fn home_dir() -> Option<OsString> {
    return std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
}

/// JDKs, the Android SDK and emulators
pub fn data_dir(name: &str) -> PathBuf {
    return BuildHomeEnvironment::read().base_directory(BaseDirectory::Data).join(name);
}

/// Downloaded artifacts, like ~/.gradle/caches
pub fn cache_dir(name: &str) -> PathBuf {
    return BuildHomeEnvironment::read().base_directory(BaseDirectory::Cache).join(name);
}

/// The settings
pub fn config_dir() -> PathBuf {
    return BuildHomeEnvironment::read().base_directory(BaseDirectory::Config);
}

/// A line of the build log: `[app] Compiling`, the module in orange
pub fn log(module: &str, message: &str) {
    println!("[{}] {message}", terminal::paint(module, Color::Primary));
}

/// A line about a tool build-script brings itself: `[vcpkg] Installing`, the tool in yellow
pub fn log_tool(tool: &str, message: &str) {
    println!("[{}] {message}", terminal::paint(tool, Color::Secondary));
}

/// Reads one answer from the terminal, trimmed and lowercase, empty at the end of the input
pub fn prompt(question: &str) -> Result<String, String> {
    let answer = terminal::read_line(question)?.unwrap_or_default();
    return Ok(answer.to_ascii_lowercase());
}

/// "810 MB" or "2.2 GB"
pub fn format_size(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        return format!("{:.1} GB", bytes as f64 / 1_000_000_000.0);
    }
    return format!("{} MB", bytes / 1_000_000);
}

pub fn read_file(file: &Path) -> Result<String, String> {
    return fs::read_to_string(file)
        .map_err(|error| format!("could not read {}: {error}", file.display()));
}

pub fn write_file(file: &Path, content: &str) -> Result<(), String> {
    create_parent_dir(file)?;
    return fs::write(file, content)
        .map_err(|error| format!("could not write {}: {error}", file.display()));
}

pub fn copy_file(from: &Path, to: &Path) -> Result<(), String> {
    create_parent_dir(to)?;
    return fs::copy(from, to)
        .map(|_| ())
        .map_err(|error| format!("could not copy {}: {error}", from.display()));
}

/// Every file below `from` to the same place below `to`
pub fn copy_dir_contents(from: &Path, to: &Path) -> Result<(), String> {
    for file in files_below(&[from.to_path_buf()]) {
        let relative = file.strip_prefix(from).map_err(|error| error.to_string())?;
        copy_file(&file, &to.join(relative))?;
    }
    return Ok(());
}

pub fn create_dir(directory: &Path) -> Result<(), String> {
    return fs::create_dir_all(directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()));
}

pub fn create_parent_dir(file: &Path) -> Result<(), String> {
    match file.parent() {
        Some(parent) => return create_dir(parent),
        None => return Ok(()),
    }
}

/// An empty directory, without anything an earlier build left in it
pub fn recreate_dir(directory: &Path) -> Result<(), String> {
    remove_dir(directory)?;
    return create_dir(directory);
}

pub fn remove_dir(directory: &Path) -> Result<(), String> {
    if !directory.exists() || fs::remove_dir_all(directory).is_ok() {
        return Ok(());
    }

    // extracted archives may contain directories without permissions
    make_accessible(directory);

    return fs::remove_dir_all(directory)
        .map_err(|error| format!("could not delete {}: {error}", directory.display()));
}

/// Read, write and enter for the user below the directory, like chmod -R u+rwX: archives may hold
/// directories without permissions, which cannot be deleted
#[cfg(unix)]
fn make_accessible(directory: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let mut pending = vec![directory.to_path_buf()];

    while let Some(path) = pending.pop() {
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };

        if metadata.is_symlink() {
            continue;
        }

        let mode = metadata.permissions().mode();
        let accessible = if metadata.is_dir() || mode & 0o111 != 0 { mode | 0o700 } else { mode | 0o600 };
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(accessible));

        if metadata.is_dir() {
            pending.extend(fs::read_dir(&path).into_iter().flatten().flatten().map(|entry| entry.path()));
        }
    }
}

/// Windows has no such permissions
#[cfg(not(unix))]
fn make_accessible(_directory: &Path) {}

/// Sorted
pub fn files_below(directories: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = vec![];

    let mut pending = directories.to_vec();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push(path);
            }
        }
    }

    files.sort();

    return files;
}

/// Sorted
pub fn files_with_extension(directories: &[PathBuf], extension: &str) -> Vec<PathBuf> {
    let mut files = files_below(directories);
    files.retain(|file| file.extension().is_some_and(|found| found == extension));

    return files;
}

/// Also in subdirectories
pub fn contains_files(directory: &Path) -> bool {
    let Ok(entries) = fs::read_dir(directory) else {
        return false;
    };
    return entries.flatten().any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            contains_files(&path)
        } else {
            true
        }
    });
}

/// With the output in the terminal, but no stdin: a tool must not swallow what
/// the user types for a prompt. `what` names it in errors.
pub fn run_command(command: &mut Command, what: &str) -> Result<(), String> {
    let status = command.stdin(Stdio::null())
        .status()
        .map_err(|error| {
            format!(
                "could not start {}: {error}",
                command.get_program().to_string_lossy()
            )
        })?;

    if !status.success() {
        return Err(format!("{what} failed ({status})"));
    }

    return Ok(());
}

/// Like run_command, but what the tool prints only shows when it fails
pub fn run_command_quietly(command: &mut Command, what: &str) -> Result<(), String> {
    let output = command.stdin(Stdio::null())
        .output()
        .map_err(|error| {
            format!(
                "could not start {}: {error}",
                command.get_program().to_string_lossy()
            )
        })?;

    if !output.status.success() {
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return Err(format!(
            "{what} failed ({}): {}",
            output.status,
            text.trim()
        ));
    }

    return Ok(());
}

/// Like run_command, but what the tool prints comes in one piece once it ended: compilers running at
/// the same time do not mix their messages
pub fn run_command_in_one_piece(command: &mut Command, what: &str) -> Result<(), String> {
    let output = command.stdin(Stdio::null())
        .output()
        .map_err(|error| {
            format!(
                "could not start {}: {error}",
                command.get_program().to_string_lossy()
            )
        })?;

    let mut stderr = std::io::stderr().lock();
    let _ = stderr.write_all(&output.stdout);
    let _ = stderr.write_all(&output.stderr);

    if !output.status.success() {
        return Err(format!("{what} failed ({})", output.status));
    }

    return Ok(());
}

/// Like run_command, but stopped once it ran longer than the timeout, like tests that hang
pub fn run_command_within(command: &mut Command, what: &str, timeout: Duration) -> Result<(), String> {
    let mut child = command.stdin(Stdio::null())
        .spawn()
        .map_err(|error| {
            format!(
                "could not start {}: {error}",
                command.get_program().to_string_lossy()
            )
        })?;

    let started = Instant::now();

    loop {
        let status = child.try_wait().map_err(|error| format!("{what}: {error}"))?;

        match status {
            Some(status) if status.success() => return Ok(()),
            Some(status) => return Err(format!("{what} failed ({status})")),
            None if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{what} timed out after {}s", timeout.as_secs()));
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// For servers that want a user name and password. No Debug, the password must not end up in a log.
pub struct Credentials {
    pub username: String,
    pub password: String,
}

/// False if the server does not have the file
pub fn download(
    url: &str,
    target: &Path,
    show_progress: bool,
    credentials: Option<&Credentials>,
) -> Result<bool, String> {
    create_parent_dir(target)?;

    let response = http_request(url, credentials)?;
    let status = response.status().as_u16();

    match (status, credentials) {
        (200, _) => {}
        (404 | 410, _) => return Ok(false),
        (401 | 403, None) => return Err(format!("{url} needs credentials (HTTP {status})")),
        (401 | 403, Some(_)) => return Err(format!("{url} did not accept the credentials (HTTP {status})")),
        _ => return Err(format!("download of {url} failed: HTTP {status}")),
    }

    // renamed once complete, nothing half downloaded stays behind
    let partial = partial_path(target);
    let body = response.into_body();
    let size = body.content_length();
    let name = target.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let progress = (show_progress && std::io::stderr().is_terminal()).then_some(name);

    let mut reader = body.into_reader();
    let written = fs::File::create(&partial).and_then(|mut file| copy_with_progress(&mut reader, &mut file, size, progress));

    if let Err(error) = written {
        let _ = fs::remove_file(&partial);
        return Err(format!("download of {url} failed: {error}"));
    }

    fs::rename(&partial, target)
        .map_err(|error| format!("could not save {}: {error}", target.display()))?;
    println!("Download {url}");

    return Ok(true);
}

/// Like io::copy, with the share copied behind the name of the file while it lasts
fn copy_with_progress(
    reader: &mut impl Read,
    writer: &mut impl Write,
    size: Option<u64>,
    progress: Option<String>,
) -> std::io::Result<()> {
    let mut buffer = vec![0; 1 << 16];
    let mut copied: u64 = 0;
    let mut shown = Instant::now();

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }

        writer.write_all(&buffer[..read])?;
        copied += read as u64;

        if let Some(name) = &progress && shown.elapsed() > Duration::from_millis(100) {
            let line = match size {
                Some(size) if size > 0 => format!("{name} {:>3}% of {} MB", copied * 100 / size, size / 1_000_000),
                _ => format!("{name} {} MB", copied / 1_000_000),
            };
            eprint!("\r{line}\x1b[K");
            shown = Instant::now();
        }
    }

    if progress.is_some() {
        eprint!("\r\x1b[K");
    }

    return Ok(());
}

/// `checksum`: the SHA bits (1 or 256) and the expected hex string. A file that does not match is deleted.
pub fn download_verified(
    url: &str,
    target: &Path,
    show_progress: bool,
    checksum: Option<(u32, &str)>,
) -> Result<bool, String> {
    if !download(url, target, show_progress, None)? {
        return Ok(false);
    }

    if let Some((bits, expected)) = checksum {
        let expected = expected.trim().to_ascii_lowercase();
        let actual = sha(target, bits)?;
        if actual != expected {
            let _ = fs::remove_file(target);
            return Err(format!(
                "checksum mismatch for {url} (expected {expected}, got {actual})"
            ));
        }
    }

    return Ok(true);
}

/// Downloads an archive and extracts it to `unpacked`, then deletes the archive
pub fn download_and_extract(
    url: &str,
    archive: &Path,
    checksum: Option<(u32, &str)>,
    unpacked: &Path,
) -> Result<(), String> {
    if !download_verified(url, archive, true, checksum)? {
        return Err(format!("{url} is not available"));
    }

    remove_dir(unpacked)?;
    extract(archive, unpacked)?;
    let _ = fs::remove_file(archive);

    return Ok(());
}

/// The HTTP status and the body, also for error statuses
pub fn http_get(url: &str, credentials: Option<&Credentials>) -> Result<(u16, String), String> {
    let response = http_request(url, credentials)?;
    let status = response.status().as_u16();

    let mut body = String::new();
    response.into_body()
        .into_reader()
        .read_to_string(&mut body)
        .map_err(|error| format!("response from {url}: {error}"))?;

    return Ok((status, body));
}

/// A GET, tried twice more when the connection fails or the server is busy, like with curl --retry 2.
/// Proxies come from HTTPS_PROXY and the like, certificates are trusted when the system trusts them.
fn http_request(url: &str, credentials: Option<&Credentials>) -> Result<ureq::http::Response<ureq::Body>, String> {
    // one client for every request: the connection to a repository stays open for its next files
    static CLIENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
        let tls = ureq::tls::TlsConfig::builder()
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .build();

        return ureq::Agent::config_builder()
            .http_status_as_error(false)
            .redirect_auth_headers(ureq::config::RedirectAuthHeaders::SameHost)
            .tls_config(tls)
            .user_agent(format!("build-script/{}", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(Duration::from_secs(30)))
            .build()
            .into();
    });

    let mut attempt = 0;

    loop {
        let mut request = CLIENT.get(url);

        // like with curl, a redirect to another host does not get them
        if let Some(credentials) = credentials {
            let token = BASE64_STANDARD.encode(format!("{}:{}", credentials.username, credentials.password));
            request = request.header("Authorization", format!("Basic {token}"));
        }

        let result = request.call();
        let transient = match &result {
            Ok(response) => matches!(response.status().as_u16(), 408 | 429 | 500..=599),
            Err(error) => matches!(error, ureq::Error::Io(_) | ureq::Error::Timeout(_) | ureq::Error::ConnectionFailed),
        };

        if !transient || attempt == 2 {
            return result.map_err(|error| format!("request to {url} failed: {error}"));
        }

        attempt += 1;
        std::thread::sleep(Duration::from_secs(attempt));
    }
}

/// SHA-1 or SHA-256 of the file as hex
pub fn sha(file: &Path, bits: u32) -> Result<String, String> {
    fn hex<Hash: Digest>(file: &Path) -> Result<String, String> {
        let mut reader = fs::File::open(file)
            .map_err(|error| format!("could not read {}: {error}", file.display()))?;
        let mut hash = Hash::new();
        let mut buffer = vec![0; 1 << 16];

        loop {
            let read = reader.read(&mut buffer)
                .map_err(|error| format!("could not read {}: {error}", file.display()))?;
            if read == 0 {
                break;
            }
            hash.update(&buffer[..read]);
        }

        return Ok(hash.finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect());
    }

    match bits {
        1 => return hex::<Sha1>(file),
        256 => return hex::<Sha256>(file),
        _ => return Err(format!("SHA-{bits} is neither SHA-1 nor SHA-256")),
    }
}

/// Zip archives, also .aar and .jar, and tar archives, also compressed with gzip or xz: their first
/// bytes tell them apart
pub fn extract(archive: &Path, target: &Path) -> Result<(), String> {
    create_dir(target)?;

    let failed = |error: std::io::Error| format!("could not extract {}: {error}", archive.display());
    let mut file = fs::File::open(archive).map_err(failed)?;
    let mut magic = [0; 6];
    let read = file.read(&mut magic).map_err(failed)?;
    file.seek(SeekFrom::Start(0)).map_err(failed)?;
    let reader = BufReader::new(file);

    let unpacked = match &magic[..read] {
        [b'P', b'K', ..] => return extract_zip(archive, target, |_| true),
        [0x1f, 0x8b, ..] => tar::Archive::new(GzDecoder::new(reader)).unpack(target),
        [0xfd, b'7', b'z', b'X', b'Z', 0] => tar::Archive::new(XzReader::new(reader, true)).unpack(target),
        _ => tar::Archive::new(reader).unpack(target),
    };

    return unpacked.map_err(failed);
}

/// The entries of a zip archive `keep` takes by their names, like a jar without its signature.
/// Executables stay executable, symbolic links stay links within the target.
pub fn extract_zip(archive: &Path, target: &Path, keep: impl Fn(&str) -> bool) -> Result<(), String> {
    let failed = |error: &dyn std::fmt::Display| format!("could not extract {}: {error}", archive.display());
    let mut zip = open_zip(archive)?;

    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|error| failed(&error))?;
        let name = entry.name()
            .map_err(|error| failed(&error))?
            .to_string();

        if !keep(&name) {
            continue;
        }

        // a name like ../x would be written outside of the target
        let path = entry.enclosed_name()
            .map(|relative| target.join(relative))
            .ok_or_else(|| failed(&format!("{name} is outside of the archive")))?;

        if entry.is_dir() {
            create_dir(&path)?;
            continue;
        }

        create_parent_dir(&path)?;
        let _ = fs::remove_file(&path);

        #[cfg(unix)]
        if entry.is_symlink() {
            let mut link = String::new();
            entry.read_to_string(&mut link).map_err(|error| failed(&error))?;

            // entries after a link out of the target would be written there
            use std::path::Component;
            let mut resolved = path.parent()
                .unwrap_or(target)
                .to_path_buf();
            for component in Path::new(&link).components() {
                match component {
                    Component::Normal(part) => resolved.push(part),
                    Component::ParentDir => {
                        resolved.pop();
                    }
                    Component::CurDir => {}
                    // absolute
                    Component::RootDir | Component::Prefix(_) => resolved = PathBuf::new(),
                }
            }

            if !resolved.starts_with(target) {
                return Err(failed(&format!("{name} links to {link}, outside of the archive")));
            }

            std::os::unix::fs::symlink(&link, &path).map_err(|error| failed(&error))?;
            continue;
        }

        let mut file = fs::File::create(&path).map_err(|error| failed(&error))?;
        std::io::copy(&mut entry, &mut file).map_err(|error| failed(&error))?;

        // like the tools of the Android SDK
        #[cfg(unix)]
        if entry.unix_mode().is_some_and(|mode| mode & 0o111 != 0) {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).map_err(|error| failed(&error))?;
        }
    }

    return Ok(());
}

/// Adds the files to a zip archive by their names in it, to a new one where there is none. Stored files
/// stay uncompressed, like the native libraries an app loads from its apk.
pub fn add_to_zip(archive: &Path, files: &[(String, PathBuf)], stored: bool) -> Result<(), String> {
    let failed = |error: &dyn std::fmt::Display| format!("could not write {}: {error}", archive.display());

    // renamed once complete, the entries already there copied as they are: appending would write a
    // central directory that differs from the headers aapt2 wrote, zipalign warns about that
    let partial = partial_path(archive);
    let mut zip = ZipWriter::new(fs::File::create(&partial).map_err(|error| failed(&error))?);

    if archive.is_file() {
        let mut existing = open_zip(archive)?;

        for index in 0..existing.len() {
            let entry = existing.by_index_raw(index).map_err(|error| failed(&error))?;
            zip.raw_copy_file(entry).map_err(|error| failed(&error))?;
        }
    }

    let method = if stored { CompressionMethod::Stored } else { CompressionMethod::Deflated };
    let options = SimpleFileOptions::default().compression_method(method);

    for (name, file) in files {
        zip.start_file(name, options).map_err(|error| failed(&error))?;
        let mut input = fs::File::open(file)
            .map_err(|error| format!("could not read {}: {error}", file.display()))?;
        std::io::copy(&mut input, &mut zip).map_err(|error| failed(&error))?;
    }

    zip.finish().map_err(|error| failed(&error))?;

    return fs::rename(&partial, archive).map_err(|error| failed(&error));
}

/// The files below the directory by their paths relative to it with /, as an archive names them
pub fn relative_files(directory: &Path) -> Vec<(String, PathBuf)> {
    return files_below(&[directory.to_path_buf()])
        .into_iter()
        .filter_map(|file| {
            let parts: Vec<String> = file.strip_prefix(directory)
                .ok()?
                .iter()
                .map(|part| part.to_string_lossy().to_string())
                .collect();
            return Some((parts.join("/"), file));
        })
        .collect();
}

/// The directory `produce` makes of a file that never changes, made once and kept in the cache
pub fn kept_in_cache(
    name: &str,
    file: &Path,
    produce: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let metadata = fs::metadata(file)
        .map_err(|error| format!("could not read {}: {error}", file.display()))?;

    let mut hasher = DefaultHasher::new();
    (file, metadata.len(), metadata.modified().ok()).hash(&mut hasher);
    let stem = file.file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let target = cache_dir(name).join(format!("{stem}-{:016x}", hasher.finish()));

    if target.is_dir() {
        return Ok(target);
    }

    // renamed once complete, a build that stops half way leaves nothing behind
    let partial = partial_path(&target);
    remove_dir(&partial)?;
    produce(&partial)?;
    match fs::rename(&partial, &target) {
        Ok(()) => return Ok(target),
        // another build was faster
        Err(_) if target.is_dir() => {
            remove_dir(&partial)?;
            return Ok(target);
        }
        Err(error) => {
            return Err(format!(
                "could not move {} to {}: {error}",
                partial.display(),
                target.display()
            ));
        }
    }
}

/// Where `path` is written before it is complete and renamed. A new one each
/// call: modules building at the same time may write the same file.
pub fn partial_path(path: &Path) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let next = NEXT.fetch_add(1, Ordering::Relaxed);

    return PathBuf::from(format!(
        "{}.partial-{}-{next}",
        path.display(),
        std::process::id()
    ));
}

/// One installation at a time: modules building at the same time may need the same JDK or SDK package
pub fn installing<T>(install: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    static INSTALLING: Mutex<()> = Mutex::new(());
    let _installing = INSTALLING.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    return install();
}

/// The names of the entries of a zip archive, like a jar
pub fn zip_entries(archive: &Path) -> Result<Vec<String>, String> {
    return open_zip(archive)?
        .file_names()
        .map(|name| name.map(|name| name.to_string()).map_err(|_| format!("{} is no zip archive", archive.display())))
        .collect();
}

/// One file of a zip archive, like a configuration inside a jar
pub fn read_zip_entry(archive: &Path, entry: &str) -> Result<Vec<u8>, String> {
    let mut zip = open_zip(archive)?;
    let mut file = zip.by_name(entry).map_err(|_| format!("{} has no {entry}", archive.display()))?;

    let mut content = vec![];
    file.read_to_end(&mut content)
        .map_err(|error| format!("could not read {entry} of {}: {error}", archive.display()))?;

    return Ok(content);
}

fn open_zip(archive: &Path) -> Result<ZipArchive<BufReader<fs::File>>, String> {
    let file = fs::File::open(archive)
        .map_err(|error| format!("could not read {}: {error}", archive.display()))?;

    return ZipArchive::new(BufReader::new(file)).map_err(|_| format!("{} is no zip archive", archive.display()));
}

#[derive(Debug, Clone, Copy)]
enum BaseDirectory {
    Data,
    Cache,
    Config,
}

/// The environment variables the build home depends on
#[derive(Debug, Default)]
struct BuildHomeEnvironment {
    home: Option<OsString>,
    build_script_home: Option<OsString>,
    xdg_data_home: Option<OsString>,
    xdg_cache_home: Option<OsString>,
    xdg_config_home: Option<OsString>,
}

impl BuildHomeEnvironment {
    fn read() -> Self {
        return BuildHomeEnvironment {
            home: home_dir(),
            build_script_home: std::env::var_os("BUILD_SCRIPT_HOME"),
            xdg_data_home: std::env::var_os("XDG_DATA_HOME"),
            xdg_cache_home: std::env::var_os("XDG_CACHE_HOME"),
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME"),
        };
    }

    /// $BUILD_SCRIPT_HOME keeps everything in one directory, the caches in caches/
    /// like ~/.gradle. Without it the XDG variables decide, ~/.build-script for those not set.
    fn base_directory(&self, kind: BaseDirectory) -> PathBuf {
        let build_script_home = self.build_script_home.as_deref()
            .filter(|home| !home.is_empty())
            .map(PathBuf::from);

        if build_script_home.is_none() {
            let xdg_base_dir = match kind {
                BaseDirectory::Data => self.xdg_data_home.as_deref(),
                BaseDirectory::Cache => self.xdg_cache_home.as_deref(),
                BaseDirectory::Config => self.xdg_config_home.as_deref(),
            };

            // the specification asks to ignore relative paths
            if let Some(xdg_base_dir) = xdg_base_dir.map(PathBuf::from)
                .filter(|path| path.is_absolute())
            {
                return xdg_base_dir.join(BUILD_SCRIPT_DIR_NAME);
            }
        }

        let build_script_home = build_script_home.unwrap_or_else(|| {
            let home = self.home.as_deref().map_or(PathBuf::from("."), PathBuf::from);
            home.join(DEFAULT_BUILD_SCRIPT_HOME_DIR_NAME)
        });

        return match kind {
            BaseDirectory::Data | BaseDirectory::Config => build_script_home,
            BaseDirectory::Cache => build_script_home.join("caches"),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_named_build_moves_the_build_directory() {
        let dir = std::env::temp_dir().join(format!("build-script-build-dir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        create_dir(&dir).unwrap();
        assert_eq!(build_dir(&dir), dir.join("build"));

        // BUILD of Bazel is build on macOS
        write_file(&dir.join("build"), "").unwrap();
        assert_eq!(build_dir(&dir), dir.join(".build"));
        fs::remove_dir_all(&dir).unwrap();
    }

    fn home_a() -> BuildHomeEnvironment {
        return BuildHomeEnvironment {
            home: Some("/home/a".into()),
            ..BuildHomeEnvironment::default()
        };
    }

    fn base_dirs(env: BuildHomeEnvironment) -> (PathBuf, PathBuf) {
        return (
            env.base_directory(BaseDirectory::Data),
            env.base_directory(BaseDirectory::Cache),
        );
    }

    // XDG and its absolute paths are of unix systems
    #[cfg(unix)]
    #[test]
    fn build_home_follows_xdg_and_falls_back_to_dot_build_script() {
        assert_eq!(
            base_dirs(home_a()),
            (
                PathBuf::from("/home/a/.build-script"),
                PathBuf::from("/home/a/.build-script/caches"),
            )
        );

        assert_eq!(
            base_dirs(BuildHomeEnvironment {
                xdg_data_home: Some("/data".into()),
                xdg_cache_home: Some("/cache".into()),
                ..home_a()
            }),
            (
                PathBuf::from("/data/build-script"),
                PathBuf::from("/cache/build-script"),
            )
        );

        // each XDG variable on its own, the other one falls back
        assert_eq!(
            base_dirs(BuildHomeEnvironment {
                xdg_data_home: Some("/data".into()),
                ..home_a()
            }),
            (
                PathBuf::from("/data/build-script"),
                PathBuf::from("/home/a/.build-script/caches"),
            )
        );

        // relative paths do not count
        assert_eq!(
            base_dirs(BuildHomeEnvironment {
                xdg_cache_home: Some("cache".into()),
                ..home_a()
            })
            .1,
            PathBuf::from("/home/a/.build-script/caches")
        );

        // settings go with the data, or to $XDG_CONFIG_HOME
        assert_eq!(
            home_a().base_directory(BaseDirectory::Config),
            PathBuf::from("/home/a/.build-script")
        );
        assert_eq!(
            BuildHomeEnvironment {
                xdg_config_home: Some("/config".into()),
                ..home_a()
            }
            .base_directory(BaseDirectory::Config),
            PathBuf::from("/config/build-script")
        );

        // one directory for everything wins over XDG
        assert_eq!(
            base_dirs(BuildHomeEnvironment {
                xdg_data_home: Some("/data".into()),
                build_script_home: Some("/bs".into()),
                ..home_a()
            }),
            (PathBuf::from("/bs"), PathBuf::from("/bs/caches"),)
        );
    }

    // sleep of the system
    #[cfg(unix)]
    #[test]
    fn commands_that_run_too_long_are_stopped() {
        let started = Instant::now();
        let result = run_command_within(Command::new("sleep").arg("10"), "tests", Duration::from_millis(200));

        assert!(result.is_err_and(|error| error.starts_with("tests timed out")));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(run_command_within(&mut Command::new("true"), "tests", Duration::from_secs(5)), Ok(()));
        assert!(run_command_within(&mut Command::new("false"), "tests", Duration::from_secs(5)).is_err());
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("build-script-{name}-{}", std::process::id()));
        recreate_dir(&directory).unwrap();
        return directory;
    }

    #[test]
    fn zip_archives_are_written_added_to_and_extracted() {
        let directory = scratch_dir("zip");
        write_file(&directory.join("content/a.txt"), "a").unwrap();
        write_file(&directory.join("content/lib/x86/libb.so"), "b").unwrap();

        let archive = directory.join("archive.zip");
        add_to_zip(&archive, &relative_files(&directory.join("content")), false).unwrap();
        add_to_zip(&archive, &[("c.txt".to_string(), directory.join("content/a.txt"))], true).unwrap();

        assert_eq!(zip_entries(&archive).unwrap(), ["a.txt", "lib/x86/libb.so", "c.txt"]);
        assert_eq!(read_zip_entry(&archive, "lib/x86/libb.so").unwrap(), b"b");
        assert!(read_zip_entry(&archive, "d.txt").is_err());

        extract(&archive, &directory.join("extracted")).unwrap();
        assert_eq!(read_file(&directory.join("extracted/c.txt")).unwrap(), "a");

        extract_zip(&archive, &directory.join("kept"), |name| name.ends_with(".so")).unwrap();
        assert!(directory.join("kept/lib/x86/libb.so").is_file());
        assert!(!directory.join("kept/a.txt").exists());

        fs::remove_dir_all(&directory).unwrap();
    }

    // permissions and links of unix
    #[cfg(unix)]
    #[test]
    fn extracted_executables_and_links_stay_what_they_were() {
        use std::os::unix::fs::PermissionsExt;

        let directory = scratch_dir("zip-tools");
        let archive = directory.join("tools.zip");

        let mut zip = ZipWriter::new(fs::File::create(&archive).unwrap());
        zip.start_file("bin/tool", SimpleFileOptions::default().unix_permissions(0o755)).unwrap();
        zip.write_all(b"#!/bin/sh\n").unwrap();
        zip.add_symlink("bin/alias", "tool", SimpleFileOptions::default()).unwrap();
        zip.finish().unwrap();

        extract(&archive, &directory.join("extracted")).unwrap();

        let mode = fs::metadata(directory.join("extracted/bin/tool")).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
        assert_eq!(fs::read_link(directory.join("extracted/bin/alias")).unwrap(), PathBuf::from("tool"));

        let escaping = directory.join("escaping.zip");
        let mut zip = ZipWriter::new(fs::File::create(&escaping).unwrap());
        zip.add_symlink("lib", "../../outside", SimpleFileOptions::default()).unwrap();
        zip.finish().unwrap();

        let error = extract(&escaping, &directory.join("escaped")).unwrap_err();
        assert!(error.contains("lib links to ../../outside"), "{error}");

        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn tar_archives_are_extracted_plain_and_compressed() {
        let directory = scratch_dir("tar");

        for (name, compressed) in [("plain.tar", false), ("compressed.tar.gz", true)] {
            let mut tar = tar::Builder::new(vec![]);
            let mut header = tar::Header::new_gnu();
            header.set_size(6);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, "dir/file.txt", &b"inside"[..]).unwrap();
            let bytes = tar.into_inner().unwrap();

            let bytes = match compressed {
                true => {
                    let mut gzip = flate2::write::GzEncoder::new(vec![], flate2::Compression::default());
                    gzip.write_all(&bytes).unwrap();
                    gzip.finish().unwrap()
                }
                false => bytes,
            };

            fs::write(directory.join(name), bytes).unwrap();
            extract(&directory.join(name), &directory.join(name).with_extension("extracted")).unwrap();
            assert_eq!(read_file(&directory.join(name).with_extension("extracted").join("dir/file.txt")).unwrap(), "inside");
        }

        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn sha_of_a_file_as_hex() {
        let directory = scratch_dir("sha");
        let file = directory.join("abc");
        write_file(&file, "abc").unwrap();

        assert_eq!(sha(&file, 1).unwrap(), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(sha(&file, 256).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert!(sha(&file, 512).is_err());

        fs::remove_dir_all(&directory).unwrap();
    }

    /// A server on this machine that answers one request after the other with these statuses, its
    /// body is the request it got
    fn serve(statuses: Vec<&'static str>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            for status in statuses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = vec![0; 4096];
                let read = stream.read(&mut request).unwrap();

                write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {read}\r\nConnection: close\r\n\r\n").unwrap();
                stream.write_all(&request[..read]).unwrap();
            }
        });

        return format!("http://{address}");
    }

    #[test]
    fn downloads_tell_missing_files_and_missing_credentials_apart() {
        let directory = scratch_dir("download");
        let url = serve(vec!["200 OK", "404 Not Found", "401 Unauthorized", "200 OK"]);
        let target = directory.join("file");
        let credentials = Credentials {
            username: "user".to_string(),
            password: "secret".to_string(),
        };

        assert_eq!(download(&format!("{url}/file"), &target, false, Some(&credentials)), Ok(true));
        let request = read_file(&target).unwrap();
        assert!(request.starts_with("GET /file HTTP/1.1"));
        assert!(request.lines().any(|line| line.trim_end().eq_ignore_ascii_case("authorization: Basic dXNlcjpzZWNyZXQ=")));

        assert_eq!(download(&format!("{url}/missing"), &target, false, None), Ok(false));
        assert!(download(&format!("{url}/private"), &target, false, None).is_err_and(|error| error.contains("needs credentials")));

        let (status, body) = http_get(&format!("{url}/index.json"), None).unwrap();
        assert_eq!(status, 200);
        assert!(body.starts_with("GET /index.json"));

        fs::remove_dir_all(&directory).unwrap();
    }
}
