// Up-to-date checks: a module whose inputs did not change since its last build is not built again
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::common::system;

/// Everything the outputs of a build depend on, condensed into one hash
#[derive(Debug)]
pub struct InputFingerprint {
    hasher: DefaultHasher,
    /// holds the fingerprint of the last successful build
    file: PathBuf,
}

impl InputFingerprint {
    /// Starts with the build-script executable: another version may build differently
    pub fn new(file: PathBuf) -> Self {
        let mut hasher = DefaultHasher::new();

        if let Some(metadata) = std::env::current_exe()
            .ok()
            .and_then(|executable| fs::metadata(executable).ok())
        {
            metadata.len().hash(&mut hasher);
            metadata.modified()
                .ok()
                .hash(&mut hasher);
        }

        return InputFingerprint { hasher, file };
    }

    pub fn add(&mut self, value: impl Hash) {
        value.hash(&mut self.hasher);
    }

    /// Paths and contents of the files and of all files below the directories. A
    /// missing path counts too: creating it later changes the fingerprint.
    pub fn add_files(&mut self, paths: &[PathBuf]) -> Result<(), String> {
        return hash_files(paths, &mut self.hasher);
    }

    /// Whether all outputs exist and the last successful build had this fingerprint.
    /// If not, the stored one is deleted: a build that fails half way is not up to date.
    pub fn is_up_to_date(&self, outputs: &[&Path]) -> Result<bool, String> {
        let up_to_date = outputs.iter().all(|output| output.exists())
            && fs::read_to_string(&self.file).is_ok_and(|stored| stored.trim() == self.hex());

        if up_to_date {
            return Ok(true);
        }

        match fs::remove_file(&self.file) {
            Ok(()) => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(format!("could not delete {}: {error}", self.file.display())),
        }
    }

    /// Once the build succeeded
    pub fn store(&self) -> Result<(), String> {
        return system::write_file(&self.file, &self.hex());
    }

    fn hex(&self) -> String {
        return format!("{:016x}", self.hasher.finish());
    }
}

/// Like InputFingerprint::add_files: the key of outputs kept until their inputs change
pub fn files_hash(paths: &[PathBuf]) -> Result<u64, String> {
    let mut hasher = DefaultHasher::new();
    hash_files(paths, &mut hasher)?;

    return Ok(hasher.finish());
}

fn hash_files(paths: &[PathBuf], hasher: &mut DefaultHasher) -> Result<(), String> {
    for path in paths {
        path.hash(hasher);

        let files = if path.is_file() {
            vec![path.clone()]
        } else {
            system::files_below(&[path.clone()])
        };

        for file in files {
            file.hash(hasher);
            content_hash(&file)?.hash(hasher);
        }
    }
    return Ok(());
}

thread_local! {
    /// By path, length and modification time: the jars of a library are inputs of every module above it
    static CONTENT_HASHES: RefCell<HashMap<(PathBuf, u64, Option<SystemTime>), u64>> = RefCell::new(HashMap::new());
}

/// Read in chunks, a jar need not fit in memory
fn content_hash(file: &Path) -> Result<u64, String> {
    let error = |error: std::io::Error| format!("could not read {}: {error}", file.display());
    let metadata = fs::metadata(file).map_err(error)?;
    let key = (file.to_path_buf(), metadata.len(), metadata.modified().ok());

    if let Some(hash) = CONTENT_HASHES.with(|hashes| hashes.borrow().get(&key).copied()) {
        return Ok(hash);
    }

    let mut hasher = DefaultHasher::new();

    let mut reader = BufReader::with_capacity(1 << 16, File::open(file).map_err(error)?);
    loop {
        let chunk = reader.fill_buf().map_err(error)?;

        if chunk.is_empty() {
            break;
        }

        hasher.write(chunk);
        let length = chunk.len();
        reader.consume(length);
    }

    let hash = hasher.finish();
    CONTENT_HASHES.with(|hashes| hashes.borrow_mut().insert(key, hash));

    return Ok(hash);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint_of(paths: &[PathBuf], file: &Path) -> InputFingerprint {
        let mut fingerprint = InputFingerprint::new(file.to_path_buf());
        fingerprint.add("arguments");
        fingerprint.add_files(paths).unwrap();

        return fingerprint;
    }

    #[test]
    fn changed_inputs_are_not_up_to_date() {
        let root =
            std::env::temp_dir().join(format!("build-script-up-to-date-{}", std::process::id()));

        let _ = fs::remove_dir_all(&root);
        let sources = root.join("src");
        let output = root.join("build/app.jar");
        let fingerprint_file = root.join("build/fingerprints/app");
        system::write_file(&sources.join("a/Main.kt"), "fun main() {}").unwrap();

        let fingerprint = || fingerprint_of(&[sources.clone()], &fingerprint_file);
        assert!(
            !fingerprint().is_up_to_date(&[&output]).unwrap(),
            "never built"
        );

        system::write_file(&output, "jar").unwrap();
        fingerprint().store().unwrap();
        assert!(fingerprint().is_up_to_date(&[&output]).unwrap());

        // a changed file, a new file and a missing output each mean building again
        system::write_file(&sources.join("a/Main.kt"), "fun main() { println() }").unwrap();
        assert!(!fingerprint().is_up_to_date(&[&output]).unwrap());
        // the old fingerprint is gone, a failing build does not count as up to date
        assert!(!fingerprint_file.exists());

        fingerprint().store().unwrap();
        system::write_file(&sources.join("b/Other.kt"), "class Other").unwrap();
        assert!(!fingerprint().is_up_to_date(&[&output]).unwrap());

        fingerprint().store().unwrap();
        fs::remove_file(&output).unwrap();
        assert!(!fingerprint().is_up_to_date(&[&output]).unwrap());
        assert!(!fingerprint().is_up_to_date(&[&output]).unwrap());

        fs::remove_dir_all(&root).unwrap();
    }
}
