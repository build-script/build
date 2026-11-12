// tool: a program downloaded once per version, checked against its SHA-256 and project.lock, for plugins
// that bring the tools of their language
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::common::settings::Settings;
use crate::common::up_to_date::hash_hex;
use crate::common::{lock, system};
use crate::language::runtime::{FunctionParameters, Type, Value, builtin};
use crate::project;

/// in every installed tool: the URL it came from and the SHA-256 of its archive
const MARKER: &str = ".build-script-tool";

/// the tools taken from BUILD_SCRIPT_TOOL_<NAME>, said once per build
static INSTALLED_ELSEWHERE: Mutex<Option<HashSet<String>>> = Mutex::new(None);

pub fn builtins() -> Vec<(&'static str, Value)> {
    return vec![builtin(
        "tool",
        FunctionParameters::new()
            .required("name", Type::String)
            .required("version", Type::String)
            .required("url", Type::String)
            .with_default("sha256", Type::String, "")
            .with_default("sha256_url", Type::String, "")
            .returns(Type::String),
        |_, args| {
            let tool = ToolArchive {
                name: args.get("name")?,
                version: args.get("version")?,
                url: args.get("url")?,
                sha256: args.get("sha256")?,
                sha256_url: args.get("sha256_url")?,
            };

            let directory = match installed_elsewhere(&tool.name)? {
                Some(directory) => directory,
                None => install(&tool, &system::data_dir("tools"), &mirrors()?)?,
            };

            return Ok(directory.display().to_string().into());
        },
    )];
}

/// What a script says of a tool, "" for what it does not say
pub struct ToolArchive {
    pub name: String,
    pub version: String,
    pub url: String,
    pub sha256: String,
    /// a file with the SHA-256, or with lines of a SHA-256 and a file name like SHASUMS256.txt
    pub sha256_url: String,
}

/// The directory of the tool below `tools_dir`, installed unless it is there. Its archive has to match
/// the SHA-256 of the lock, of the script or of `sha256_url`, in this order, the lock keeps it.
pub fn install(tool: &ToolArchive, tools_dir: &Path, mirrors: &[(String, String)]) -> Result<PathBuf, String> {
    for part in [&tool.name, &tool.version] {
        if part.is_empty() || part.contains(['/', '\\']) || part.contains("..") {
            return Err(format!("\"{part}\" cannot name a tool or its version, they name its directory"));
        }
    }

    let archive_name = archive_name(&tool.url)?;
    let key = format!("tool:{}/{}/{archive_name}", tool.name, tool.version);
    let target = tools_dir.join(&tool.name).join(format!("{}-{}", tool.version, &hash_hex(&tool.url)[..8]));

    if let Some(sha256) = installed_sha256(&target, &tool.url) {
        lock::check_sha256(&key, &sha256)?;
        return Ok(target);
    }

    return system::installing(&target, || {
        // another module may have installed it meanwhile
        if let Some(sha256) = installed_sha256(&target, &tool.url) {
            lock::check_sha256(&key, &sha256)?;
            return Ok(target.clone());
        }

        let expected = expected_sha256(tool, &key, &archive_name, mirrors)?;
        let url = mirrored(&tool.url, mirrors);
        let archive = target.with_file_name(format!("{}-{archive_name}", target.file_name().unwrap_or_default().to_string_lossy()));
        let mut actual = String::new();

        system::install_archive_checked(&url, &archive, &target, |file| {
            let sha256 = system::sha(file, 256)?;

            if let Some(expected) = &expected && *expected != sha256 {
                return Err(format!("{url} has the SHA-256 {sha256}, but {expected} was expected"));
            }

            lock::check_sha256(&key, &sha256)?;
            actual = sha256;
            return Ok(());
        })?;

        system::write_file(&target.join(MARKER), &format!("{} sha256={actual}\n", tool.url))?;
        return Ok(target.clone());
    });
}

/// The SHA-256 of the archive of an installed tool, None when it is not installed from `url`
fn installed_sha256(target: &Path, url: &str) -> Option<String> {
    let marker = std::fs::read_to_string(target.join(MARKER)).ok()?;
    let (installed_from, sha256) = marker.trim().split_once(" sha256=")?;

    return (installed_from == url).then(|| sha256.to_string());
}

/// The last part of the path of the URL: go1.27.4.darwin-arm64.tar.gz
fn archive_name(url: &str) -> Result<String, String> {
    let path = url.split(['?', '#'])
        .next()
        .unwrap_or_default();

    return match path.rsplit_once('/') {
        Some((_, name)) if !name.is_empty() => Ok(name.to_string()),
        _ => Err(format!("{url} does not end with the name of an archive")),
    };
}

/// From the lock, the script or `sha256_url`; None for a tool the lock does not know and the script
/// names no SHA-256 for, the download is taken as it is and the lock keeps its SHA-256
fn expected_sha256(tool: &ToolArchive, key: &str, archive_name: &str, mirrors: &[(String, String)]) -> Result<Option<String>, String> {
    if let Some(locked) = lock::locked_sha256(key) {
        return Ok(Some(locked));
    }

    if !tool.sha256.is_empty() {
        return Ok(Some(tool.sha256.trim().to_ascii_lowercase()));
    }

    if tool.sha256_url.is_empty() {
        return Ok(None);
    }

    let url = mirrored(&tool.sha256_url, mirrors);
    let (status, body) = system::http_get(&url, None)?;

    if status != 200 {
        return Err(format!("the SHA-256 of {} is not at {url} (HTTP {status})", tool.name));
    }

    return checksum_of(&body, archive_name)
        .map(Some)
        .ok_or_else(|| format!("{url} has no SHA-256 of {archive_name}"));
}

/// A file with only the checksum, or lines of a checksum and a file name: "<hex>  go1.27.4.tar.gz",
/// with a * before the name for binary files
fn checksum_of(content: &str, archive_name: &str) -> Option<String> {
    let is_sha256 = |text: &str| text.len() == 64 && text.chars().all(|character| character.is_ascii_hexdigit());
    let words: Vec<&str> = content.split_whitespace().collect();

    if let [single] = words.as_slice() && is_sha256(single) {
        return Some(single.to_ascii_lowercase());
    }

    return content.lines()
        .filter_map(|line| line.split_once(char::is_whitespace))
        .find(|(_, name)| name.trim().trim_start_matches('*') == archive_name)
        .map(|(checksum, _)| checksum.to_ascii_lowercase())
        .filter(|checksum| is_sha256(checksum));
}

/// The URL from the mirror whose prefix is the longest that fits, otherwise as it is
fn mirrored(url: &str, mirrors: &[(String, String)]) -> String {
    return mirrors.iter()
        .filter(|(prefix, _)| url.starts_with(prefix.as_str()))
        .max_by_key(|(prefix, _)| prefix.len())
        .map_or_else(|| url.to_string(), |(prefix, mirror)| format!("{mirror}{}", &url[prefix.len()..]));
}

/// The mirror.<URL> = <URL> settings of build-script config
fn mirrors() -> Result<Vec<(String, String)>, String> {
    return Ok(Settings::load()?
        .with_prefix(project::MIRROR)
        .into_iter()
        .map(|(prefix, mirror)| (prefix.to_string(), mirror.to_string()))
        .collect());
}

/// BUILD_SCRIPT_TOOL_GO for go: a tool installed otherwise, like in a CI image or by Nix
fn installed_elsewhere(name: &str) -> Result<Option<PathBuf>, String> {
    let variable = override_variable(name);
    let Some(directory) = std::env::var_os(&variable) else {
        return Ok(None);
    };

    let directory = PathBuf::from(directory);

    if !directory.is_dir() {
        return Err(format!("{variable} names {}, which is no directory", directory.display()));
    }

    let mut said = INSTALLED_ELSEWHERE.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if said.get_or_insert_with(HashSet::new).insert(name.to_string()) {
        system::log_tool(name, &format!("{} from {variable}", directory.display()));
    }

    return Ok(Some(directory));
}

fn override_variable(name: &str) -> String {
    let name: String = name.chars()
        .map(|character| if character.is_ascii_alphanumeric() { character.to_ascii_uppercase() } else { '_' })
        .collect();

    return format!("BUILD_SCRIPT_TOOL_{name}");
}

#[cfg(test)]
mod tests {
    use crate::common::test_support::{ScratchDir, serve_files};

    use super::*;

    /// A tar of one directory with a program, readable as text for serve_files
    fn tool_archive(top: &str, program: &str) -> String {
        let mut builder = tar::Builder::new(Vec::new());
        let content = format!("#!/bin/sh\necho {program}\n");
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, format!("{top}/bin/{program}"), content.as_bytes())
            .unwrap();

        return String::from_utf8(builder.into_inner().unwrap()).unwrap();
    }

    fn sha256_of(content: &str, directory: &Path) -> String {
        let file = directory.join("sha-input");
        system::write_file(&file, content).unwrap();
        return system::sha(&file, 256).unwrap();
    }

    #[test]
    fn tools_are_installed_once_checked_and_locked() {
        let _turn = lock::TESTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let directory = ScratchDir::new("tool");
        let archive = tool_archive("hello-1.0", "hello");
        let sha256 = sha256_of(&archive, &directory);
        let other = tool_archive("hello-1.0", "changed");

        let server = serve_files(vec![
            ("/dl/hello-1.0.tar".to_string(), archive.clone()),
            ("/dl/SHASUMS256.txt".to_string(), format!("{}  other.tar\n{sha256}  hello-1.0.tar\n", "0".repeat(64))),
            ("/mirror/changed-1.0.tar".to_string(), other),
        ]);
        let tools = directory.join("tools");
        let tool = |sha256: &str, sha256_url: &str| ToolArchive {
            name: "hello".to_string(),
            version: "1.0".to_string(),
            url: format!("{server}/dl/hello-1.0.tar"),
            sha256: sha256.to_string(),
            sha256_url: sha256_url.to_string(),
        };

        lock::open(&directory.join("project.build"), false).unwrap();

        // a wrong SHA-256 leaves nothing behind
        let error = install(&tool(&"0".repeat(64), ""), &tools, &[]).unwrap_err();
        assert!(error.contains("but 0000"), "{error}");
        assert!(!tools.join("hello").exists() || system::files_below(&[tools.join("hello")]).is_empty());

        // the list of checksums names it, the top directory becomes the tool
        let installed = install(&tool("", &format!("{server}/dl/SHASUMS256.txt")), &tools, &[]).unwrap();
        assert!(installed.join("bin/hello").is_file(), "{}", installed.display());
        assert_eq!(lock::locked_sha256("tool:hello/1.0/hello-1.0.tar"), Some(sha256.clone()));

        // installed: nothing is downloaded, not even the list
        let again = install(&tool("", "http://127.0.0.1:1/unreachable"), &tools, &[]).unwrap();
        assert_eq!(again, installed);

        // a mirror is taken: a version the lock does not know yet takes what it delivers
        let mirrors = [(format!("{server}/dl/hello-1.0.tar"), format!("{server}/mirror/changed-1.0.tar"))];
        let from_mirror = install(&ToolArchive { version: "1.0b".to_string(), ..tool("", "") }, &tools, &mirrors).unwrap();
        assert!(from_mirror.join("bin/changed").is_file());

        // the lock checks what a mirror delivers: 1.0 is locked with the original archive
        system::remove_dir(&installed).unwrap();
        let error = install(&tool("", ""), &tools, &mirrors).unwrap_err();
        assert!(error.contains("but") && error.contains(&sha256), "{error}");

        lock::save().unwrap();
        let saved = system::read_file(&directory.join("project.lock")).unwrap();
        assert!(saved.contains(&format!("tool:hello/1.0/hello-1.0.tar sha256={sha256}\n")), "{saved}");

        assert!(install(&ToolArchive { name: "../x".to_string(), ..tool("", "") }, &tools, &[]).is_err());
        lock::close();
    }

    #[test]
    fn checksums_come_alone_or_in_lists_and_mirrors_by_the_longest_prefix() {
        let sha256 = "a".repeat(64);
        assert_eq!(checksum_of(&format!("{sha256}\n"), "x.tar"), Some(sha256.clone()));
        assert_eq!(checksum_of(&format!("{}  y.tar\n{sha256} *x.tar\n", "b".repeat(64)), "x.tar"), Some(sha256.clone()));
        assert_eq!(checksum_of(&format!("{sha256}  y.tar\n"), "x.tar"), None);

        let mirrors = [
            ("https://go.dev/".to_string(), "https://a.example/".to_string()),
            ("https://go.dev/dl/".to_string(), "https://b.example/go/".to_string()),
        ];
        assert_eq!(mirrored("https://go.dev/dl/go.tar.gz", &mirrors), "https://b.example/go/go.tar.gz");
        assert_eq!(mirrored("https://nodejs.org/x", &mirrors), "https://nodejs.org/x");

        assert_eq!(archive_name("https://go.dev/dl/go1.27.4.linux-amd64.tar.gz?download=1").unwrap(), "go1.27.4.linux-amd64.tar.gz");
        assert!(archive_name("https://go.dev/dl/").is_err());
        assert_eq!(override_variable("protoc-gen"), "BUILD_SCRIPT_TOOL_PROTOC_GEN");
    }
}
