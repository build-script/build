// Downloads over HTTPS with the certificates and proxies of the system, checked before they are kept
use std::fs;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::prelude::BASE64_STANDARD;

use super::*;

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
    return download_checked(url, target, show_progress, credentials, |_| Ok(()));
}

/// Like download, `verify` checks the file before it is in place: one that does not pass never is
pub fn download_checked(
    url: &str,
    target: &Path,
    show_progress: bool,
    credentials: Option<&Credentials>,
    verify: impl FnOnce(&Path) -> Result<(), String>,
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

    let body = response.into_body();
    let size = body.content_length();
    let name = target.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let progress = (show_progress && std::io::stderr().is_terminal()).then_some(name);

    // renamed once complete, nothing half downloaded stays behind
    let mut reader = body.into_reader();
    produce_atomically(target, |partial| {
        fs::File::create(partial)
            .and_then(|mut file| copy_with_progress(&mut reader, &mut file, size, progress))
            .map_err(|error| format!("download of {url} failed: {error}"))?;

        return verify(partial);
    })?;
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

/// `checksum`: the SHA bits (1 or 256) and the expected hex string
fn verify_checksum(url: &str, file: &Path, checksum: Option<(u32, &str)>) -> Result<(), String> {
    let Some((bits, expected)) = checksum else {
        return Ok(());
    };

    let expected = expected.trim().to_ascii_lowercase();
    let actual = sha(file, bits)?;

    if actual != expected {
        return Err(format!("checksum mismatch for {url} (expected {expected}, got {actual})"));
    }

    return Ok(());
}

/// Downloads an archive and installs what it holds into `target`: the one directory in it, or all of it
/// when there are more. An archive that does not match `checksum` is not kept.
pub fn install_archive(url: &str, archive: &Path, checksum: Option<(u32, &str)>, target: &Path) -> Result<(), String> {
    return install_archive_checked(url, archive, target, |file| verify_checksum(url, file, checksum));
}

/// Like install_archive, `verify` checks the archive before it is unpacked. Unpacked completely next to
/// `target` before it is renamed, a half installed tool is never used.
pub fn install_archive_checked(
    url: &str,
    archive: &Path,
    target: &Path,
    verify: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    if !download_checked(url, archive, true, None, verify)? {
        return Err(format!("{url} is not available"));
    }

    let unpacked = PathBuf::from(format!("{}.unpacked", target.display()));
    remove_dir(&unpacked)?;
    extract(archive, &unpacked)?;
    let _ = fs::remove_file(archive);

    let entries: Vec<PathBuf> = fs::read_dir(&unpacked)
        .map_err(|error| format!("could not read {}: {error}", unpacked.display()))?
        .flatten()
        .map(|entry| entry.path())
        .collect();

    let content = match entries.as_slice() {
        [single] if single.is_dir() => single.clone(),
        _ => unpacked.clone(),
    };

    remove_dir(target)?;
    fs::rename(&content, target)
        .map_err(|error| format!("could not move {} to {}: {error}", content.display(), target.display()))?;

    return remove_dir(&unpacked);
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
        // a Linux without CA certificates, like a slim container, gets those of Mozilla build-script brings
        #[cfg(target_os = "linux")]
        let roots = match rustls_native_certs::load_native_certs().certs.is_empty() {
            true => ureq::tls::RootCerts::WebPki,
            false => ureq::tls::RootCerts::PlatformVerifier,
        };
        #[cfg(not(target_os = "linux"))]
        let roots = ureq::tls::RootCerts::PlatformVerifier;

        let tls = ureq::tls::TlsConfig::builder()
            .root_certs(roots)
            .build();

        return ureq::Agent::config_builder()
            .http_status_as_error(false)
            .redirect_auth_headers(ureq::config::RedirectAuthHeaders::SameHost)
            .tls_config(tls)
            .user_agent(format!("build-script/{}", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
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

#[cfg(test)]
mod tests {
    use crate::common::test_support::{ScratchDir, serve_statuses};

    use super::*;

    #[test]
    fn downloads_tell_missing_files_and_missing_credentials_apart() {
        let directory = ScratchDir::new("download");
        let url = serve_statuses(vec!["200 OK", "404 Not Found", "401 Unauthorized", "200 OK"]);
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

    }
}
