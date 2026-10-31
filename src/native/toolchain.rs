// The compilers of C and C++ modules: those of the system, or zig, which brings clang, libc and
// libc++ for every target in one download
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use crate::common::system::{self, HostArchitecture, HostOperatingSystem};

pub const ZIG_VERSION: &str = "0.17.0";

/// What --version of each compiler said, asked once per build
static VERSIONS: Mutex<BTreeMap<Vec<String>, String>> = Mutex::new(BTreeMap::new());

/// Compiler, archiver and what they need in their environment
pub struct Toolchain {
    /// vcpkg builds the ports with zig too
    pub zig: bool,
    pub c_compiler: Vec<String>,
    pub cpp_compiler: Vec<String>,
    pub archiver: Vec<String>,
    /// zig keeps the libc++ it builds once in its cache
    pub environment: Vec<(&'static str, PathBuf)>,
}

impl Toolchain {
    /// "system": cc, c++ and ar of the system, or those CC, CXX and AR name, like make takes them.
    /// "zig": zig cc, zig c++ and zig ar.
    pub fn named(name: &str) -> Result<Self, String> {
        let from_environment = |variable: &str, default: &str| vec![std::env::var(variable).unwrap_or(default.to_string())];

        if name == "system" {
            return Ok(Toolchain {
                zig: false,
                c_compiler: from_environment("CC", "cc"),
                cpp_compiler: from_environment("CXX", "c++"),
                archiver: from_environment("AR", "ar"),
                environment: vec![],
            });
        }

        if name != "zig" {
            return Err(format!("toolchain '{name}' is neither \"system\" nor \"zig\""));
        }

        let zig = install_zig()?.display().to_string();

        return Ok(Toolchain {
            zig: true,
            c_compiler: vec![zig.clone(), "cc".to_string()],
            cpp_compiler: vec![zig.clone(), "c++".to_string()],
            archiver: vec![zig, "ar".to_string()],
            environment: vec![("ZIG_GLOBAL_CACHE_DIR", system::cache_dir("zig"))],
        });
    }

    pub fn command(&self, program: &[String]) -> Command {
        let mut command = Command::new(&program[0]);
        command.args(&program[1..]);

        for (name, value) in &self.environment {
            command.env(name, value);
        }

        return command;
    }

    /// What the compiler says to --version, empty when it does not start: compiling then tells why
    pub fn version(&self, compiler: &[String]) -> String {
        let mut versions = VERSIONS.lock().unwrap();

        return versions.entry(compiler.to_vec())
            .or_insert_with(|| {
                let output = self.command(compiler)
                    .arg("--version")
                    .stdin(Stdio::null())
                    .output();

                match output {
                    Ok(output) => return String::from_utf8_lossy(&output.stdout).trim().to_string(),
                    Err(_) => return String::new(),
                }
            })
            .clone();
    }
}

/// zig from ziglang.org into <build home>/zig/<version>, checked against the SHA-256 of its index
fn install_zig() -> Result<PathBuf, String> {
    let target = system::data_dir("zig").join(ZIG_VERSION);
    let zig = target.join("zig");

    return system::installing(|| {
        if zig.is_file() {
            return Ok(zig.clone());
        }

        let host = match (system::host_operating_system()?, system::host_architecture()?) {
            (HostOperatingSystem::MacOs, HostArchitecture::Aarch64) => "aarch64-macos",
            (HostOperatingSystem::MacOs, HostArchitecture::X86_64) => "x86_64-macos",
            (HostOperatingSystem::Linux, HostArchitecture::Aarch64) => "aarch64-linux",
            (HostOperatingSystem::Linux, HostArchitecture::X86_64) => "x86_64-linux",
            (HostOperatingSystem::Windows, _) => return Err("zig is not set up for Windows".into()),
        };

        let (status, body) = system::http_get("https://ziglang.org/download/index.json", None)?;

        if status != 200 {
            return Err(format!("ziglang.org answered {status}"));
        }

        let index: serde_json::Value =
            serde_json::from_str(&body).map_err(|error| format!("the zig index: {error}"))?;
        let release = &index[ZIG_VERSION][host];

        let (Some(url), Some(sha256)) = (release["tarball"].as_str(), release["shasum"].as_str())
        else {
            return Err(format!("zig {ZIG_VERSION} has no build for {host}"));
        };

        system::log_tool("zig", &format!("Installing zig {ZIG_VERSION}"));
        let unpacked = system::data_dir("zig").join(format!("{ZIG_VERSION}.unpacked"));
        let archive = target.with_extension("tar.xz");
        system::download_and_extract(url, &archive, Some((256, sha256)), &unpacked)?;

        // the archive holds one directory, zig-<host>-<version>
        let inner = fs::read_dir(&unpacked)
            .map_err(|error| format!("could not read {}: {error}", unpacked.display()))?
            .flatten()
            .next()
            .ok_or("the zig archive is empty")?
            .path();

        fs::rename(&inner, &target)
            .map_err(|error| format!("could not move {}: {error}", inner.display()))?;
        system::remove_dir(&unpacked)?;

        return Ok(zig.clone());
    });
}
