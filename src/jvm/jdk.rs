// JDKs: installed through the foojay API, like Gradle
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value as Json;

use crate::common::system::{self, HostArchitecture, HostOperatingSystem};
use crate::language::runtime::{
    Arguments, BuildScriptContext, FunctionParameters, Type, Value, builtin,
};

pub const DEFAULT_JAVA_VERSION: &str = "21";
pub const DEFAULT_JDK_DISTRIBUTION: &str = "temurin";

const FOOJAY_DISCOVERY_API_URL: &str = "https://api.foojay.io/disco/v3.0";

pub struct Jdk {
    pub home: PathBuf,
}

pub fn builtins() -> Vec<(&'static str, Value)> {
    return vec![builtin(
        "jdk",
        FunctionParameters::new()
            .with_default("version", Type::String, DEFAULT_JAVA_VERSION)
            .with_default("distribution", Type::String, DEFAULT_JDK_DISTRIBUTION)
            .returns(Type::String),
        jdk_directory,
    )];
}

/// Where jni.h is, for C and C++ libraries a JVM loads: include/ of the default JDK and that of its system
pub fn jni_include_dirs(project_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let include = Jdk::resolve(DEFAULT_JAVA_VERSION, DEFAULT_JDK_DISTRIBUTION, project_dir)?.home.join("include");

    let system = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };

    return Ok(vec![include.clone(), include.join(system)]);
}

pub fn join_classpath(paths: &[PathBuf]) -> Result<OsString, String> {
    return std::env::join_paths(paths).map_err(|error| format!("invalid classpath: {error}"));
}

impl Jdk {
    /// The JDK of a version like "21", installed if needed, or an existing one by its directory
    pub fn resolve(
        version_or_path: &str,
        distribution: &str, // foojay distribution name like temurin
        project_dir: &Path, // base of relative JDK dirs
    ) -> Result<Self, String> {
        let is_version = !version_or_path.is_empty()
            && version_or_path.chars()
                .all(|char| char.is_ascii_digit() || char == '.');

        if is_version {
            return install_jdk(version_or_path, distribution);
        }

        let jdk = Jdk {
            home: project_dir.join(version_or_path),
        };

        if !jdk.tool_path("java").is_file() {
            return Err(format!(
                "'{version_or_path}' is neither a java version nor a jdk directory"
            ));
        }

        return Ok(jdk);
    }

    pub fn tool_path(&self, name: &str) -> PathBuf {
        return self.home.join("bin")
            .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    }

    /// `java <options> -cp <classpath> <main class>`, the arguments of the program follow
    pub fn java_command(&self, options: &[String], classpath: &[PathBuf], main_class: &str) -> Result<Command, String> {
        let mut command = Command::new(self.tool_path("java"));
        command.args(options)
            .arg("-cp")
            .arg(join_classpath(classpath)?)
            .arg(main_class);

        return Ok(command);
    }

    /// From the `release` file: 21 for "21.0.12", 8 for "1.8.0_402"
    pub fn major_version(&self) -> Option<u32> {
        let release = fs::read_to_string(self.home.join("release")).ok()?;
        let version = release.lines()
            .find_map(|line| line.strip_prefix("JAVA_VERSION="))?
            .trim_matches('"');

        let mut parts = version.split(|char: char| !char.is_ascii_digit());

        // up to java 8 the major version came after "1."
        let major = parts.next()?
            .parse()
            .ok()?;

        if major == 1 {
            return parts.next()?
                .parse()
                .ok();
        }

        return Some(major);
    }

    /// Bytecode level of the JDK, like jvmToolchain(...) in Gradle
    pub fn jvm_target(&self) -> String {
        match self.major_version() {
            Some(8) | None => return "1.8".to_string(),
            Some(major) => return major.to_string(),
        }
    }
}

/// `jdk(...)`: the directory of the JDK, installed if needed
fn jdk_directory(context: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
    let jdk = Jdk::resolve(
        &args.get::<String>("version")?,
        &args.get::<String>("distribution")?,
        &context.project_dir,
    )?;
    return Ok(jdk.home.display().to_string().into());
}

/// Into <build home>/jdks/<distribution>-<version>-<os>-<arch>, an existing installation is used
fn install_jdk(
    java_version: &str, // "21" for the latest 21.x, or "21.0.12"
    distribution: &str,
) -> Result<Jdk, String> {
    let platform = FoojayPlatform::current()?;
    let jdks_dir = system::data_dir("jdks");

    let target = jdks_dir.join(format!(
        "{distribution}-{java_version}-{}-{}",
        platform.os, platform.arch
    ));

    if let Some(home) = find_jdk_home(&target, 0) {
        return Ok(Jdk { home });
    }

    // builds on other threads may need the same JDK, the first installs it
    return system::installing(&target, || {
        if let Some(home) = find_jdk_home(&target, 0) {
            return Ok(Jdk { home });
        }

        let package = find_jdk_package(java_version, distribution, &platform)?;
        system::log_tool(
            "jdk",
            &format!("Installing {distribution} {} ({})", package.java_version, system::format_size(package.size)),
        );

        if package.checksum.is_none() {
            println!(
                "Warning: no SHA-256 checksum available for {}",
                package.filename
            );
        }

        // installed completely before it is renamed: a half extracted jdk is never used
        let checksum = package.checksum.as_deref().map(|checksum| (256, checksum));
        system::install_archive(&package.url, &jdks_dir.join(&package.filename), checksum, &target)?;

        let Some(home) = find_jdk_home(&target, 0) else {
            return Err(format!("{} does not contain a JDK", package.filename));
        };

        return Ok(Jdk { home });
    });
}

fn find_jdk_home(
    search_dir: &Path, // an unpacked jdk or installation
    depth: usize,
) -> Option<PathBuf> {
    let candidate = Jdk {
        home: search_dir.to_path_buf(),
    };

    if candidate.tool_path("java").is_file() {
        return Some(candidate.home);
    }

    if depth >= 4 {
        return None;
    }

    let mut subdirs: Vec<PathBuf> = fs::read_dir(search_dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();

    subdirs.sort();

    return subdirs.iter()
        .find_map(|subdir| find_jdk_home(subdir, depth + 1));
}

struct FoojayPlatform {
    os: &'static str,
    arch: &'static str,
    archive_type: &'static str,
}

impl FoojayPlatform {
    fn current() -> Result<Self, String> {
        let os = match system::host_operating_system()? {
            HostOperatingSystem::MacOs => "macos",
            HostOperatingSystem::Linux => "linux",
            HostOperatingSystem::Windows => "windows",
        };

        let arch = match system::host_architecture()? {
            HostArchitecture::Aarch64 => "aarch64",
            HostArchitecture::X86_64 => "x64",
        };

        let archive_type = if os == "windows" { "zip" } else { "tar.gz" };

        return Ok(FoojayPlatform {
            os,
            arch,
            archive_type,
        });
    }
}

/// A JDK archive as the foojay Disco API describes it
struct JdkPackage {
    java_version: String, // the full version, like 21.0.12+1
    filename: String,
    size: u64, // bytes
    url: String,
    checksum: Option<String>, // SHA-256
}

/// The latest generally available release of the distribution for the
/// platform, no early access or release candidates
fn find_jdk_package(
    java_version: &str, // "21" for the latest 21.x, or "21.0.12"
    distribution: &str, // temurin or zulu
    platform: &FoojayPlatform,
) -> Result<JdkPackage, String> {
    let mut request_url = format!(
        "{FOOJAY_DISCOVERY_API_URL}/packages?version={java_version}&distribution={distribution}\
        &operating_system={}&architecture={}&archive_type={}\
        &package_type=jdk&release_status=ga&latest=available&javafx_bundled=false&directly_downloadable=true",
        platform.os, platform.arch, platform.archive_type,
    );

    if platform.os == "linux" {
        request_url.push_str("&lib_c_type=glibc");
    }

    // foojay reports unknown versions with HTTP 400 and a message in the body
    let (status, body) = system::http_get(&request_url, None)?;

    let response: Json = serde_json::from_str(&body)
        .map_err(|error| format!("foojay API responded with HTTP {status}: {error}"))?;

    let Some(package) = response.get("result").and_then(|result| result.get(0)) else {
        let message = response.get("message")
            .and_then(Json::as_str)
            .unwrap_or("");
        return Err(format!(
            "no JDK '{distribution}' {java_version} found for {}/{}: {message}",
            platform.os, platform.arch
        ));
    };

    let (status, body) = system::http_get(&foojay_string(package, &["links", "pkg_info_uri"])?, None)?;

    let package_info_response: Json = serde_json::from_str(&body)
        .map_err(|error| format!("foojay API responded with HTTP {status}: {error}"))?;

    let package_info = package_info_response.get("result")
        .and_then(|result| result.get(0))
        .ok_or("foojay response without package info")?;

    // the file name ends up in a path, so only its last component counts
    let filename = foojay_string(package, &["filename"])?;

    let filename = Path::new(&filename)
        .file_name()
        .ok_or_else(|| format!("invalid file name '{filename}'"))?
        .to_string_lossy()
        .to_string();

    let checksum = foojay_string(package_info, &["checksum"])
        .ok()
        .filter(|checksum| !checksum.is_empty());

    let is_sha256 = foojay_string(package_info, &["checksum_type"])
        .is_ok_and(|checksum_type| checksum_type == "sha256");

    return Ok(JdkPackage {
        java_version: foojay_string(package, &["java_version"])?,
        filename,
        size: package.get("size").and_then(Json::as_u64).unwrap_or(0),
        url: foojay_string(package_info, &["direct_download_uri"])?,
        checksum: checksum.filter(|_| is_sha256),
    });
}

fn foojay_string(
    response: &Json,
    json_path: &[&str], // keys into the json object, leading to the field
) -> Result<String, String> {
    let mut current = response;

    for key in json_path {
        current = current.get(key)
            .ok_or_else(|| format!("foojay response has no '{}'", json_path.join(".")))?;
    }

    return current.as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("foojay response: '{}' is not a string", json_path.join(".")));
}

#[cfg(test)]
mod tests {
    use crate::common::test_support::ScratchDir;

    use super::*;

    #[test]
    fn jdk_can_be_a_path_to_an_existing_jdk() {
        let root = ScratchDir::new("jdk");

        // layout of a macOS JDK archive
        let home = root.join("jdk-21.0.12+1/Contents/Home");
        fs::create_dir_all(home.join("bin")).unwrap();
        fs::write(
            home.join("bin")
                .join(format!("java{}", std::env::consts::EXE_SUFFIX)),
            "",
        )
        .unwrap();
        fs::write(
            home.join("release"),
            "IMPLEMENTOR=\"Eclipse Adoptium\"\nJAVA_VERSION=\"21.0.12\"\n",
        )
        .unwrap();

        assert_eq!(find_jdk_home(&root, 0), Some(home.clone()));

        let found = Jdk::resolve(
            "jdk-21.0.12+1/Contents/Home",
            DEFAULT_JDK_DISTRIBUTION,
            &root,
        )
        .unwrap();

        assert_eq!(found.home, home);
        assert_eq!(found.major_version(), Some(21));
        assert_eq!(found.jvm_target(), "21");

        fs::write(home.join("release"), "JAVA_VERSION=\"1.8.0_402\"\n").unwrap();
        assert_eq!(found.major_version(), Some(8));
        assert_eq!(found.jvm_target(), "1.8");

        let error = Jdk::resolve("does-not-exist", DEFAULT_JDK_DISTRIBUTION, &root)
            .err()
            .unwrap();

        assert!(error.contains("neither a java version"), "{error}");

    }

    /// Needs network access: cargo test -- --ignored
    #[test]
    #[ignore]
    fn finds_jdk_packages_via_foojay() {
        let platform = FoojayPlatform::current().unwrap();

        let package =
            find_jdk_package(DEFAULT_JAVA_VERSION, DEFAULT_JDK_DISTRIBUTION, &platform).unwrap();

        assert!(
            package.java_version.starts_with("21."),
            "{}",
            package.java_version
        );
        assert!(package.url.starts_with("https://"), "{}", package.url);
        assert_eq!(package.checksum.map(|checksum| checksum.len()), Some(64));

        let error = find_jdk_package("99", DEFAULT_JDK_DISTRIBUTION, &platform)
            .err()
            .unwrap();

        assert!(error.contains("no JDK"), "{error}");
    }
}
