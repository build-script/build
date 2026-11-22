// The compilers of C and C++ modules: those of the system, zig, which brings clang, libc and libc++ for
// every target in one download, or the GCC build-script keeps, for C++26 reflection
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use crate::common::system::{self, HostArchitecture, HostOperatingSystem};
use crate::common::target::{Architecture, OperatingSystem, Target};
use crate::language::runtime::AndroidSettings;

pub const ZIG_VERSION: &str = "0.17.0";

/// What --version of each compiler said, asked once per build
static VERSIONS: Mutex<BTreeMap<Vec<String>, String>> = Mutex::new(BTreeMap::new());

/// Compiler, archiver and what they need in their environment
pub struct Toolchain {
    /// vcpkg builds the ports with the same compilers
    pub kind: Kind,
    /// what it builds for: the machine itself, or with zig and the NDK another system
    pub target: Target,
    pub c_compiler: Vec<String>,
    pub cpp_compiler: Vec<String>,
    pub archiver: Vec<String>,
    /// zig keeps the libc++ it builds once in its cache, the GCC of Windows finds its DLLs through the PATH
    pub environment: Vec<(&'static str, PathBuf)>,
}

/// Whose compilers a toolchain has
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// cc and c++ of the system, or those CC and CXX name
    System,
    /// zig cc and zig c++, for this machine and other desktop systems
    Zig,
    /// the GCC build-script keeps, for this machine
    Gcc,
    /// the clang of the NDK, for an Android target
    Ndk(Ndk),
}

/// The NDK a toolchain for Android is of
#[derive(Debug, Clone, PartialEq)]
pub struct Ndk {
    pub home: PathBuf,
    /// the lowest min_sdk of the apps packing the library
    pub api_level: i64,
}

/// The C++ standard library and the C library what a toolchain compiled links with: the C++ libraries of one
/// link need the same, on Windows the C libraries too
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Abi {
    /// libstdc++ of GCC, also that of clang on Linux
    LibStdCxx,
    /// libc++ of LLVM: the clang of macOS, zig and the NDK
    LibCxx,
    /// the C library and the STL of Visual Studio, which the clang of Windows takes
    Msvc,
    /// MinGW with libc++, like zig for Windows
    MinGwLibCxx,
    /// MinGW with libstdc++, like GCC for Windows
    MinGwLibStdCxx,
}

impl Abi {
    pub fn describe(self) -> &'static str {
        match self {
            Abi::LibStdCxx => return "libstdc++",
            Abi::LibCxx => return "libc++",
            Abi::Msvc => return "the C library and STL of Visual Studio",
            Abi::MinGwLibCxx => return "MinGW and libc++",
            Abi::MinGwLibStdCxx => return "MinGW and libstdc++",
        }
    }

    /// Whether C libraries of the one link with the other: on Windows MinGW and Visual Studio have C libraries
    /// of their own
    pub fn links_c_of(self, other: Abi) -> bool {
        let mingw = |abi: Abi| matches!(abi, Abi::MinGwLibCxx | Abi::MinGwLibStdCxx);
        return (self == Abi::Msvc) == (other == Abi::Msvc) && mingw(self) == mingw(other);
    }
}

impl Toolchain {
    /// "system": cc, c++ and ar of the system, on Windows clang, clang++ and llvm-ar, or those CC, CXX
    /// and AR name, like make takes them. "zig": zig cc, zig c++ and zig ar. "gcc": gcc, g++ and gcc-ar.
    pub fn named(name: &str) -> Result<Self, String> {
        let from_environment = |variable: &str, default: &str| vec![std::env::var(variable).unwrap_or(default.to_string())];

        if name == "system" {
            let [c_compiler, cpp_compiler, archiver] = system_compilers()?;

            return Ok(Toolchain {
                kind: Kind::System,
                target: Target::host(),
                c_compiler: from_environment("CC", &c_compiler),
                cpp_compiler: from_environment("CXX", &cpp_compiler),
                archiver: from_environment("AR", &archiver),
                environment: vec![],
            });
        }

        if name == "gcc" {
            let gcc = super::gcc::install()?;
            let mut environment = vec![];

            if let Some(directory) = gcc.path {
                let path = std::env::var_os("PATH").unwrap_or_default();
                let joined = std::env::join_paths([directory].into_iter().chain(std::env::split_paths(&path)))
                    .map_err(|error| format!("PATH: {error}"))?;
                environment.push(("PATH", PathBuf::from(joined)));
            }

            return Ok(Toolchain {
                kind: Kind::Gcc,
                target: Target::host(),
                c_compiler: vec![gcc.c_compiler],
                cpp_compiler: vec![gcc.cpp_compiler],
                archiver: vec![gcc.archiver],
                environment,
            });
        }

        if name != "zig" {
            return Err(format!("toolchain '{name}' is none of \"auto\", \"system\", \"zig\" and \"gcc\""));
        }

        let zig = install_zig()?.display().to_string();

        return Ok(Toolchain {
            kind: Kind::Zig,
            target: Target::host(),
            c_compiler: vec![zig.clone(), "cc".to_string()],
            cpp_compiler: vec![zig.clone(), "c++".to_string()],
            archiver: vec![zig, "ar".to_string()],
            environment: vec![("ZIG_GLOBAL_CACHE_DIR", system::cache_dir("zig"))],
        });
    }

    /// Windows with the C library and the linker of MinGW, like zig builds for it: libraries are lib<name>.a,
    /// a .def file exports what a DLL has
    pub fn mingw(&self) -> bool {
        return self.target.os == OperatingSystem::Windows && matches!(self.kind, Kind::Zig | Kind::Gcc);
    }

    /// Windows with the C library and the linker of Visual Studio: the clang of the system there
    pub fn msvc(&self) -> bool {
        return self.target.os == OperatingSystem::Windows && self.kind == Kind::System;
    }

    /// What its C++ links with. Those of the system are what CC names: clang takes libc++ on macOS and
    /// libstdc++ on Linux, a GCC there libstdc++ everywhere.
    pub fn abi(&self) -> Abi {
        let gcc = self.kind == Kind::System && is_gcc(&self.version(&self.c_compiler));
        return abi_of(&self.kind, self.target.os, gcc);
    }

    /// The compilers of a module for the target the script is evaluated for. "auto": those of the system for
    /// this machine, zig for another desktop system. "system" builds for this machine alone, "zig" for every
    /// desktop system.
    pub fn for_target(name: &str, target: Option<Target>, android: Option<&AndroidSettings>) -> Result<Self, String> {
        let Some(target) = target else {
            return Toolchain::named(if name == "auto" { "system" } else { name });
        };

        // the NDK builds for Android, with the lowest min_sdk of the apps that pack the library
        if target.os == OperatingSystem::Android {
            if name == "zig" || name == "gcc" {
                return Err(format!("{name} builds for no Android, \"auto\" takes the NDK for {}", target.name()));
            }
            return super::android::toolchain(target, android.unwrap_or(&super::android::default_settings()));
        }

        if name == "system" || name == "gcc" {
            return Err(format!("toolchain = \"{name}\" builds for this machine, \"auto\" or \"zig\" for {}", target.name()));
        }

        let triple = target.zig_triple().ok_or_else(|| format!("zig builds for no {}", target.name()))?;
        let mut zig = Toolchain::named("zig")?;
        zig.target = target;
        zig.c_compiler.extend(["-target".to_string(), triple.clone()]);
        zig.cpp_compiler.extend(["-target".to_string(), triple]);

        return Ok(zig);
    }

    /// The C compiler for commands that compile and link against the libraries it built, like cgo of Go. A
    /// C compiler links no C++ runtime of its own: zig and the clang of macOS take libc++, gcc libstdc++, the NDK
    /// its static libc++. Objects of MSVC name their runtime themselves. zig checks what it builds without
    /// `release` with UBSan, whose runtime it links only for -fsanitize=undefined when the link is not its own
    /// debug build.
    pub fn c_compiler_for_tools(&self, release: bool) -> super::CCompiler {
        let cpp_runtime: &[&str] = match (&self.kind, self.abi()) {
            (Kind::Ndk(_), _) => &["-lc++_static", "-lc++abi"],
            (_, Abi::LibCxx | Abi::MinGwLibCxx) => &["-lc++"],
            (_, Abi::LibStdCxx | Abi::MinGwLibStdCxx) => &["-lstdc++"],
            (_, Abi::Msvc) => &[],
        };

        return super::CCompiler {
            command: self.c_compiler.clone(),
            environment: self.environment
                .iter()
                .map(|(variable, value)| (variable.to_string(), value.clone()))
                .collect(),
            cpp_runtime: cpp_runtime.iter().map(|flag| flag.to_string()).collect(),
            link_flags: match self.kind == Kind::Zig && !release {
                true => vec!["-fsanitize=undefined".to_string()],
                false => vec![],
            },
        };
    }

    /// A toolchain file of CMake for zig and GCC, for vcpkg and build_cmake. CMake takes a compiler as one
    /// program, so scripts start zig cc, zig c++, zig ar and zig ranlib, for another target with -target: in the
    /// cache, one directory for this machine and one per target. GCC has programs of its own, its file lies
    /// with them.
    pub fn cmake_toolchain_file(&self) -> Result<PathBuf, String> {
        if self.kind == Kind::Gcc {
            let program = |words: &[String]| words[0].replace('\\', "/");
            let archiver = program(&self.archiver);
            // the ports go into shared libraries too, like those for the JVM
            let mut content = format!(
                "set(CMAKE_POSITION_INDEPENDENT_CODE ON)\nset(CMAKE_C_COMPILER \"{}\")\nset(CMAKE_CXX_COMPILER \"{}\")\n\
                 set(CMAKE_AR \"{archiver}\" CACHE FILEPATH \"\" FORCE)\nset(CMAKE_RANLIB \"{}\" CACHE FILEPATH \"\" FORCE)\n",
                program(&self.c_compiler),
                program(&self.cpp_compiler),
                archiver.replace("gcc-ar", "gcc-ranlib"),
            );

            // vcpkg names the system MinGW, which CMake does not know, windres compiles the resources of programs
            if self.target.os == OperatingSystem::Windows {
                content.push_str(&format!(
                    "set(CMAKE_SYSTEM_NAME Windows CACHE STRING \"\" FORCE)\nset(CMAKE_SYSTEM_PROCESSOR {} CACHE STRING \"\")\n\
                     set(CMAKE_RC_COMPILER \"{}\" CACHE FILEPATH \"\")\n",
                    self.target.arch.name(),
                    archiver.replace("gcc-ar", "windres"),
                ));
            }

            let prefix = Path::new(&self.c_compiler[0])
                .parent()
                .and_then(Path::parent)
                .ok_or_else(|| format!("{} lies in no installation of GCC", self.c_compiler[0]))?;
            let file = prefix.join("toolchain.cmake");
            system::write_file_if_changed(&file, &content)?;
            return Ok(file);
        }

        if self.kind != Kind::Zig || cfg!(windows) {
            return Err("CMake and vcpkg take zig only on macOS and Linux, its scripts are of sh".to_string());
        }

        let foreign = !self.target.is_host();
        let dir = system::cache_dir("zig-cmake").join(if foreign { self.target.name() } else { "host".to_string() });
        let ranlib = vec![self.archiver[0].clone(), "ranlib".to_string()];

        for (name, program) in [("cc", &self.c_compiler), ("c++", &self.cpp_compiler), ("ar", &self.archiver), ("ranlib", &ranlib)] {
            let words: Vec<String> = program.iter()
                .map(|word| format!("'{}'", word.replace('\'', r"'\''")))
                .collect();
            write_executable(&dir.join(name), &format!("#!/bin/sh\nexec {} \"$@\"\n", words.join(" ")))?;
        }

        let tool = |name: &str| dir.join(name).display().to_string();
        let mut content = format!(
            "set(CMAKE_C_COMPILER \"{}\")\nset(CMAKE_CXX_COMPILER \"{}\")\nset(CMAKE_AR \"{}\")\nset(CMAKE_RANLIB \"{}\")\n",
            tool("cc"),
            tool("c++"),
            tool("ar"),
            tool("ranlib"),
        );

        // resources of programs for Windows, like their version: CMake calls windres of MinGW, zig rc compiles them
        if foreign && self.target.os == OperatingSystem::Windows {
            write_executable(&dir.join("windres"), &windres(&self.archiver[0], self.target))?;
            content.push_str(&format!("set(CMAKE_RC_COMPILER \"{}\")\n", tool("windres")));
        }

        // another system than the machine: CMake builds for it and runs nothing it built
        if foreign {
            let system = match self.target.os {
                OperatingSystem::Linux => "Linux",
                OperatingSystem::MacOs => "Darwin",
                OperatingSystem::Windows => "Windows",
                OperatingSystem::Android => "Android",
            };
            content.push_str(&format!(
                "set(CMAKE_SYSTEM_NAME {system})\nset(CMAKE_SYSTEM_PROCESSOR {})\nset(CMAKE_CROSSCOMPILING TRUE)\n",
                self.target.arch.name()
            ));
        }

        let file = dir.join("toolchain.cmake");
        system::write_file_if_changed(&file, &content)?;

        return Ok(file);
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

/// Of what a compiler says to --version: GCC names no clang, nothing when it does not start
fn is_gcc(version: &str) -> bool {
    return !version.is_empty() && !version.contains("clang");
}

fn abi_of(kind: &Kind, os: OperatingSystem, gcc: bool) -> Abi {
    match (kind, os) {
        (Kind::Zig, OperatingSystem::Windows) => return Abi::MinGwLibCxx,
        (Kind::Zig | Kind::Ndk(_), _) => return Abi::LibCxx,
        (Kind::Gcc, OperatingSystem::Windows) => return Abi::MinGwLibStdCxx,
        (Kind::Gcc, _) => return Abi::LibStdCxx,
        (Kind::System, OperatingSystem::Windows) if gcc => return Abi::MinGwLibStdCxx,
        (Kind::System, OperatingSystem::Windows) => return Abi::Msvc,
        (Kind::System, _) if gcc => return Abi::LibStdCxx,
        (Kind::System, OperatingSystem::Linux) => return Abi::LibStdCxx,
        (Kind::System, _) => return Abi::LibCxx,
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

/// windres of MinGW as CMake calls it, `-O coff -D… -I… <source> <object>`, for zig rc, which writes the same
/// object of COFF and brings the headers of MinGW
fn windres(zig: &str, target: Target) -> String {
    let machine = match target.arch {
        Architecture::Aarch64 => "aarch64",
        Architecture::X86 => "x86",
        _ => "x86_64",
    };

    return format!(
        r#"#!/bin/sh
# windres of MinGW for CMake, its options turned into those of zig rc
quote() {{ printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }}
options=""
source=""
output=""
while [ $# -gt 0 ]; do
    case "$1" in
        -O|-F|--output-format|--input-format|--target|-c|--codepage) shift ;;
        -D) shift; options="$options /d $(quote "$1")" ;;
        -D*) options="$options /d $(quote "${{1#-D}}")" ;;
        --define=*) options="$options /d $(quote "${{1#--define=}}")" ;;
        -U) shift; options="$options /u $(quote "$1")" ;;
        -U*) options="$options /u $(quote "${{1#-U}}")" ;;
        -I) shift; options="$options /i $(quote "$1")" ;;
        -I*) options="$options /i $(quote "${{1#-I}}")" ;;
        --include-dir=*) options="$options /i $(quote "${{1#--include-dir=}}")" ;;
        -i) shift; source="$1" ;;
        --input=*) source="${{1#--input=}}" ;;
        -o) shift; output="$1" ;;
        --output=*) output="${{1#--output=}}" ;;
        -*) ;;
        *) if [ -z "$source" ]; then source="$1"; else output="$1"; fi ;;
    esac
    shift
done
eval "exec $(quote '{zig}') rc /:output-format coff /:target {machine} /:auto-includes gnu $options -- $(quote "$source") $(quote "$output")"
"#
    );
}

/// A script of sh, executable. Written only when it changed: builds of modules running side by side start it.
pub fn write_executable(file: &Path, script: &str) -> Result<(), String> {
    system::write_file_if_changed(file, script)?;

    // Windows has no executable bit
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let executable = std::fs::metadata(file).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0);
        if !executable {
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755))
                .map_err(|error| format!("could not make {} executable: {error}", file.display()))?;
        }
    }

    return Ok(());
}

#[cfg(test)]
mod tests {
    use super::Toolchain;
    use crate::common::target::Target;

    #[test]
    fn the_toolchain_follows_the_target() {
        // "auto" on this machine: the compilers of the system, like "system"
        let auto = Toolchain::for_target("auto", None, None).unwrap();
        assert_eq!(auto.kind, super::Kind::System);
        assert_eq!(auto.target, Target::host());

        // those of the system build for this machine alone, the NDK builds for Android
        let other = Target::named("windows-x86_64").filter(|target| !target.is_host()).unwrap_or(Target::named("linux-aarch64").unwrap());
        let error = Toolchain::for_target("system", Some(other), None).err().unwrap();
        assert!(error.contains("\"auto\" or \"zig\""), "{error}");

        let error = Toolchain::for_target("zig", Target::named("android-aarch64"), None).err().unwrap();
        assert!(error.contains("NDK"), "{error}");

        // the GCC build-script keeps builds for this machine
        let error = Toolchain::for_target("gcc", Some(other), None).err().unwrap();
        assert!(error.contains("toolchain = \"gcc\" builds for this machine"), "{error}");
    }

    #[test]
    fn the_standard_libraries_follow_the_compilers() {
        use super::{Abi, Kind, abi_of, is_gcc};
        use crate::common::target::OperatingSystem::{Linux, MacOs, Windows};

        assert!(is_gcc("gcc (Homebrew GCC 16.2.0) 16.2.0"));
        assert!(!is_gcc("Apple clang version 16.0.0 (clang-1600.0.26.6)"));
        // a compiler that does not start says nothing: the default of the system
        assert!(!is_gcc(""));

        assert_eq!(abi_of(&Kind::System, MacOs, false), Abi::LibCxx);
        assert_eq!(abi_of(&Kind::System, MacOs, true), Abi::LibStdCxx);
        assert_eq!(abi_of(&Kind::System, Linux, false), Abi::LibStdCxx);
        assert_eq!(abi_of(&Kind::System, Windows, false), Abi::Msvc);
        assert_eq!(abi_of(&Kind::System, Windows, true), Abi::MinGwLibStdCxx);
        assert_eq!(abi_of(&Kind::Zig, Linux, false), Abi::LibCxx);
        assert_eq!(abi_of(&Kind::Zig, Windows, false), Abi::MinGwLibCxx);
        assert_eq!(abi_of(&Kind::Gcc, MacOs, false), Abi::LibStdCxx);
        assert_eq!(abi_of(&Kind::Gcc, Windows, false), Abi::MinGwLibStdCxx);

        // C libraries differ on Windows alone
        assert!(Abi::LibCxx.links_c_of(Abi::LibStdCxx));
        assert!(Abi::MinGwLibCxx.links_c_of(Abi::MinGwLibStdCxx));
        assert!(!Abi::Msvc.links_c_of(Abi::MinGwLibStdCxx));
    }

    #[test]
    fn cmake_takes_the_programs_of_gcc_as_they_are() {
        use crate::common::system;
        use crate::common::test_support::ScratchDir;

        let directory = ScratchDir::new("gcc-cmake");
        let tool = |name: &str| vec![directory.join("bin").join(name).display().to_string()];
        let gcc = Toolchain {
            kind: super::Kind::Gcc,
            target: Target::host(),
            c_compiler: tool("gcc-16"),
            cpp_compiler: tool("g++-16"),
            archiver: tool("gcc-ar-16"),
            environment: vec![],
        };

        let file = gcc.cmake_toolchain_file().unwrap();
        assert_eq!(file, directory.join("toolchain.cmake"));

        let content = system::read_file(&file).unwrap();
        let bin = directory.join("bin").display().to_string().replace('\\', "/");
        assert!(content.contains(&format!("set(CMAKE_CXX_COMPILER \"{bin}/g++-16\")")), "{content}");
        assert!(content.contains(&format!("set(CMAKE_RANLIB \"{bin}/gcc-ranlib-16\" CACHE FILEPATH \"\" FORCE)")), "{content}");
    }

    #[cfg(unix)]
    #[test]
    fn windres_of_cmake_becomes_zig_rc() {
        use crate::common::test_support::ScratchDir;

        // a zig that says what it was given
        let directory = ScratchDir::new("windres");
        let zig = directory.join("zig");
        super::write_executable(&zig, "#!/bin/sh\nfor argument in \"$@\"; do printf '[%s]' \"$argument\"; done\n").unwrap();
        let windres = directory.join("windres");
        super::write_executable(&windres, &super::windres(&zig.display().to_string(), Target::named("windows-aarch64").unwrap())).unwrap();

        let output = std::process::Command::new(&windres)
            .args(["-O", "coff", "-DVERSION=1.3.1", "-D", "NAME=zlib", "-I", "inc dir", "-Iother", "zlib1.rc", "zlib1.rc.obj"])
            .output()
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "[rc][/:output-format][coff][/:target][aarch64][/:auto-includes][gnu][/d][VERSION=1.3.1][/d][NAME=zlib][/i][inc dir][/i][other][--][zlib1.rc][zlib1.rc.obj]"
        );
    }
}
