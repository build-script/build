// JDKs: installed through the foojay API, like Gradle
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use serde_json::Value as Json;

use crate::common::system::{self, HostArchitecture, HostOperatingSystem};
use crate::common::up_to_date;
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

    /// Like java_command for a tool like kotlinc or d8, its classes from an archive. Run it with ToolCommand::run.
    pub fn tool_command(&self, tool: &str, classpath: &[PathBuf], main_class: &str) -> Result<ToolCommand, String> {
        let archive = ClassArchive::new(&system::cache_dir(CLASS_ARCHIVES_DIR), self, tool, classpath);
        let command = self.java_command(&archive.options, classpath, main_class)?;

        return Ok(ToolCommand { command, archive });
    }

    /// javac, its classes from an archive. Run it with ToolCommand::run.
    pub fn javac_command(&self) -> ToolCommand {
        let archive = ClassArchive::new(&system::cache_dir(CLASS_ARCHIVES_DIR), self, "javac", &[]);
        let mut command = Command::new(self.tool_path("javac"));

        // the launcher passes options with -J on to its JVM
        command.args(archive.options.iter().map(|option| format!("-J{option}")));

        return ToolCommand { command, archive };
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

/// A JVM tool like javac, kotlinc or d8 whose classes come from an archive of class data sharing in the
/// cache: the JVM maps them instead of loading and verifying them again, kotlinc starts in half the time.
/// The first run writes the archive.
pub struct ToolCommand {
    pub command: Command,
    archive: ClassArchive,
}

impl ToolCommand {
    /// `run` runs the command, once it succeeded the archive its JVM wrote is put in place
    pub fn run<T>(mut self, run: impl FnOnce(&mut Command) -> Result<T, String>) -> Result<T, String> {
        let result = run(&mut self.command);
        self.archive.finish(result.is_ok());

        return result;
    }
}

/// In the cache, the archives of the tools
const CLASS_ARCHIVES_DIR: &str = "class-archives";

/// How long an archive stays without being used, like that of an older Kotlin version
const UNUSED_ARCHIVES_KEPT: Duration = Duration::from_secs(7 * 24 * 3600);

/// The JVM options that map the archive of a tool, or that write it when there is none yet
struct ClassArchive {
    options: Vec<String>,
    /// where the JVM writes it when it ends, and the archive that becomes
    writing: Option<(PathBuf, PathBuf)>,
}

impl ClassArchive {
    /// One for every tool, JDK and classpath in `directory`. JDKs before 13 cannot write one, classes
    /// from directories are not archived.
    fn new(directory: &Path, jdk: &Jdk, tool: &str, classpath: &[PathBuf]) -> Self {
        let none = ClassArchive { options: vec![], writing: None };

        if jdk.major_version().is_none_or(|major| major < 13) || classpath.iter().any(|entry| !entry.is_file()) {
            return none;
        }

        // an updated JDK or another version of the tool is another archive
        let stamp = |file: &Path| {
            return fs::metadata(file)
                .ok()
                .map(|metadata| (metadata.len(), metadata.modified().ok()));
        };
        let classpath: Vec<_> = classpath.iter()
            .map(|entry| (entry, stamp(entry)))
            .collect();
        let key = up_to_date::hash_hex((&jdk.home, stamp(&jdk.home.join("lib/modules")), classpath));
        let archive = directory.join(format!("{tool}-{key}.jsa"));

        // a JDK that wrote none, like one without archive of its own classes, is not asked again for a week
        if archive.with_extension("unwritten").exists() || system::create_dir(directory).is_err() {
            return none;
        }

        // its warnings, like about an archive that does not fit, would end up in the output of the tool
        let mut options = vec!["-Xlog:cds*=off".to_string()];

        if archive.is_file() {
            // its time is that of its last use, Windows lets no read only file change it
            let _ = fs::File::open(&archive).and_then(|file| file.set_modified(SystemTime::now()));

            options.push(format!("-XX:SharedArchiveFile={}", archive.display()));
            return ClassArchive { options, writing: None };
        }

        remove_unused_archives(directory);
        let partial = system::partial_path(&archive);
        options.push(format!("-XX:ArchiveClassesAtExit={}", partial.display()));

        return ClassArchive { options, writing: Some((partial, archive)) };
    }

    /// Once the JVM ended. A tool that failed may have ended before it wrote the whole archive.
    fn finish(&self, succeeded: bool) {
        let Some((partial, archive)) = &self.writing else {
            return;
        };

        match (succeeded, partial.is_file()) {
            // the archive of a build that ran at the same time may be in place and in use already
            (true, true) => {
                if fs::rename(partial, archive).is_err() {
                    remove_archive(partial);
                }
            }
            (true, false) => {
                let _ = system::write_file(&archive.with_extension("unwritten"), "");
            }
            (false, _) => remove_archive(partial),
        }
    }
}

/// Before another archive is written: each one is as large as the classes of its tool, kotlinc's 80 MB.
/// Those still in use are written again, on Windows a week after they were written.
fn remove_unused_archives(directory: &Path) {
    let entries = fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten();

    for entry in entries {
        let unused = entry.metadata()
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|used| used.elapsed().is_ok_and(|age| age > UNUSED_ARCHIVES_KEPT));

        if unused {
            remove_archive(&entry.path());
        }
    }
}

/// The JVM writes archives read only, Windows deletes no such file
fn remove_archive(file: &Path) {
    if let Ok(metadata) = fs::metadata(file) {
        let mut permissions = metadata.permissions();
        permissions.set_readonly(false);
        let _ = fs::set_permissions(file, permissions);
    }

    let _ = fs::remove_file(file);
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

    #[test]
    fn tools_map_the_archive_their_first_run_wrote() {
        let root = ScratchDir::new("class-archive");
        let home = root.join("jdk");
        system::write_file(&home.join("release"), "JAVA_VERSION=\"21.0.12\"\n").unwrap();
        system::write_file(&home.join("lib/modules"), "modules").unwrap();
        let jdk = Jdk { home: home.clone() };

        let jar = root.join("tool.jar");
        system::write_file(&jar, "classes").unwrap();
        let archives = root.join("archives");

        let first = ClassArchive::new(&archives, &jdk, "tool", &[jar.clone()]);
        let (partial, archive) = first.writing.clone().unwrap();
        assert!(first.options.contains(&format!("-XX:ArchiveClassesAtExit={}", partial.display())));

        // the JVM writes it when it ends
        system::write_file(&partial, "archive").unwrap();
        first.finish(true);
        assert!(archive.is_file());

        let next = ClassArchive::new(&archives, &jdk, "tool", &[jar.clone()]);
        assert_eq!(next.options, ["-Xlog:cds*=off".to_string(), format!("-XX:SharedArchiveFile={}", archive.display())]);
        assert!(next.writing.is_none());

        // a failed run may have stopped half way, read only like the JVM writes it
        let failed = ClassArchive::new(&archives, &jdk, "other", &[jar.clone()]);
        let (partial, archive) = failed.writing.clone().unwrap();
        system::write_file(&partial, "half").unwrap();
        let mut permissions = fs::metadata(&partial).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&partial, permissions).unwrap();
        failed.finish(false);
        assert!(!partial.exists() && !archive.exists());

        // a JDK that wrote none is not asked again
        ClassArchive::new(&archives, &jdk, "other", &[jar.clone()]).finish(true);
        assert!(ClassArchive::new(&archives, &jdk, "other", &[jar.clone()]).options.is_empty());

        assert!(ClassArchive::new(&archives, &jdk, "classes", &[archives.clone()]).options.is_empty());

        system::write_file(&home.join("release"), "JAVA_VERSION=\"11.0.2\"\n").unwrap();
        assert!(ClassArchive::new(&archives, &jdk, "tool", &[jar]).options.is_empty());

    }

    #[test]
    fn archives_unused_for_a_week_go_once_another_is_written() {
        let root = ScratchDir::new("unused-class-archives");
        let home = root.join("jdk");
        system::write_file(&home.join("release"), "JAVA_VERSION=\"21.0.12\"\n").unwrap();
        let jdk = Jdk { home };

        let jar = root.join("tool.jar");
        system::write_file(&jar, "classes").unwrap();
        let archives = root.join("archives");

        // read only like the JVM writes them
        let written = |file: &Path, used: SystemTime| {
            system::write_file(file, "archive").unwrap();
            fs::File::options()
                .write(true)
                .open(file)
                .and_then(|file| file.set_modified(used))
                .unwrap();

            let mut permissions = fs::metadata(file).unwrap().permissions();
            permissions.set_readonly(true);
            fs::set_permissions(file, permissions).unwrap();
        };
        let eight_days_ago = SystemTime::now() - Duration::from_secs(8 * 24 * 3600);

        let first = ClassArchive::new(&archives, &jdk, "tool", &[jar.clone()]);
        let (_, used) = first.writing.clone().unwrap();
        written(&used, eight_days_ago);

        // its use counts from now on
        ClassArchive::new(&archives, &jdk, "tool", &[jar.clone()]);
        let modified = fs::metadata(&used)
            .and_then(|metadata| metadata.modified())
            .unwrap();
        assert!(modified.elapsed().unwrap() < Duration::from_secs(60));

        let unused = archives.join("kotlinc-unused.jsa");
        let recent = archives.join("d8-recent.jsa");
        written(&unused, eight_days_ago);
        written(&recent, SystemTime::now());

        ClassArchive::new(&archives, &jdk, "other", &[jar]);
        assert!(!unused.exists());
        assert!(recent.exists() && used.exists());

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
