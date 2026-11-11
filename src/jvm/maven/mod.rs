// Maven: repositories, coordinates, dependency notations and versions, like in Gradle
mod metadata;
mod pom;
mod resolver;

use std::cmp::Ordering;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::common::system;
use crate::language::runtime::{FunctionParameters, Type, Value, builtin};

pub use pom::license_report;
pub use resolver::DependencyResolver;

pub const MAVEN_CENTRAL: &str = "https://repo.maven.apache.org/maven2";
pub const GOOGLE: &str = "https://maven.google.com";

/// Guards against cyclic parents and BOM imports
const MAX_DEPTH: usize = 32;
/// Rank of known qualifiers, `RELEASE` stands for the final version
const RELEASE: u8 = 6;

/// What the resolved libraries will run on, decides Gradle module metadata variants
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetPlatform {
    Jvm,
    Android,
}

/// A dependency as written in a build script
#[derive(Debug, Clone, PartialEq)]
pub enum DependencyNotation {
    /// group:artifact:version[:classifier], the version may come from a platform
    Library(MavenCoordinate),
    /// platform(group:artifact:version), a BOM
    Platform(MavenCoordinate),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MavenCoordinate {
    pub group: String,
    pub artifact: String,
    pub version: String,
    pub classifier: Option<String>,
}

#[derive(Debug, Clone)]
pub enum MavenRepository {
    Remote {
        url: String,
        credentials: Option<RepositoryCredentials>,
    },
    Local(PathBuf),
}

/// The environment variables with the user name and password of a repository.
/// Their values are read only to download, so they are in no script and no log.
#[derive(Debug, Clone, PartialEq)]
pub struct RepositoryCredentials {
    pub username_env: String,
    pub password_env: String,
}

/// group, artifact, classifier: what belongs together in version conflicts
type MavenModuleKey = (String, String, Option<String>);
type PomKey = (String, String, String);

/// The repositories of Gradle as maven_central(), maven_local(), google() and maven(url), and platform(bom)
pub fn builtins() -> Vec<(&'static str, Value)> {
    let repository = || FunctionParameters::new().returns(Type::String);
    return vec![
        builtin("maven_central", repository(), |_, _| {
            Ok(MAVEN_CENTRAL.into())
        }),
        builtin("maven_local", repository(), |_, _| {
            let home = PathBuf::from(system::home_dir().unwrap_or_default());
            Ok(format!("file://{}", home.join(".m2/repository").display()).into())
        }),
        builtin("google", repository(), |_, _| Ok(GOOGLE.into())),
        // credentials come from environment variables, the script only names them
        builtin(
            "maven",
            FunctionParameters::new()
                .required("url", Type::String)
                .optional("username_env", Type::String)
                .optional("password_env", Type::String)
                .returns(Type::String),
            |_, args| {
                let url: String = args.get("url")?;

                match (args.get("username_env")?, args.get("password_env")?) {
                    (None, None) => return Ok(url.into()),
                    (Some(username_env), Some(password_env)) => {
                        let credentials =
                            RepositoryCredentials::new(username_env, password_env)?;
                        let repository = format!("{url} {credentials}");
                        MavenRepository::parse(&repository, Path::new(""))?;

                        return Ok(repository.into());
                    }
                    _ => return Err("username_env and password_env go together".to_string()),
                }
            },
        ),
        // a BOM whose versions apply to the other dependencies
        builtin(
            "platform",
            FunctionParameters::new()
                .required("bom", Type::String)
                .returns(Type::String),
            |_, args| {
                let bom: String = args.get("bom")?;
                MavenCoordinate::parse(&bom)?;
                Ok(format!("platform({bom})").into())
            },
        ),
    ];
}

/// The repositories argument of a build function, at least one
pub fn repositories(specs: &[String], project_dir: &Path) -> Result<Vec<MavenRepository>, String> {
    if specs.is_empty() {
        return Err("specify at least one repository, e.g. maven_central()".to_string());
    }
    return specs.iter()
        .map(|spec| MavenRepository::parse(spec, project_dir))
        .collect();
}

/// The highest version the repositories have together, a final one when there
/// is one: milestones, release candidates and SNAPSHOTs only when nothing else is there
pub fn newest_version(
    repositories: &[MavenRepository],
    coordinate: &MavenCoordinate,
) -> Result<String, String> {
    let mut versions: Vec<String> = vec![];

    for repository in repositories {
        if let Some(metadata) = metadata::fetch(repository, &coordinate.artifact_directory())? {
            versions.extend(metadata.versions().iter().cloned());
        }
    }

    let newest = |stable: bool| {
        versions.iter()
            .filter(|version| !stable || is_final(version))
            .max_by(|left, right| compare_versions(left, right))
            .cloned()
    };

    let names: Vec<String> = repositories.iter()
        .map(ToString::to_string)
        .collect();

    return newest(true).or_else(|| newest(false)).ok_or_else(|| {
        format!(
            "{}:{} is in none of the repositories ({}), the one that has it goes into the script as maven(url)",
            coordinate.group,
            coordinate.artifact,
            names.join(", ")
        )
    });
}

/// Simplified comparison in the style of Maven's ComparableVersion
pub fn compare_versions(left: &str, right: &str) -> Ordering {
    let left = version_parts(left);
    let right = version_parts(right);
    for index in 0..left.len().max(right.len()) {
        let ordering = match (left.get(index), right.get(index)) {
            (Some(l), Some(r)) => compare_parts(l, r),
            (Some(l), None) => compare_to_missing(l),
            (None, Some(r)) => compare_to_missing(r).reverse(),
            (None, None) => Ordering::Equal,
        };

        if ordering != Ordering::Equal {
            return ordering;
        }
    }

    return Ordering::Equal;
}

impl DependencyNotation {
    pub fn parse(notation: &str) -> Result<Self, String> {
        let notation = notation.trim();

        if let Some(bom) = notation.strip_prefix("platform(")
            .and_then(|rest| rest.strip_suffix(')'))
        {
            let bom = MavenCoordinate::parse(bom)?;

            if bom.version.is_empty() {
                return Err(format!("the platform {bom} needs a version"));
            }

            return Ok(DependencyNotation::Platform(bom));
        }

        return Ok(DependencyNotation::Library(MavenCoordinate::parse(
            notation,
        )?));
    }
}

impl MavenCoordinate {
    pub fn new(group: &str, artifact: &str, version: &str) -> Self {
        return MavenCoordinate {
            group: group.to_string(),
            artifact: artifact.to_string(),
            version: version.to_string(),
            classifier: None,
        };
    }

    /// group:artifact:version[:classifier], or group:artifact when a platform
    /// provides the version (the version stays empty then)
    pub fn parse(notation: &str) -> Result<Self, String> {
        let parts: Vec<&str> = notation.trim()
            .split(':')
            .collect();

        if !(2..=4).contains(&parts.len()) || parts.iter().any(|part| part.is_empty()) {
            return Err(format!(
                "'{notation}' is not a valid dependency, expected group:artifact:version"
            ));
        }

        return Ok(MavenCoordinate {
            group: parts[0].to_string(),
            artifact: parts[1].to_string(),
            version: parts.get(2)
                .map(|version| version.to_string())
                .unwrap_or_default(),
            classifier: parts.get(3).map(|classifier| classifier.to_string()),
        });
    }

    pub fn is_snapshot(&self) -> bool {
        return self.version.ends_with(metadata::SNAPSHOT);
    }

    fn module_key(&self) -> MavenModuleKey {
        return (
            self.group.clone(),
            self.artifact.clone(),
            self.classifier.clone(),
        );
    }

    fn pom_key(&self) -> PomKey {
        return (
            self.group.clone(),
            self.artifact.clone(),
            self.version.clone(),
        );
    }

    /// group/as/path/artifact, with a directory per version below
    pub fn artifact_directory(&self) -> String {
        return format!("{}/{}", self.group.replace('.', "/"), self.artifact);
    }

    fn version_directory(&self) -> String {
        return format!("{}/{}", self.artifact_directory(), self.version);
    }

    fn module_path(&self) -> String {
        return format!(
            "{}/{}-{}.module",
            self.version_directory(),
            self.artifact,
            self.version
        );
    }

    fn pom_path(&self) -> String {
        return format!(
            "{}/{}-{}.pom",
            self.version_directory(),
            self.artifact,
            self.version
        );
    }

    fn artifact_path(&self, extension: &str) -> String {
        let classifier = self.classifier.as_ref()
            .map(|classifier| format!("-{classifier}"))
            .unwrap_or_default();
        return format!(
            "{}/{}-{}{classifier}.{extension}",
            self.version_directory(),
            self.artifact,
            self.version
        );
    }

    /// Of a file a variant names in the .module file, relative to it: Guava points its Android
    /// variant at another version. None for urls that leave the directory of the artifact.
    fn variant_file_path(&self, url: &str) -> Option<String> {
        let mut segments = vec![self.version.as_str()];

        for segment in url.split('/') {
            match segment {
                ".." => {
                    segments.pop()?;
                }
                "" | "." => return None,
                name => segments.push(name),
            }
        }

        // a version directory and a file name
        if segments.len() != 2 {
            return None;
        }

        return Some(format!(
            "{}/{}",
            self.artifact_directory(),
            segments.join("/")
        ));
    }
}

impl MavenRepository {
    /// http(s) URLs are remote, maybe followed by credentials(USERNAME_ENV, PASSWORD_ENV),
    /// anything else is a directory relative to the project, optionally with file://
    pub fn parse(spec: &str, project_dir: &Path) -> Result<Self, String> {
        let (location, credentials) = match spec.split_once(" credentials(") {
            Some((location, names)) => (location, Some(RepositoryCredentials::parse(names)?)),
            None => (spec, None),
        };

        if location.starts_with("https://") || location.starts_with("http://") {
            return Ok(MavenRepository::Remote {
                url: location.trim_end_matches('/').to_string(),
                credentials,
            });
        }

        if credentials.is_some() {
            return Err(format!("{location}: only http(s) repositories take credentials"));
        }

        let path = location.strip_prefix("file://").unwrap_or(location);

        return Ok(MavenRepository::Local(project_dir.join(path)));
    }

    /// The user name and password from the environment, for repositories that need them
    pub fn credentials(&self) -> Result<Option<system::Credentials>, String> {
        let MavenRepository::Remote {
            url,
            credentials: Some(credentials),
        } = self
        else {
            return Ok(None);
        };

        let value = |name: &str| {
            std::env::var(name).map_err(|_| {
                format!(
                    "{url} takes its credentials from {} and {}, but {name} is not set",
                    credentials.username_env, credentials.password_env
                )
            })
        };

        return Ok(Some(system::Credentials {
            username: value(&credentials.username_env)?,
            password: value(&credentials.password_env)?,
        }));
    }
}

impl RepositoryCredentials {
    /// Only names of environment variables: whoever passes the password itself
    /// by mistake must not find it in an error message
    pub fn new(username_env: String, password_env: String) -> Result<Self, String> {
        let names = [("username_env", &username_env), ("password_env", &password_env)];

        for (parameter, name) in names {
            let mut chars = name.chars();
            let valid = chars.next()
                .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
                && chars.all(|char| char.is_ascii_alphanumeric() || char == '_');

            if !valid {
                return Err(format!(
                    "{parameter} takes the name of an environment variable, like REPOSITORY_USER, not its value"
                ));
            }
        }

        return Ok(RepositoryCredentials {
            username_env,
            password_env,
        });
    }

    /// `USERNAME_ENV, PASSWORD_ENV)`, what follows `credentials(`
    fn parse(names: &str) -> Result<Self, String> {
        let names = names.trim_end()
            .strip_suffix(')')
            .unwrap_or(names);

        let Some((username_env, password_env)) = names.split_once(',') else {
            return Err("credentials(...) takes the names of two environment variables".to_string());
        };

        return RepositoryCredentials::new(
            username_env.trim().to_string(),
            password_env.trim().to_string(),
        );
    }
}

impl TargetPlatform {
    /// Acceptable `org.jetbrains.kotlin.platform.type` values, best first
    fn kotlin_platforms(&self) -> &'static [&'static str] {
        match self {
            TargetPlatform::Jvm => return &["jvm"],
            TargetPlatform::Android => return &["androidJvm", "jvm"],
        }
    }

    /// How well a variant's `org.gradle.jvm.environment` fits, best first, None if it does
    /// not run here. AndroidX marks Android variants with it instead of androidJvm.
    fn jvm_environment_rank(&self, environment: Option<&str>) -> Option<usize> {
        match (self, environment) {
            (TargetPlatform::Jvm, Some("android")) => return None,
            (TargetPlatform::Jvm, _) => return Some(0),
            (TargetPlatform::Android, Some("android")) => return Some(0),
            (TargetPlatform::Android, _) => return Some(1),
        }
    }
}

impl fmt::Display for MavenCoordinate {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}:{}", self.group, self.artifact)?;

        if !self.version.is_empty() || self.classifier.is_some() {
            write!(f, ":{}", self.version)?;
        }

        if let Some(classifier) = &self.classifier {
            write!(f, ":{classifier}")?;
        }

        return Ok(());
    }
}

impl fmt::Display for MavenRepository {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            MavenRepository::Remote { url, .. } => return write!(f, "{url}"),
            MavenRepository::Local(path) => return write!(f, "{}", path.display()),
        }
    }
}

impl fmt::Display for RepositoryCredentials {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        return write!(f, "credentials({}, {})", self.username_env, self.password_env);
    }
}

/// Version ranges like [1.0,2.0) are pinned to their lower bound
fn concrete_version(version: &str) -> Result<String, String> {
    if version.contains("${") {
        return Err(format!("version '{version}' contains an unknown property"));
    }

    if version.starts_with('(') {
        return Err(format!("version range '{version}' is not supported"));
    }

    let Some(range) = version.strip_prefix('[') else {
        return Ok(version.to_string());
    };

    let lower = range.split([',', ']', ')'])
        .next()
        .unwrap_or("")
        .trim();

    if lower.is_empty() {
        return Err(format!("version range '{version}' is not supported"));
    }

    return Ok(lower.to_string());
}

#[derive(Debug)]
enum MavenVersionPart {
    Number(u64),
    Text(String),
}

fn version_parts(version: &str) -> Vec<MavenVersionPart> {
    let mut parts = vec![];
    let mut current = String::new();
    let mut numeric = false;

    let mut flush = |current: &mut String, numeric: bool| {
        if current.is_empty() {
            return;
        }

        parts.push(match numeric {
            true => MavenVersionPart::Number(current.parse().unwrap_or(u64::MAX)),
            false => MavenVersionPart::Text(current.to_ascii_lowercase()),
        });
        current.clear();
    };

    for char in version.chars() {
        if matches!(char, '.' | '-' | '_' | '+') {
            flush(&mut current, numeric);
            continue;
        }

        let is_digit = char.is_ascii_digit();

        if !current.is_empty() && is_digit != numeric {
            flush(&mut current, numeric);
        }

        numeric = is_digit;
        current.push(char);
    }

    flush(&mut current, numeric);

    return parts;
}

/// No alpha, beta, milestone, release candidate or SNAPSHOT. Unknown qualifiers
/// like the jre of Guava's 33.6.0-jre count as final.
fn is_final(version: &str) -> bool {
    return version_parts(version).iter().all(|part| match part {
        MavenVersionPart::Number(_) => true,
        MavenVersionPart::Text(text) => qualifier_rank(text).is_none_or(|rank| rank >= RELEASE),
    });
}

fn qualifier_rank(qualifier: &str) -> Option<u8> {
    return Some(match qualifier {
        "dev" | "eap" => 0,
        "alpha" | "a" => 1,
        "beta" | "b" => 2,
        "milestone" | "m" => 3,
        "rc" | "cr" | "pre" => 4,
        "snapshot" => 5,
        "ga" | "final" | "release" => RELEASE,
        "sp" => 7,
        _ => return None,
    });
}

fn compare_parts(left: &MavenVersionPart, right: &MavenVersionPart) -> Ordering {
    match (left, right) {
        (MavenVersionPart::Number(l), MavenVersionPart::Number(r)) => return l.cmp(r),
        (MavenVersionPart::Number(_), MavenVersionPart::Text(_)) => return Ordering::Greater,
        (MavenVersionPart::Text(_), MavenVersionPart::Number(_)) => return Ordering::Less,
        (MavenVersionPart::Text(l), MavenVersionPart::Text(r)) => {
            match (qualifier_rank(l), qualifier_rank(r)) {
                (Some(l), Some(r)) => return l.cmp(&r),
                (Some(_), None) => return Ordering::Less,
                (None, Some(_)) => return Ordering::Greater,
                (None, None) => return l.cmp(r),
            }
        }
    }
}

/// How an extra part compares to "nothing": 1.0.1 > 1.0 > 1.0-beta
fn compare_to_missing(part: &MavenVersionPart) -> Ordering {
    match part {
        MavenVersionPart::Number(0) => return Ordering::Equal,
        MavenVersionPart::Number(_) => return Ordering::Greater,
        MavenVersionPart::Text(text) => match qualifier_rank(text) {
            Some(rank) => return rank.cmp(&RELEASE),
            None => return Ordering::Greater,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_like_maven() {
        let compare = compare_versions;
        assert_eq!(compare("1.10.0", "1.9.0"), Ordering::Greater);
        assert_eq!(compare("2.0.0-Beta1", "2.0.0"), Ordering::Less);
        assert_eq!(compare("2.0.0-RC1", "2.0.0-Beta2"), Ordering::Greater);
        assert_eq!(compare("1.0", "1.0.0"), Ordering::Equal);
        assert_eq!(compare("1.0.1", "1.0"), Ordering::Greater);
        assert_eq!(compare("33.1.0-jre", "33.0.0-jre"), Ordering::Greater);
        assert_eq!(compare("26.3-R0.1-SNAPSHOT", "26.3-R0.1"), Ordering::Less);

        assert!(is_final("33.6.0-jre") && is_final("1.21-R0.4") && is_final("2.0.0.Final"));
        assert!(!is_final("2.3.0-Beta1") && !is_final("1.0-RC") && !is_final("26.3-R0.1-SNAPSHOT"));
    }

    #[test]
    fn repositories_name_the_environment_variables_of_their_credentials() {
        let project = Path::new("/project");

        let MavenRepository::Remote { url, credentials } = MavenRepository::parse(
            "https://nexus.example.com/repository/releases/ credentials(NEXUS_USER, NEXUS_TOKEN)",
            project,
        )
        .unwrap() else {
            panic!("a remote repository");
        };

        assert_eq!(url, "https://nexus.example.com/repository/releases");
        assert_eq!(
            credentials.unwrap().to_string(),
            "credentials(NEXUS_USER, NEXUS_TOKEN)"
        );

        // a password passed by mistake does not show up in the error
        let error = RepositoryCredentials::new("NEXUS_USER".to_string(), "s3cr3t!pw".to_string())
            .unwrap_err();
        assert!(error.starts_with("password_env takes the name"), "{error}");
        assert!(!error.contains("s3cr3t"), "{error}");

        assert!(MavenRepository::parse("libs credentials(A, B)", project).is_err());
        assert!(matches!(
            MavenRepository::parse("my libs", project).unwrap(),
            MavenRepository::Local(path) if path == Path::new("/project/my libs")
        ));

        let missing = MavenRepository::parse(
            "https://nexus.example.com credentials(BUILD_SCRIPT_TEST_UNSET_USER, BUILD_SCRIPT_TEST_UNSET_TOKEN)",
            project,
        )
        .unwrap();
        let error = missing.credentials()
            .err()
            .unwrap();
        assert!(error.ends_with("but BUILD_SCRIPT_TEST_UNSET_USER is not set"), "{error}");
    }
}
