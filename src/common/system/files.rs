// Files and directories: read and written whole, written atomically, copied, removed, found below
// directories, kept in the cache
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime};

use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::common::up_to_date;

use super::*;

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

pub fn read_bytes(file: &Path) -> Result<Vec<u8>, String> {
    return fs::read(file).map_err(|error| format!("could not read {}: {error}", file.display()));
}

/// Writes only what changed: a header written again would compile everything that includes it again.
/// Renamed into place, nobody reads half of it.
pub fn write_file_if_changed(file: &Path, content: &str) -> Result<(), String> {
    if fs::read_to_string(file).is_ok_and(|existing| existing == content) {
        return Ok(());
    }

    return produce_atomically(file, |partial| write_file(partial, content));
}

/// `produce` writes the file or directory at a partial path next to `target`, which is then renamed to it:
/// a build that stops half way leaves nothing half written behind. A file replaces the one there, a
/// directory another build finished meanwhile stays.
pub fn produce_atomically(target: &Path, produce: impl FnOnce(&Path) -> Result<(), String>) -> Result<(), String> {
    create_parent_dir(target)?;
    let partial = partial_path(target);

    if let Err(error) = produce(&partial) {
        let _ = fs::remove_file(&partial);
        let _ = remove_dir(&partial);
        return Err(error);
    }

    match fs::rename(&partial, target) {
        Ok(()) => return Ok(()),
        Err(_) if target.is_dir() => return remove_dir(&partial),
        Err(error) => return Err(format!("could not move {} to {}: {error}", partial.display(), target.display())),
    }
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

/// The absolute path without links. On Windows without the \\?\ that canonicalize puts in front of
/// drive paths, Java and the tools of the Android SDK do not understand it.
pub fn canonical_path(path: &Path) -> std::io::Result<PathBuf> {
    let canonical = fs::canonicalize(path)?;

    if let Some(drive_path) = canonical.to_str().and_then(|text| text.strip_prefix(r"\\?\"))
        && !drive_path.starts_with(r"UNC\")
    {
        return Ok(PathBuf::from(drive_path));
    }

    return Ok(canonical);
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

/// The directory `produce` makes of a file that never changes, made once and kept in the cache
pub fn kept_in_cache(
    name: &str,
    file: &Path,
    produce: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let metadata = fs::metadata(file)
        .map_err(|error| format!("could not read {}: {error}", file.display()))?;

    let key = up_to_date::hash_hex((file, metadata.len(), metadata.modified().ok()));
    let stem = file.file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let target = cache_dir(name).join(format!("{stem}-{key}"));

    if target.is_dir() {
        return Ok(target);
    }

    // another build may be faster, its directory stays
    produce_atomically(&target, produce)?;
    return Ok(target);
}

/// Where `path` is written before it is complete and renamed. A new one each
/// call: modules building at the same time may write the same file. Those of builds
/// that stopped half way, unchanged for an hour, are removed.
pub fn partial_path(path: &Path) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let next = NEXT.fetch_add(1, Ordering::Relaxed);

    let name = path.file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let prefix = format!("{name}.partial-");
    let entries = path.parent()
        .and_then(|parent| fs::read_dir(parent).ok())
        .into_iter()
        .flatten()
        .flatten();

    for entry in entries {
        let abandoned = entry.metadata()
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| modified.elapsed().is_ok_and(|age| age > Duration::from_secs(3600)));

        let name = entry.file_name();
        if !abandoned || !name.to_string_lossy().starts_with(&prefix) {
            continue;
        }

        match entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
            true => {
                let _ = remove_dir(&entry.path());
            }
            false => {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    return PathBuf::from(format!(
        "{}.partial-{}-{next}",
        path.display(),
        std::process::id()
    ));
}

/// What a cache keeps unused, like the dex of a library version no app needs anymore
const UNUSED_CACHE_ENTRIES_KEPT: Duration = Duration::from_secs(7 * 24 * 3600);

/// Sets the time of a file or directory in a cache to now: that of its last use, which
/// `remove_unused_cache_entries` goes by.
pub fn mark_used(entry: &Path) {
    let mut options = fs::OpenOptions::new();

    // Windows opens directories only with backup semantics, and read only files only to write
    // their attributes
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        options.access_mode(FILE_WRITE_ATTRIBUTES).custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
    }

    #[cfg(not(windows))]
    options.read(true);

    let _ = options.open(entry).and_then(|file| file.set_modified(SystemTime::now()));
}

/// Removes what in a cache directory was not used for a week, before something new goes into it.
/// A directory is renamed first: a build stopped while it is removed leaves none half removed.
pub fn remove_unused_cache_entries(directory: &Path) {
    let entries = fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten();

    for entry in entries {
        let unused = entry.metadata()
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|used| used.elapsed().is_ok_and(|age| age > UNUSED_CACHE_ENTRIES_KEPT));

        if !unused {
            continue;
        }

        let path = entry.path();
        if !entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
            remove_file_even_read_only(&path);
            continue;
        }

        // Windows renames no directory with a file in use, it stays
        let removing = PathBuf::from(format!("{}.removing-{}", path.display(), std::process::id()));
        if fs::rename(&path, &removing).is_ok() {
            let _ = remove_dir(&removing);
        }
    }
}

/// Like the class archives the JVM writes: Windows deletes no read only file
pub fn remove_file_even_read_only(file: &Path) {
    if let Ok(metadata) = fs::metadata(file) {
        let mut permissions = metadata.permissions();
        permissions.set_readonly(false);
        let _ = fs::set_permissions(file, permissions);
    }

    let _ = fs::remove_file(file);
}

/// One installation into `target` at a time: modules building at the same time may need the same JDK or SDK
/// package. Others go on, a vcpkg install of an hour holds back no JDK.
pub fn installing<T>(target: &Path, install: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    static INSTALLING: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

    let lock = INSTALLING.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry(target.to_path_buf())
        .or_default()
        .clone();
    let _installing = lock.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    return install();
}

#[cfg(test)]
mod tests {
    use crate::common::test_support::ScratchDir;

    use super::*;

    #[test]
    fn partial_files_of_stopped_builds_are_removed() {
        let directory = ScratchDir::new("partial");
        let target = directory.join("library.jar");
        let abandoned = directory.join("library.jar.partial-1-0");
        let running = directory.join("library.jar.partial-2-0");
        let other = directory.join("other.jar.partial-1-0");

        for file in [&abandoned, &running, &other] {
            write_file(file, "part").unwrap();
        }

        let two_hours_ago = std::time::SystemTime::now() - Duration::from_secs(7200);
        for file in [&abandoned, &other] {
            fs::File::options()
                .write(true)
                .open(file)
                .and_then(|file| file.set_modified(two_hours_ago))
                .unwrap();
        }

        let partial = partial_path(&target);
        assert!(partial.starts_with(&directory));
        assert!(!abandoned.exists());
        assert!(running.exists());
        assert!(other.exists());

    }

    #[test]
    fn sha_of_a_file_as_hex() {
        let directory = ScratchDir::new("sha");
        let file = directory.join("abc");
        write_file(&file, "abc").unwrap();

        assert_eq!(sha(&file, 1).unwrap(), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(sha(&file, 256).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert!(sha(&file, 512).is_err());

    }

    #[test]
    fn cache_entries_unused_for_a_week_are_removed() {
        let cache = ScratchDir::new("unused-cache-entries");
        let eight_days_ago = SystemTime::now() - Duration::from_secs(8 * 24 * 3600);
        let last_used = |entry: &Path, used: SystemTime| {
            fs::File::open(entry)
                .and_then(|file| file.set_modified(used))
                .unwrap();
        };

        // read only like the class archives the JVM writes
        let unused_file = cache.join("kotlinc-unused.jsa");
        write_file(&unused_file, "archive").unwrap();
        let mut permissions = fs::metadata(&unused_file).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&unused_file, permissions).unwrap();
        last_used(&unused_file, eight_days_ago);

        let unused_directory = cache.join("unused-library");
        write_file(&unused_directory.join("dex.zip"), "dex").unwrap();
        last_used(&unused_directory, eight_days_ago);

        let used_directory = cache.join("used-library");
        write_file(&used_directory.join("dex.zip"), "dex").unwrap();
        last_used(&used_directory, eight_days_ago);
        mark_used(&used_directory);

        let recent_file = cache.join("d8-recent.jsa");
        write_file(&recent_file, "archive").unwrap();

        remove_unused_cache_entries(&cache);

        let remaining: Vec<_> = fs::read_dir(&*cache)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(remaining.len(), 2, "{remaining:?}");
        assert!(used_directory.join("dex.zip").is_file() && recent_file.is_file());

    }
}
