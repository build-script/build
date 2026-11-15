// The compilers of C and C++ modules: those of the system, or zig, which brings clang, libc and
// libc++ for every target in one download
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use crate::common::system::{self, HostArchitecture, HostOperatingSystem};
use crate::common::target::{Architecture, OperatingSystem, Target};

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

    /// A toolchain file of CMake for zig, for vcpkg and build_cmake: CMake takes a compiler as one program, so
    /// scripts start zig cc, zig c++, zig ar and zig ranlib, for another target with -target. In the cache,
    /// one directory for this machine and one per target.
    pub fn cmake_toolchain_file(&self) -> Result<PathBuf, String> {
        if !self.zig || cfg!(windows) {
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
