// The compilers of C and C++ modules: those of the system, or zig, which brings clang, libc and
// libc++ for every target in one download
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use crate::common::system::{self, HostArchitecture, HostOperatingSystem};
use crate::common::target::{OperatingSystem, Target};

pub const ZIG_VERSION: &str = "0.17.0";

/// What --version of each compiler said, asked once per build
static VERSIONS: Mutex<BTreeMap<Vec<String>, String>> = Mutex::new(BTreeMap::new());

/// Compiler, archiver and what they need in their environment
pub struct Toolchain {
    /// vcpkg builds the ports with zig too
    pub zig: bool,
    /// what it builds for: the machine itself, or with zig and the NDK another system
    pub target: Target,
    pub c_compiler: Vec<String>,
    pub cpp_compiler: Vec<String>,
    pub archiver: Vec<String>,
    /// zig keeps the libc++ it builds once in its cache
    pub environment: Vec<(&'static str, PathBuf)>,
}

impl Toolchain {
    /// "system": cc, c++ and ar of the system, on Windows clang, clang++ and llvm-ar, or those CC, CXX
    /// and AR name, like make takes them. "zig": zig cc, zig c++ and zig ar.
    pub fn named(name: &str) -> Result<Self, String> {
        let from_environment = |variable: &str, default: &str| vec![std::env::var(variable).unwrap_or(default.to_string())];

        if name == "system" {
            let [c_compiler, cpp_compiler, archiver] = system_compilers()?;

            return Ok(Toolchain {
                zig: false,
                target: Target::host(),
                c_compiler: from_environment("CC", &c_compiler),
                cpp_compiler: from_environment("CXX", &cpp_compiler),
                archiver: from_environment("AR", &archiver),
                environment: vec![],
            });
        }

        if name != "zig" {
            return Err(format!("toolchain '{name}' is none of \"auto\", \"system\" and \"zig\""));
        }

        let zig = install_zig()?.display().to_string();

        return Ok(Toolchain {
            zig: true,
            target: Target::host(),
            c_compiler: vec![zig.clone(), "cc".to_string()],
            cpp_compiler: vec![zig.clone(), "c++".to_string()],
            archiver: vec![zig, "ar".to_string()],
            environment: vec![("ZIG_GLOBAL_CACHE_DIR", system::cache_dir("zig"))],
        });
    }

    /// The compilers of a module for the target the script is evaluated for. "auto": those of the system for
    /// this machine, zig for another desktop system. "system" builds for this machine alone, "zig" for every
    /// desktop system.
    pub fn for_target(name: &str, target: Option<Target>) -> Result<Self, String> {
        let Some(target) = target else {
            return Toolchain::named(if name == "auto" { "system" } else { name });
        };

        if target.os == OperatingSystem::Android {
            return Err(format!("C and C++ for {} build with the NDK, for the Android apps that pack them", target.name()));
        }

        if name == "system" {
            return Err(format!("toolchain = \"system\" builds for this machine, \"auto\" or \"zig\" for {}", target.name()));
        }

        let triple = target.zig_triple().ok_or_else(|| format!("zig builds for no {}", target.name()))?;
        let mut zig = Toolchain::named("zig")?;
        zig.target = target;
        zig.c_compiler.extend(["-target".to_string(), triple.clone()]);
        zig.cpp_compiler.extend(["-target".to_string(), triple]);

        return Ok(zig);
    }

    pub fn command(&self, program: &[String]) -> Command {
        let mut command = Command::new(&program[0]);
        command.args(&program[1..]);

        for (name, value) in &self.environment {
            command.env(name, value);
        }

        return command;
    }

    /// The program with its arguments. On Windows a command line has at most 32767 characters, the flags of
    /// abseil are more: then the arguments go into the response file, which clang and llvm-ar read like
    /// their command line. Quoted like for a shell: llvm-ar would take those of Windows.
    pub fn command_with_arguments(&self, program: &[String], arguments: &[String], response_file: &Path) -> Result<Command, String> {
        let length: usize = arguments.iter()
            .map(|argument| argument.len() + 3)
            .sum();

        if !cfg!(windows) || length < 30000 {
            let mut command = self.command(program);
            command.args(arguments);
            return Ok(command);
        }

        let quoted: Vec<String> = arguments.iter()
            .map(|argument| format!("\"{}\"", argument.replace('\\', "\\\\").replace('"', "\\\"")))
            .collect();
        system::write_file(response_file, &quoted.join("\n"))?;

        let mut command = self.command(program);
        command.arg("--rsp-quoting=posix")
            .arg(format!("@{}", response_file.display()));
        return Ok(command);
    }

    /// What the compiler says to --version, empty when it does not start: compiling then tells why
    pub fn version(&self, compiler: &[String]) -> String {
        let mut versions = VERSIONS.lock().unwrap();

        return versions.entry(compiler.to_vec())
            .or_insert_with(|| {
                let output = system::command_output(self.command(compiler).arg("--version"), "--version");
                return output.map(|output| output.trim().to_string())
                    .unwrap_or_default();
            })
            .clone();
    }
}

/// The C compiler, the C++ compiler and the archiver of the system. Windows has no cc: there clang, with
/// the ABI of MSVC, it links with the C library and the linker of Visual Studio.
pub fn system_compilers() -> Result<[String; 3], String> {
    if !cfg!(windows) {
        return Ok(["cc", "c++", "ar"].map(String::from));
    }

    match clang_dir() {
        Some(bin) => return Ok(["clang", "clang++", "llvm-ar"].map(|tool| bin.join(format!("{tool}.exe")).display().to_string())),
        // the compilers CC and CXX name need none
        None if std::env::var_os("CC").is_some() && std::env::var_os("CXX").is_some() => {
            return Ok(["clang", "clang++", "llvm-ar"].map(String::from));
        }
        None => {
            return Err("C and C++ build with clang on Windows: install LLVM, like with winget install LLVM.LLVM, or the C++ Clang tools of Visual Studio. CC and CXX name other compilers".into());
        }
    }
}

/// The directory of clang on Windows: of the PATH, of LLVM or of Visual Studio
fn clang_dir() -> Option<PathBuf> {
    if let Some(clang) = system::find_program("clang") {
        return clang.parent().map(Path::to_path_buf);
    }

    let llvm = PathBuf::from(std::env::var_os("ProgramFiles")?).join(r"LLVM\bin");

    if llvm.join("clang.exe").is_file() {
        return Some(llvm);
    }

    // the C++ Clang tools of Visual Studio
    let vswhere = PathBuf::from(std::env::var_os("ProgramFiles(x86)")?).join(r"Microsoft Visual Studio\Installer\vswhere.exe");
    let architecture = if cfg!(target_arch = "aarch64") { "ARM64" } else { "x64" };
    let mut find = Command::new(vswhere);
    find.args(["-latest", "-products", "*", "-find"])
        .arg(format!(r"VC\Tools\Llvm\{architecture}\bin\clang.exe"));
    let output = system::command_output(&mut find, "vswhere").ok()?;
    let clang = PathBuf::from(output.lines().next()?.trim());

    if !clang.is_file() {
        return None;
    }

    return clang.parent().map(Path::to_path_buf);
}

/// zig from ziglang.org into <build home>/zig/<version>, checked against the SHA-256 of its index
fn install_zig() -> Result<PathBuf, String> {
    let target = system::data_dir("zig").join(ZIG_VERSION);
    let zig = target.join(format!("zig{}", std::env::consts::EXE_SUFFIX));

    return system::installing(&target, || {
        if zig.is_file() {
            return Ok(zig.clone());
        }

        let host = match (system::host_operating_system()?, system::host_architecture()?) {
            (HostOperatingSystem::MacOs, HostArchitecture::Aarch64) => "aarch64-macos",
            (HostOperatingSystem::MacOs, HostArchitecture::X86_64) => "x86_64-macos",
            (HostOperatingSystem::Linux, HostArchitecture::Aarch64) => "aarch64-linux",
            (HostOperatingSystem::Linux, HostArchitecture::X86_64) => "x86_64-linux",
            (HostOperatingSystem::Windows, HostArchitecture::Aarch64) => "aarch64-windows",
            (HostOperatingSystem::Windows, HostArchitecture::X86_64) => "x86_64-windows",
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
        // a zip on Windows, it holds one directory, zig-<host>-<version>
        let archive = target.with_extension(if cfg!(windows) { "zip" } else { "tar.xz" });
        system::install_archive(url, &archive, Some((256, sha256)), &target)?;

        return Ok(zig.clone());
    });
}

#[cfg(test)]
mod tests {
    use super::Toolchain;
    use crate::common::target::Target;

    #[test]
    fn the_toolchain_follows_the_target() {
        // "auto" on this machine: the compilers of the system, like "system"
        let auto = Toolchain::for_target("auto", None).unwrap();
        assert!(!auto.zig);
        assert_eq!(auto.target, Target::host());

        // those of the system build for this machine alone, the NDK builds for Android
        let other = Target::named("windows-x86_64").filter(|target| !target.is_host()).unwrap_or(Target::named("linux-aarch64").unwrap());
        let error = Toolchain::for_target("system", Some(other)).err().unwrap();
        assert!(error.contains("\"auto\" or \"zig\""), "{error}");

        let error = Toolchain::for_target("auto", Target::named("android-aarch64")).err().unwrap();
        assert!(error.contains("NDK"), "{error}");
    }
}
