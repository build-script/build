// What tests share: a directory of their own that is gone afterwards, servers on this machine
use std::io::{Read, Write};
use std::ops::Deref;
use std::path::{Path, PathBuf};

use super::system;

/// build-script-<name>-<process> in the temporary directory, empty at first and removed when the test
/// ends, also when it fails
pub struct ScratchDir(PathBuf);

impl ScratchDir {
    pub fn new(name: &str) -> Self {
        let directory = std::env::temp_dir().join(format!("build-script-{name}-{}", std::process::id()));
        system::recreate_dir(&directory).unwrap();
        return ScratchDir(directory);
    }
}

impl Deref for ScratchDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        return &self.0;
    }
}

impl AsRef<Path> for ScratchDir {
    fn as_ref(&self) -> &Path {
        return &self.0;
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = system::remove_dir(&self.0);
    }
}

/// A server that answers one request after the other with these statuses, its body is the request it got
pub fn serve_statuses(statuses: Vec<&'static str>) -> String {
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

/// A server with these files, by path and content, every other path is missing
pub fn serve_files(files: Vec<(String, String)>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut request = vec![0; 4096];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            let path = request.split_whitespace()
                .nth(1)
                .unwrap_or_default();

            let (status, content) = match files.iter().find(|(file, _)| file == path) {
                Some((_, content)) => ("200 OK", content.as_str()),
                None => ("404 Not Found", ""),
            };
            write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{content}", content.len()).unwrap();
        }
    });

    return format!("http://{address}");
}
