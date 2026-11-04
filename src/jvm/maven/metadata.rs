// maven-metadata.xml: the versions of an artifact, and which build a SNAPSHOT version stands for
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::common::system;

use super::MavenRepository;

/// How long what a repository said about versions holds, like the 24 hours
/// Gradle waits before it looks for a newer SNAPSHOT
const EXPIRY: Duration = Duration::from_secs(24 * 60 * 60);
pub const SNAPSHOT: &str = "-SNAPSHOT";

/// Set by `--refresh`: whatever was fetched before counts as expired
pub(super) static REFRESHED_AT: OnceLock<SystemTime> = OnceLock::new();

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct MavenMetadata {
    versioning: Versioning,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Versioning {
    versions: Versions,
    snapshot: Snapshot,
    snapshot_versions: SnapshotVersions,
}

/// Of an artifact: every version the repository has
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Versions {
    version: Vec<String>,
}

/// The newest build, the file names follow from it in metadata without <snapshotVersions>
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Snapshot {
    timestamp: Option<String>,
    build_number: Option<String>,
    /// the files keep the plain -SNAPSHOT name
    local_copy: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct SnapshotVersions {
    snapshot_version: Vec<SnapshotVersion>,
}

/// The version in the name of one file: 26.3-R0.1-20261004.024323-9
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SnapshotVersion {
    classifier: Option<String>,
    extension: String,
    value: String,
}

/// A file in the directory of a SNAPSHOT version, under its plain name:
/// org/spigotmc/spigot-api/26.3-R0.1-SNAPSHOT/spigot-api-26.3-R0.1-SNAPSHOT.jar
#[derive(Debug, PartialEq)]
pub struct SnapshotFile<'a> {
    pub directory: &'a str,
    artifact: &'a str,
    version: &'a str,
    classifier: Option<&'a str>,
    extension: &'a str,
}

/// From now on, metadata fetched before counts as expired
pub fn refresh() {
    let _ = REFRESHED_AT.set(SystemTime::now());
}

/// The maven-metadata.xml in `directory` of a repository. Remote ones are
/// asked again once a day, also when they have none.
pub fn fetch(
    repository: &MavenRepository,
    directory: &str,
) -> Result<Option<MavenMetadata>, String> {
    return fetch_from(&system::cache_dir("maven-metadata"), repository, directory);
}

/// Like fetch, with the answers of remote repositories kept in `cache`
fn fetch_from(
    cache: &Path,
    repository: &MavenRepository,
    directory: &str,
) -> Result<Option<MavenMetadata>, String> {
    let path = format!("{directory}/maven-metadata.xml");

    let url = match repository {
        // mvn install writes maven-metadata-local.xml
        MavenRepository::Local(root) => {
            for name in ["maven-metadata-local.xml", "maven-metadata.xml"] {
                let file = root.join(directory).join(name);
                if file.is_file() {
                    return read_metadata_file(&file).map(Some);
                }
            }
            return Ok(None);
        }
        MavenRepository::Remote { url, .. } => url,
    };

    // per repository, each one has versions and builds of its own
    let cached = cache.join(repository_directory(url)).join(&path);

    let missing = missing_marker(&cached);

    if !is_expired(&missing) {
        return Ok(None);
    }

    if is_expired(&cached) {
        let credentials = repository.credentials()?;

        match system::download(
            &format!("{url}/{path}"),
            &cached,
            false,
            credentials.as_ref(),
        ) {
            Ok(true) => {
                let _ = std::fs::remove_file(&missing);
            }
            Ok(false) => {
                system::write_file(&missing, "")?;
                let _ = std::fs::remove_file(&cached);
                return Ok(None);
            }
            // offline, the last answer has to do
            Err(error) if cached.is_file() => {
                println!("Using the cached {path}, {error}");
            }
            Err(error) => return Err(error),
        }
    }

    return read_metadata_file(&cached).map(Some);
}

fn read_metadata_file(file: &Path) -> Result<MavenMetadata, String> {
    return MavenMetadata::parse(&system::read_file(file)?)
        .map_err(|error| format!("{} is not valid Maven metadata: {error}", file.display()));
}

/// A directory name for a repository: hub.spigotmc.org_nexus_repository_snapshots
fn repository_directory(url: &str) -> String {
    let location = url.split_once("://").map_or(url, |(_, location)| location);

    return location.chars()
        .map(
            |char| match char.is_ascii_alphanumeric() || matches!(char, '.' | '-') {
                true => char,
                false => '_',
            },
        )
        .collect();
}

/// Of files that may change, like metadata: whether to fetch them again
fn is_expired(file: &Path) -> bool {
    let Ok(modified) = file.metadata().and_then(|metadata| metadata.modified()) else {
        return true;
    };

    if REFRESHED_AT.get()
        .is_some_and(|refreshed| modified < *refreshed)
    {
        return true;
    }

    return modified.elapsed().is_ok_and(|age| age > EXPIRY);
}

fn missing_marker(cached: &Path) -> PathBuf {
    let mut name = cached.as_os_str().to_owned();
    name.push(".missing");
    return PathBuf::from(name);
}

impl MavenMetadata {
    pub fn parse(content: &str) -> Result<Self, String> {
        return quick_xml::de::from_str(content).map_err(|error| error.to_string());
    }

    /// Of an artifact, in the order the repository lists them
    pub fn versions(&self) -> &[String] {
        return &self.versioning.versions.version;
    }

    /// The version in the name of the newest build of the file, None when it
    /// keeps its plain name
    pub fn snapshot_build(&self, file: &SnapshotFile) -> Option<String> {
        let versioning = &self.versioning;

        if let Some(build) = versioning.snapshot_versions.snapshot_version.iter()
            .find(|build| {
                build.classifier.as_deref() == file.classifier && build.extension == file.extension
            })
        {
            return Some(build.value.clone());
        }

        let snapshot = &versioning.snapshot;

        if snapshot.local_copy.as_deref() == Some("true") {
            return None;
        }

        let (Some(timestamp), Some(build_number)) = (&snapshot.timestamp, &snapshot.build_number)
        else {
            return None;
        };

        let release = file.version.strip_suffix(SNAPSHOT)?;

        return Some(format!("{release}-{timestamp}-{build_number}"));
    }
}

impl<'a> SnapshotFile<'a> {
    /// None for files of other versions, and for files a .module names with the build in it
    pub fn parse(path: &'a str) -> Option<Self> {
        let (directory, name) = path.rsplit_once('/')?;
        let (artifact_directory, version) = directory.rsplit_once('/')?;
        let artifact = artifact_directory.rsplit('/').next()?;

        if !version.ends_with(SNAPSHOT) {
            return None;
        }

        let rest = name.strip_prefix(artifact)?
            .strip_prefix('-')?
            .strip_prefix(version)?;

        let (classifier, extension) = match rest.strip_prefix('-') {
            Some(classified) => {
                let (classifier, extension) = classified.split_once('.')?;
                (Some(classifier), extension)
            }
            None => (None, rest.strip_prefix('.')?),
        };

        return Some(SnapshotFile {
            directory,
            artifact,
            version,
            classifier,
            extension,
        });
    }

    /// The path of the file in the build: spigot-api-26.3-R0.1-20261004.024323-9.jar
    pub fn path_of_build(&self, build: &str) -> String {
        let classifier = self.classifier.map(|classifier| format!("-{classifier}"))
            .unwrap_or_default();

        return format!(
            "{}/{}-{build}{classifier}.{}",
            self.directory, self.artifact, self.extension
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const METADATA: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<metadata modelVersion="1.1.0">
  <groupId>org.spigotmc</groupId>
  <artifactId>spigot-api</artifactId>
  <version>26.3-R0.1-SNAPSHOT</version>
  <versioning>
    <snapshot>
      <timestamp>20261004.024323</timestamp>
      <buildNumber>9</buildNumber>
    </snapshot>
    <lastUpdated>20261004024323</lastUpdated>
    <snapshotVersions>
      <snapshotVersion>
        <extension>jar</extension>
        <value>26.3-R0.1-20261004.024323-9</value>
        <updated>20261004024323</updated>
      </snapshotVersion>
      <snapshotVersion>
        <classifier>shaded</classifier>
        <extension>jar</extension>
        <value>26.3-R0.1-20261003.120000-8</value>
        <updated>20261003120000</updated>
      </snapshotVersion>
    </snapshotVersions>
  </versioning>
</metadata>"#;

    #[test]
    fn snapshot_files_are_named_after_their_newest_build() {
        let metadata = MavenMetadata::parse(METADATA).unwrap();
        let build = |path: &str| {
            let file = SnapshotFile::parse(path).unwrap();
            metadata.snapshot_build(&file)
                .map(|build| file.path_of_build(&build))
        };

        let directory = "org/spigotmc/spigot-api/26.3-R0.1-SNAPSHOT";

        assert_eq!(
            build(&format!("{directory}/spigot-api-26.3-R0.1-SNAPSHOT.jar")).unwrap(),
            format!("{directory}/spigot-api-26.3-R0.1-20261004.024323-9.jar")
        );
        assert_eq!(
            build(&format!(
                "{directory}/spigot-api-26.3-R0.1-SNAPSHOT-shaded.jar"
            ))
            .unwrap(),
            format!("{directory}/spigot-api-26.3-R0.1-20261003.120000-8-shaded.jar")
        );

        // no <snapshotVersion> for the pom: the newest build stands for every file
        assert_eq!(
            build(&format!("{directory}/spigot-api-26.3-R0.1-SNAPSHOT.pom")).unwrap(),
            format!("{directory}/spigot-api-26.3-R0.1-20261004.024323-9.pom")
        );

        // releases, and files that name their build already, stay as they are
        assert_eq!(
            SnapshotFile::parse("org/spigotmc/spigot-api/26.3/spigot-api-26.3.jar"),
            None
        );
        assert_eq!(
            SnapshotFile::parse(&format!(
                "{directory}/spigot-api-26.3-R0.1-20261004.024323-9.jar"
            )),
            None
        );

        let local = MavenMetadata::parse(
            "<metadata><versioning><snapshot><localCopy>true</localCopy></snapshot></versioning></metadata>",
        )
        .unwrap();
        let jar = format!("{directory}/spigot-api-26.3-R0.1-SNAPSHOT.jar");
        assert_eq!(
            local.snapshot_build(&SnapshotFile::parse(&jar).unwrap()),
            None
        );
    }

    #[test]
    fn metadata_expires_and_missing_metadata_is_remembered() {
        let directory =
            std::env::temp_dir().join(format!("build-script-metadata-{}", std::process::id()));
        let cached = directory.join("maven-metadata.xml");
        let _ = std::fs::remove_dir_all(&directory);

        assert!(is_expired(&cached));

        system::write_file(&cached, METADATA).unwrap();
        assert!(!is_expired(&cached));

        // what a repository did not have is not asked for again until it expires
        let repository = MavenRepository::Remote {
            url: "http://127.0.0.1:9".to_string(),
            credentials: None,
        };
        let cache = directory.join("cache");
        let cached = cache.join(repository_directory("http://127.0.0.1:9")).join("dev/test/nothing/maven-metadata.xml");
        system::write_file(&missing_marker(&cached), "").unwrap();
        assert!(fetch_from(&cache, &repository, "dev/test/nothing").unwrap().is_none());

        std::fs::remove_dir_all(&directory).unwrap();

        // local repositories have their own name for it
        let local = std::env::temp_dir().join(format!("build-script-local-{}", std::process::id()));
        system::write_file(
            &local.join("dev/test/lib/maven-metadata-local.xml"),
            "<metadata><versioning><versions><version>1.0</version><version>1.1</version></versions></versioning></metadata>",
        )
        .unwrap();
        let metadata = fetch(&MavenRepository::Local(local.clone()), "dev/test/lib")
            .unwrap()
            .unwrap();
        assert_eq!(metadata.versions(), ["1.0", "1.1"]);
        std::fs::remove_dir_all(&local).unwrap();
    }
}
