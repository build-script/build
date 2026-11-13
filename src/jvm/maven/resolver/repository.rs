// Files of the repositories: the first one that has a file, checked against its .sha1 and project.lock,
// SNAPSHOTs by their timestamped builds
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::common::{lock, system};
use crate::jvm::maven::metadata::{self, SnapshotFile};
use crate::jvm::maven::MavenRepository;

use super::*;

impl DependencyResolver {
    /// Remote files end up in the cache. A file of a SNAPSHOT version is the
    /// one of the newest build the repository has.
    pub(super) fn find_in_repositories(&mut self, path: &str) -> Result<Option<PathBuf>, String> {
        let snapshot = SnapshotFile::parse(path);

        for index in 0..self.repositories.len() {
            let url = match &self.repositories[index] {
                // mvn install keeps SNAPSHOTs under their plain name
                MavenRepository::Local(directory) => {
                    let file = directory.join(path);
                    if file.is_file() {
                        return Ok(Some(file));
                    }
                    continue;
                }
                MavenRepository::Remote { url, .. } => url.clone(),
            };

            // the build the lock keeps, else the newest
            let build_path = match &snapshot {
                Some(_) if let Some(locked) = lock::snapshot_build(path) => Some(locked),
                Some(snapshot) => self.snapshot_build_path(index, snapshot)?,
                None => None,
            };

            // timestamped builds never change, so they are cached like releases
            let path_in_repository = build_path.as_deref().unwrap_or(path);
            let file = self.cache_dir.join(path_in_repository);

            if file.is_file() {
                lock::check(path_in_repository, &file, false)?;
            } else {
                let credentials = self.repositories[index].credentials()?;
                let file_url = format!("{url}/{path_in_repository}");

                // checked before it is in the cache, a file there is never downloaded again
                let verify = |partial: &Path| {
                    verify_checksum(&file_url, partial, credentials.as_ref())?;
                    return lock::check(path_in_repository, partial, true);
                };

                if !system::download_checked(&file_url, &file, false, credentials.as_ref(), verify)? {
                    continue;
                }
            }

            if let Some(build) = &build_path {
                lock::keep_snapshot_build(path, build);
            }

            return Ok(Some(file));
        }

        return Ok(None);
    }

    /// Where the newest build of a SNAPSHOT file is in a repository, None when
    /// it has no metadata or keeps the plain name
    pub(super) fn snapshot_build_path(
        &mut self,
        index: usize,
        snapshot: &SnapshotFile,
    ) -> Result<Option<String>, String> {
        let key = (index, snapshot.directory.to_string());

        if !self.snapshot_metadata.contains_key(&key) {
            let metadata = metadata::fetch(&self.repositories[index], snapshot.directory)?;
            self.snapshot_metadata.insert(key.clone(), metadata.map(Rc::new));
        }

        let build = self.snapshot_metadata[&key].as_ref()
            .and_then(|metadata| metadata.snapshot_build(snapshot));

        return Ok(build.map(|build| snapshot.path_of_build(&build)));
    }
}

/// Against the .sha1 the repository keeps next to the file, like Maven does it. A repository
/// without one is trusted.
pub(super) fn verify_checksum(url: &str, file: &Path, credentials: Option<&system::Credentials>) -> Result<(), String> {
    let (status, body) = system::http_get(&format!("{url}.sha1"), credentials)?;
    if status != 200 {
        return Ok(());
    }

    // the hash, the hash and the file name, or SHA1(file name)= hash
    let line = body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let expected = match line.split_once("= ") {
        Some((_, hash)) => hash,
        None => line.split_whitespace().next().unwrap_or_default(),
    };

    let actual = system::sha(file, 1)?;
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(format!("{url} does not match its checksum: the repository says SHA-1 {expected}, the download has {actual}"));
    }

    return Ok(());
}
