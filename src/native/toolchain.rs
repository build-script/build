// The compilers of C and C++ modules: those of the system, or zig, which brings clang, libc and
// libc++ for every target in one download
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use crate::common::system::{self, HostArchitecture, HostOperatingSystem};

pub const ZIG_VERSION: &str = "0.17.0";

/// What --version of each compiler said, asked once per build
static VERSIONS: Mutex<BTreeMap<Vec<String>, String>> = Mutex::new(BTreeMap::new());

/// What CMake and Visual Studio link every program of Windows with
const WINDOWS_LIBRARIES: &[&str] = &["kernel32", "user32", "gdi32", "winspool", "shell32", "ole32", "oleaut32", "uuid", "comdlg32", "advapi32"];

/// What a toolchain builds for: the names of programs and libraries, and the flags that differ by system
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NativeTarget {
    MacOs,
    Linux,
    Windows,
    Android,
}

impl NativeTarget {
    /// The system build-script runs on, the compilers of the system and zig build for it
    pub fn host() -> Self {
        match std::env::consts::OS {
            "macos" => return NativeTarget::MacOs,
            "windows" => return NativeTarget::Windows,
            _ => return NativeTarget::Linux,
        }
    }

    pub fn executable(self, name: &str) -> String {
        match self {
            NativeTarget::Windows => return format!("{name}.exe"),
            _ => return name.to_string(),
        }
    }

    /// On Windows also the import library that links a shared one
    pub fn static_library(self, name: &str) -> String {
        match self {
            NativeTarget::Windows => return format!("{name}.lib"),
            _ => return format!("lib{name}.a"),
        }
    }

    pub fn shared_library(self, name: &str) -> String {
        match self {
            NativeTarget::MacOs => return format!("lib{name}.dylib"),
            NativeTarget::Windows => return format!("{name}.dll"),
            _ => return format!("lib{name}.so"),
        }
    }

    /// Where in build/ shared libraries lie: Windows loads them from the directory of the program
    pub fn shared_dir(self) -> &'static str {
        match self {
            NativeTarget::Windows => return "bin",
            _ => return "lib",
        }
    }

    /// How an rpath names the directory of the program or library, Windows has none
    pub fn rpath_origin(self) -> Option<&'static str> {
        match self {
            NativeTarget::MacOs => return Some("@loader_path"),
            NativeTarget::Windows => return None,
            _ => return Some("$ORIGIN"),
        }
    }

    /// Shared libraries of Linux link only position independent code, also that of the static ones in them
    pub fn needs_position_independent_code(self) -> bool {
        return matches!(self, NativeTarget::Linux | NativeTarget::Android);
    }

    /// C programs of glibc link the math library, the C++ standard library brings it along, on macOS
    /// libSystem has it
    pub fn links_math_library(self) -> bool {
        return self == NativeTarget::Linux;
    }

    /// What every program links besides
    pub fn system_libraries(self) -> &'static [&'static str] {
        match self {
            NativeTarget::Windows => return WINDOWS_LIBRARIES,
            _ => return &[],
        }
    }
}

/// Compiler, archiver and what they need in their environment
pub struct Toolchain {
    /// vcpkg builds the ports with zig too
    pub zig: bool,
    pub target: NativeTarget,
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
                target: NativeTarget::host(),
                c_compiler: from_environment("CC", &c_compiler),
                cpp_compiler: from_environment("CXX", &cpp_compiler),
                archiver: from_environment("AR", &archiver),
                environment: vec![],
            });
        }

        if name != "zig" {
            return Err(format!("toolchain '{name}' is neither \"system\" nor \"zig\""));
        }

        let zig = install_zig()?.display().to_string();

        return Ok(Toolchain {
            zig: true,
            target: NativeTarget::host(),
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
    use super::NativeTarget;

    #[test]
    fn every_target_names_its_programs_and_libraries() {
        let names = |target: NativeTarget| {
            return (target.executable("app"), target.static_library("core"), target.shared_library("core"), target.shared_dir());
        };

        assert_eq!(names(NativeTarget::MacOs), ("app".into(), "libcore.a".into(), "libcore.dylib".into(), "lib"));
        assert_eq!(names(NativeTarget::Linux), ("app".into(), "libcore.a".into(), "libcore.so".into(), "lib"));
        assert_eq!(names(NativeTarget::Android), ("app".into(), "libcore.a".into(), "libcore.so".into(), "lib"));
        assert_eq!(names(NativeTarget::Windows), ("app.exe".into(), "core.lib".into(), "core.dll".into(), "bin"));

        // Windows finds shared libraries next to the program, without an rpath
        assert_eq!(NativeTarget::MacOs.rpath_origin(), Some("@loader_path"));
        assert_eq!(NativeTarget::Windows.rpath_origin(), None);

        // flags of the target, not of the host that builds for it
        assert!(NativeTarget::Android.needs_position_independent_code());
        assert!(!NativeTarget::MacOs.needs_position_independent_code());
        assert!(NativeTarget::Linux.links_math_library());
        assert!(!NativeTarget::Android.links_math_library());
        assert_eq!(NativeTarget::Windows.system_libraries().first(), Some(&"kernel32"));
    }
}
