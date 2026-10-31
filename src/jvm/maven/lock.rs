// project.lock next to the script, like Cargo.lock: the SHA-256 of every file the build took from a Maven
// repository and the build each SNAPSHOT resolved to. The first build writes it, later ones take the same.
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::common::system;

use super::metadata::REFRESHED_AT;

const HEADER: &str = "# Written by build-script: the SHA-256 of every file the build took from a Maven repository, and the
# build a SNAPSHOT resolved to. Keep it with the script, build --refresh takes newer builds and files.
";

struct Lock {
    file: PathBuf,
    /// what the file holds, nothing with --refresh: a path and sha256=<hex> or build=<path of the build>
    locked: BTreeMap<String, String>,
    /// what this build took
    used: BTreeMap<String, String>,
}

static LOCK: Mutex<Option<Lock>> = Mutex::new(None);

/// The lock of the script, from now on downloads are checked against it
pub fn open(script: &Path) -> Result<(), String> {
    let file = script.with_extension("lock");
    let mut locked = BTreeMap::new();

    if file.is_file() && REFRESHED_AT.get().is_none() {
        let content = system::read_file(&file)?;

        for (number, line) in content.lines().enumerate() {
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }

            let Some((path, value)) = line.split_once(' ') else {
                return Err(format!("{}:{}: neither a checksum nor a build: {line}", file.display(), number + 1));
            };

            locked.insert(path.to_string(), value.trim().to_string());
        }
    }

    let mut lock = LOCK.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *lock = Some(Lock {
        file,
        locked,
        used: BTreeMap::new(),
    });

    return Ok(());
}

/// The build of a SNAPSHOT file the lock keeps, its path in the repository
pub fn snapshot_build(path: &str) -> Option<String> {
    let lock = LOCK.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let lock = lock.as_ref()?;

    return lock.used.get(path)
        .or_else(|| lock.locked.get(path))
        .and_then(|value| value.strip_prefix("build="))
        .map(str::to_string);
}

pub fn keep_snapshot_build(path: &str, build: &str) {
    let mut lock = LOCK.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(lock) = lock.as_mut() {
        lock.used.insert(path.to_string(), format!("build={build}"));
    }
}

/// A file of the repository at `path`. One the lock does not know yet is added, a download has to
/// match the lock. Cached files were checked when they came.
pub fn check(path: &str, file: &Path, downloaded: bool) -> Result<(), String> {
    let (lock_file, locked) = {
        let mut lock = LOCK.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(lock) = lock.as_mut() else {
            return Ok(());
        };

        if lock.used.contains_key(path) {
            return Ok(());
        }

        let locked = lock.locked.get(path).cloned();
        if let Some(value) = &locked && !downloaded {
            lock.used.insert(path.to_string(), value.clone());
            return Ok(());
        }

        (lock.file.clone(), locked)
    };

    // hashed without the lock, modules building at the same time check their files too
    let actual = format!("sha256={}", system::sha(file, 256)?);

    if let Some(expected) = locked && expected != actual {
        return Err(format!(
            "{path} does not match {}: it says {expected}, the download has {actual}. build --refresh takes the new file",
            lock_file.display()
        ));
    }

    let mut lock = LOCK.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(lock) = lock.as_mut() {
        lock.used.insert(path.to_string(), actual);
    }

    return Ok(());
}

/// Writes what the build took, when it changed. Of an artifact the build took in other versions the
/// lock forgets the old ones, it keeps what the build did not need this time, like test libraries.
pub fn save() -> Result<(), String> {
    let lock = LOCK.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(lock) = lock.as_ref() else {
        return Ok(());
    };

    /// group/artifact/version/file: group/artifact/version, of that group/artifact
    fn directory(path: &str) -> &str {
        return path.rsplit_once('/').map_or("", |(directory, _)| directory);
    }

    let used_versions: HashSet<&str> = lock.used.keys()
        .map(|path| directory(path))
        .collect();
    let used_artifacts: HashSet<&str> = lock.used.keys()
        .map(|path| directory(directory(path)))
        .collect();

    let mut entries = lock.used.clone();
    for (path, value) in &lock.locked {
        if used_versions.contains(directory(path)) || !used_artifacts.contains(directory(directory(path))) {
            entries.entry(path.clone()).or_insert_with(|| value.clone());
        }
    }

    if entries.is_empty() && !lock.file.exists() {
        return Ok(());
    }

    let lines: Vec<String> = entries.iter()
        .map(|(path, value)| format!("{path} {value}\n"))
        .collect();
    let content = format!("{HEADER}{}", lines.concat());

    if system::read_file(&lock.file).is_ok_and(|existing| existing == content) {
        return Ok(());
    }

    let partial = system::partial_path(&lock.file);
    system::write_file(&partial, &content)?;
    return fs::rename(&partial, &lock.file)
        .map_err(|error| format!("could not write {}: {error}", lock.file.display()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_keeps_checksums_and_builds_and_forgets_old_versions() {
        let directory = std::env::temp_dir().join(format!("build-script-lock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);

        let jar = directory.join("library.jar");
        system::write_file(&jar, "library").unwrap();
        let sha256 = system::sha(&jar, 256).unwrap();

        // an earlier build took gson 2.13, junit and a cached library
        let earlier = [
            "com/google/code/gson/gson/2.13/gson-2.13.jar sha256=00",
            "org/junit/junit/4.13/junit-4.13.jar sha256=11",
            "org/example/cached/1/cached-1.jar sha256=22",
        ];
        system::write_file(&directory.join("project.lock"), &format!("{HEADER}{}\n", earlier.join("\n"))).unwrap();
        open(&directory.join("project.build")).unwrap();

        check("com/google/code/gson/gson/2.14/gson-2.14.jar", &jar, true).unwrap();
        check("org/example/cached/1/cached-1.jar", &jar, false).unwrap();

        let error = check("org/junit/junit/4.13/junit-4.13.jar", &jar, true).unwrap_err();
        assert!(error.contains("junit-4.13.jar does not match"), "{error}");

        let snapshot = "org/example/tool/1.0-SNAPSHOT/tool-1.0-SNAPSHOT.jar";
        let build = "org/example/tool/1.0-SNAPSHOT/tool-1.0-20261101.010203-4.jar";
        keep_snapshot_build(snapshot, build);
        assert_eq!(snapshot_build(snapshot).as_deref(), Some(build));

        save().unwrap();
        let saved = system::read_file(&directory.join("project.lock")).unwrap();

        assert!(saved.contains(&format!("com/google/code/gson/gson/2.14/gson-2.14.jar sha256={sha256}\n")), "{saved}");
        assert!(!saved.contains("gson-2.13"), "{saved}");
        // not needed this time, like a test library
        assert!(saved.contains("org/junit/junit/4.13/junit-4.13.jar sha256=11\n"), "{saved}");
        // cached files are not hashed again
        assert!(saved.contains("org/example/cached/1/cached-1.jar sha256=22\n"), "{saved}");
        assert!(saved.contains(&format!("{snapshot} build={build}\n")), "{saved}");

        *LOCK.lock().unwrap() = None;
        fs::remove_dir_all(&directory).unwrap();
    }
}
