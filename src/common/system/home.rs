// Where build-script runs and keeps its things: the host, the build directories of modules, the home
// of build-script with its data, caches and settings, programs in the PATH
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const BUILD_SCRIPT_DIR_NAME: &str = "build-script";

/// In the user's home, when neither BUILD_SCRIPT_HOME nor XDG is set
const DEFAULT_BUILD_SCRIPT_HOME_DIR_NAME: &str = ".build-script";

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HostOperatingSystem {
    MacOs,
    Linux,
    Windows,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HostArchitecture {
    Aarch64,
    X86_64,
}

pub fn host_operating_system() -> Result<HostOperatingSystem, String> {
    match std::env::consts::OS {
        "macos" => return Ok(HostOperatingSystem::MacOs),
        "linux" => return Ok(HostOperatingSystem::Linux),
        "windows" => return Ok(HostOperatingSystem::Windows),
        other => return Err(format!("the operating system {other} is not supported")),
    }
}

pub fn host_architecture() -> Result<HostArchitecture, String> {
    match std::env::consts::ARCH {
        "aarch64" => return Ok(HostArchitecture::Aarch64),
        "x86_64" => return Ok(HostArchitecture::X86_64),
        other => return Err(format!("the CPU architecture {other} is not supported")),
    }
}

/// build/ of a module, .build/ where a file is in its way: BUILD of Bazel, on a file system that
/// does not tell cases apart, like that of macOS
pub fn build_dir(module_dir: &Path) -> PathBuf {
    // a module in a checkout of git() builds in the project
    if let Some(build_dir) = crate::common::git::project_build_dir(module_dir) {
        return build_dir;
    }

    let build_dir = module_dir.join("build");

    if build_dir.exists() && !build_dir.is_dir() {
        return module_dir.join(".build");
    }

    return build_dir;
}

/// A program by its path, or by its name in the PATH. On Windows also <name>.exe.
pub fn find_program(program: &str) -> Option<PathBuf> {
    let names = [program.to_string(), format!("{program}{}", std::env::consts::EXE_SUFFIX)];

    if program.contains('/') || (cfg!(windows) && program.contains('\\')) {
        return names.iter()
            .map(PathBuf::from)
            .find(|path| path.is_file());
    }

    let path = std::env::var_os("PATH")?;
    return std::env::split_paths(&path)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file());
}

/// The home of the user: HOME, on Windows USERPROFILE
pub fn home_dir() -> Option<OsString> {
    return std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
}

/// JDKs, the Android SDK and emulators
pub fn data_dir(name: &str) -> PathBuf {
    return BuildHomeEnvironment::read().base_directory(BaseDirectory::Data).join(name);
}

/// Downloaded artifacts, like ~/.gradle/caches
pub fn cache_dir(name: &str) -> PathBuf {
    return BuildHomeEnvironment::read().base_directory(BaseDirectory::Cache).join(name);
}

/// The settings
pub fn config_dir() -> PathBuf {
    return BuildHomeEnvironment::read().base_directory(BaseDirectory::Config);
}

#[derive(Debug, Clone, Copy)]
enum BaseDirectory {
    Data,
    Cache,
    Config,
}

/// The environment variables the build home depends on
#[derive(Debug, Default)]
struct BuildHomeEnvironment {
    home: Option<OsString>,
    build_script_home: Option<OsString>,
    xdg_data_home: Option<OsString>,
    xdg_cache_home: Option<OsString>,
    xdg_config_home: Option<OsString>,
}

impl BuildHomeEnvironment {
    fn read() -> Self {
        return BuildHomeEnvironment {
            home: home_dir(),
            build_script_home: std::env::var_os("BUILD_SCRIPT_HOME"),
            xdg_data_home: std::env::var_os("XDG_DATA_HOME"),
            xdg_cache_home: std::env::var_os("XDG_CACHE_HOME"),
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME"),
        };
    }

    /// $BUILD_SCRIPT_HOME keeps everything in one directory, the caches in caches/
    /// like ~/.gradle. Without it the XDG variables decide, ~/.build-script for those not set.
    fn base_directory(&self, kind: BaseDirectory) -> PathBuf {
        let build_script_home = self.build_script_home.as_deref()
            .filter(|home| !home.is_empty())
            .map(PathBuf::from);

        if build_script_home.is_none() {
            let xdg_base_dir = match kind {
                BaseDirectory::Data => self.xdg_data_home.as_deref(),
                BaseDirectory::Cache => self.xdg_cache_home.as_deref(),
                BaseDirectory::Config => self.xdg_config_home.as_deref(),
            };

            // the specification asks to ignore relative paths
            if let Some(xdg_base_dir) = xdg_base_dir.map(PathBuf::from)
                .filter(|path| path.is_absolute())
            {
                return xdg_base_dir.join(BUILD_SCRIPT_DIR_NAME);
            }
        }

        let build_script_home = build_script_home.unwrap_or_else(|| {
            let home = self.home.as_deref().map_or(PathBuf::from("."), PathBuf::from);
            home.join(DEFAULT_BUILD_SCRIPT_HOME_DIR_NAME)
        });

        return match kind {
            BaseDirectory::Data | BaseDirectory::Config => build_script_home,
            BaseDirectory::Cache => build_script_home.join("caches"),
        };
    }
}

#[cfg(test)]
mod tests {
    use crate::common::system::{create_dir, write_file};
    use crate::common::test_support::ScratchDir;

    use super::*;

    #[test]
    fn a_file_named_build_moves_the_build_directory() {
        let dir = ScratchDir::new("build-dir");
        create_dir(&dir).unwrap();
        assert_eq!(build_dir(&dir), dir.join("build"));

        // BUILD of Bazel is build on macOS
        write_file(&dir.join("build"), "").unwrap();
        assert_eq!(build_dir(&dir), dir.join(".build"));
    }

    #[cfg(unix)]
    fn home_a() -> BuildHomeEnvironment {
        return BuildHomeEnvironment {
            home: Some("/home/a".into()),
            ..BuildHomeEnvironment::default()
        };
    }

    #[cfg(unix)]
    fn base_dirs(env: BuildHomeEnvironment) -> (PathBuf, PathBuf) {
        return (
            env.base_directory(BaseDirectory::Data),
            env.base_directory(BaseDirectory::Cache),
        );
    }

    #[cfg(unix)]
    #[test]
    fn build_home_follows_xdg_and_falls_back_to_dot_build_script() {
        assert_eq!(
            base_dirs(home_a()),
            (
                PathBuf::from("/home/a/.build-script"),
                PathBuf::from("/home/a/.build-script/caches"),
            )
        );

        assert_eq!(
            base_dirs(BuildHomeEnvironment {
                xdg_data_home: Some("/data".into()),
                xdg_cache_home: Some("/cache".into()),
                ..home_a()
            }),
            (
                PathBuf::from("/data/build-script"),
                PathBuf::from("/cache/build-script"),
            )
        );

        // each XDG variable on its own, the other one falls back
        assert_eq!(
            base_dirs(BuildHomeEnvironment {
                xdg_data_home: Some("/data".into()),
                ..home_a()
            }),
            (
                PathBuf::from("/data/build-script"),
                PathBuf::from("/home/a/.build-script/caches"),
            )
        );

        // relative paths do not count
        assert_eq!(
            base_dirs(BuildHomeEnvironment {
                xdg_cache_home: Some("cache".into()),
                ..home_a()
            })
            .1,
            PathBuf::from("/home/a/.build-script/caches")
        );

        // settings go with the data, or to $XDG_CONFIG_HOME
        assert_eq!(
            home_a().base_directory(BaseDirectory::Config),
            PathBuf::from("/home/a/.build-script")
        );
        assert_eq!(
            BuildHomeEnvironment {
                xdg_config_home: Some("/config".into()),
                ..home_a()
            }
            .base_directory(BaseDirectory::Config),
            PathBuf::from("/config/build-script")
        );

        // one directory for everything wins over XDG
        assert_eq!(
            base_dirs(BuildHomeEnvironment {
                xdg_data_home: Some("/data".into()),
                build_script_home: Some("/bs".into()),
                ..home_a()
            }),
            (PathBuf::from("/bs"), PathBuf::from("/bs/caches"),)
        );
    }
}
