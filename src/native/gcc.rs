// The GCC of toolchain = "gcc", which builds every C and C++ module the same on every machine, also code only GCC
// compiles so far. On Linux and Windows the packages of conda-forge, which micromamba installs from a list of them
// with their SHA-256. On macOS it is built once from its sources, with the Darwin branch Homebrew takes, against
// the SDK of the Command Line Tools: conda-forge builds it there without its libstdc++, on libc++, which lacks
// parts of the newest standard, like <meta>. Its libstdc++ is configured for that SDK, another SDK builds it again.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::common::system::{self, HostArchitecture, HostOperatingSystem};

/// The GCC of toolchain = "gcc": a newer build-script may bring a newer one
pub const GCC_VERSION: &str = "16.2.0";

/// gcc-16, g++-16 and gcc-ar-16 are named for it
const MAJOR: &str = "16";

const SOURCES: &str = "https://ftpmirror.gnu.org/gcc/gcc-16.2.0/gcc-16.2.0.tar.xz";
const SOURCES_SHA256: &str = "e6738e29597f733270731aa90600f37ffdc045079dfc27ec7e8192cc81085c3e";

/// The Darwin branch of Iain Sandoe as one patch, as Homebrew builds GCC: Apple Silicon comes upstream with
/// GCC 17
const DARWIN_PATCH: &str =
    "https://raw.githubusercontent.com/Homebrew/homebrew-core/4a334d2f90f786c3fc86cbc9cf72b2f13961b078/Patches/gcc/gcc-16.2.0.diff";
const DARWIN_PATCH_SHA256: &str = "578a78ae0bc62a02f260b6a20c7f23e71deee16ed644e9cb5619247be4df5a71";

/// The packages of conda-forge with GCC for each machine, as `micromamba create --dry-run --json --platform <subdir>
/// -c conda-forge --override-channels <specs>` lists them: a new GCC_VERSION lists them again
const PACKAGES: &str = include_str!("gcc-packages.json");

/// micromamba of github.com/mamba-org/micromamba-releases, for the machines conda-forge has GCC for: the asset and
/// the SHA-256 the release gives
const MICROMAMBA_VERSION: &str = "2.10.0-0";
const MICROMAMBA: &[(&str, &str, &str)] = &[
    ("linux-64", "micromamba-linux-64", "701f39c06f8e6a670a0e186c8479b9e5c0502a2f0cda6cef65d83bdd2875e8bd"),
    ("linux-aarch64", "micromamba-linux-aarch64", "3bb56786e21b9b24024f599fe9185b4a59dc3f9f59937a42311ee583adbbb23c"),
    ("win-64", "micromamba-win-64", "f8b4284bd4eb1134122eaf93b3ea6c7be567e596de5829e1a6f478e34e41974a"),
];

/// The programs of a GCC and what they need to start
pub struct Gcc {
    pub c_compiler: String,
    pub cpp_compiler: String,
    pub archiver: String,
    /// before the PATH: on Windows the assembler and the linker of conda-forge find zstd.dll there
    pub path: Option<PathBuf>,
}

/// Installed on the first build that needs it
pub fn install() -> Result<Gcc, String> {
    if system::host_operating_system()? != HostOperatingSystem::MacOs {
        return from_conda_forge();
    }

    let bin = build_for_macos()?;
    let tool = |name: &str| bin.join(format!("{name}-{MAJOR}")).display().to_string();

    return Ok(Gcc {
        c_compiler: tool("gcc"),
        cpp_compiler: tool("g++"),
        archiver: tool("gcc-ar"),
        path: None,
    });
}

/// How conda-forge names this machine
fn subdir() -> Result<&'static str, String> {
    match (system::host_operating_system()?, system::host_architecture()?) {
        (HostOperatingSystem::Linux, HostArchitecture::X86_64) => return Ok("linux-64"),
        (HostOperatingSystem::Linux, HostArchitecture::Aarch64) => return Ok("linux-aarch64"),
        (HostOperatingSystem::Windows, HostArchitecture::X86_64) => return Ok("win-64"),
        (system, architecture) => return Err(format!("conda-forge has no GCC for {system:?} on {architecture:?}")),
    }
}

/// The programs in an installation of conda-forge: named for their target, on Windows in Library/bin
fn conda_programs(prefix: &Path, subdir: &str) -> Gcc {
    let (bin, extension) = match subdir {
        "win-64" => (prefix.join("Library").join("bin"), ".exe"),
        _ => (prefix.join("bin"), ""),
    };
    let tool = |name: &str| bin.join(format!("{}-{name}{extension}", conda_triple(subdir))).display().to_string();

    return Gcc {
        c_compiler: tool("gcc"),
        cpp_compiler: tool("g++"),
        archiver: tool("gcc-ar"),
        path: (subdir == "win-64").then_some(bin.clone()),
    };
}

/// The explicit file micromamba installs without resolving anything, every package with its SHA-256
fn explicit_file(subdir: &str) -> Result<String, String> {
    let packages: serde_json::Value = serde_json::from_str(PACKAGES).map_err(|error| format!("the packages of GCC: {error}"))?;
    let lines: Vec<&str> = packages[subdir]["packages"]
        .as_array()
        .ok_or_else(|| format!("build-script lists no packages of GCC for {subdir}"))?
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();

    return Ok(format!("@EXPLICIT\n{}\n", lines.join("\n")));
}

/// Into <build home>/gcc/<version>/<subdir>, where it stays: conda writes the path of an installation into its
/// files. Done once its marker is there.
fn from_conda_forge() -> Result<Gcc, String> {
    let subdir = subdir()?;
    let target = system::data_dir("gcc").join(GCC_VERSION).join(subdir);
    let marker = target.join(".build-script-gcc");
    let gcc = conda_programs(&target, subdir);

    return system::installing(&target, || {
        if marker.is_file() {
            return Ok(gcc);
        }

        let micromamba = install_micromamba(subdir)?;
        let work = PathBuf::from(format!("{}.install", target.display()));
        system::recreate_dir(&work)?;
        system::remove_dir(&target)?;

        let explicit = work.join("explicit.txt");
        system::write_file(&explicit, &explicit_file(subdir)?)?;
        system::log_tool("gcc", &format!("Installing GCC {GCC_VERSION} of conda-forge"));

        // its downloads in the work directory, which goes: the installation keeps what it needs of them
        let mut create = Command::new(&micromamba);
        create.args(["create", "--no-rc", "--yes", "--prefix"])
            .arg(&target)
            .arg("--file")
            .arg(&explicit)
            .env("MAMBA_ROOT_PREFIX", work.join("root"));
        run(&mut create, &work.join("micromamba.log"), "installing GCC with micromamba")?;

        // conda-forge's specs give every program an rpath into this installation: what GCC builds has its runtime
        // inside and runs without it
        let specs = target.join("lib/gcc").join(conda_triple(subdir)).join(GCC_VERSION).join("specs");

        if subdir.starts_with("linux") && specs.is_file() {
            fs::remove_file(&specs).map_err(|error| format!("could not remove {}: {error}", specs.display()))?;
        }

        system::write_file(&marker, &format!("{GCC_VERSION}\n"))?;
        system::remove_dir(&work)?;
        return Ok(gcc);
    });
}

/// What the programs of conda-forge are named for
fn conda_triple(subdir: &str) -> &'static str {
    match subdir {
        "linux-aarch64" => return "aarch64-conda-linux-gnu",
        "win-64" => return "x86_64-w64-mingw32",
        _ => return "x86_64-conda-linux-gnu",
    }
}

/// micromamba into <build home>/micromamba/<version>, checked against the SHA-256 of its release
fn install_micromamba(subdir: &str) -> Result<PathBuf, String> {
    let Some((_, asset, sha256)) = MICROMAMBA.iter().find(|(name, ..)| *name == subdir) else {
        return Err(format!("build-script knows no micromamba for {subdir}"));
    };

    let directory = system::data_dir("micromamba").join(MICROMAMBA_VERSION);
    let program = directory.join(format!("micromamba{}", std::env::consts::EXE_SUFFIX));

    return system::installing(&directory, || {
        if program.is_file() {
            return Ok(program.clone());
        }

        let url = format!("https://github.com/mamba-org/micromamba-releases/releases/download/{MICROMAMBA_VERSION}/{asset}");
        let checked = |file: &Path| match system::sha(file, 256)? == *sha256 {
            true => Ok(()),
            false => Err(format!("{url} is not the micromamba build-script knows")),
        };

        if !system::download_checked(&url, &program, false, None, checked)? {
            return Err(format!("{url} is not available"));
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&program, fs::Permissions::from_mode(0o755))
                .map_err(|error| format!("could not make {} executable: {error}", program.display()))?;
        }

        return Ok(program.clone());
    });
}

/// The SDK of the Command Line Tools or of Xcode, by its own name, and its version: MacOSX.sdk names the newest
fn sdk() -> Result<(PathBuf, String), String> {
    let missing = "GCC builds against the SDK of macOS: install the Command Line Tools with xcode-select --install";
    let path = system::command_output(Command::new("xcrun").arg("--show-sdk-path"), "xcrun").map_err(|_| missing.to_string())?;
    let version = system::command_output(Command::new("xcrun").arg("--show-sdk-version"), "xcrun").map_err(|_| missing.to_string())?;
    let path = fs::canonicalize(path.trim()).map_err(|_| missing.to_string())?;

    return Ok((path, version.trim().to_string()));
}

/// Into <build home>/gcc/<version>/macos-<architecture>-sdk-<version>, with the clang of the Command Line Tools:
/// a GCC of Homebrew in the PATH may not build for this SDK
fn build_for_macos() -> Result<PathBuf, String> {
    let (sdk, sdk_version) = sdk()?;
    let architecture = match system::host_architecture()? {
        HostArchitecture::Aarch64 => "aarch64",
        HostArchitecture::X86_64 => "x86_64",
    };
    let target = system::data_dir("gcc")
        .join(GCC_VERSION)
        .join(format!("macos-{architecture}-sdk-{sdk_version}"));
    let bin = target.join("bin");

    return system::installing(&target, || {
        if bin.join(format!("g++-{MAJOR}")).is_file() {
            return Ok(bin.clone());
        }

        // kept when a step fails, its logs say why
        let work = PathBuf::from(format!("{}.build", target.display()));
        system::recreate_dir(&work)?;
        system::log_tool("gcc", &format!("Building GCC {GCC_VERSION} for the macOS SDK {sdk_version}, once, in about ten minutes"));

        let sources = work.join("gcc");
        system::install_archive(SOURCES, &work.join("gcc.tar.xz"), Some((256, SOURCES_SHA256)), &sources)?;

        let patch = work.join("darwin.diff");
        let checked = |file: &Path| match system::sha(file, 256)? == DARWIN_PATCH_SHA256 {
            true => Ok(()),
            false => Err(format!("{DARWIN_PATCH} is not the patch build-script knows")),
        };
        if !system::download_checked(DARWIN_PATCH, &patch, false, None, checked)? {
            return Err(format!("{DARWIN_PATCH} is not available"));
        }

        let logs = |step: &str| work.join(format!("{step}.log"));
        run(Command::new("patch").args(["-p1", "-s", "-i"]).arg(&patch).current_dir(&sources), &logs("patch"), "patching GCC")?;
        // gmp, mpfr, mpc and isl, checked against the SHA-512 GCC lists
        run(Command::new("./contrib/download_prerequisites").current_dir(&sources), &logs("prerequisites"), "downloading the libraries of GCC")?;

        let kernel = system::command_output(Command::new("uname").arg("-r"), "uname")?;
        let kernel = kernel.trim().split('.').next().unwrap_or_default().to_string();
        let compiler = |name: &str| system::command_output(Command::new("xcrun").args(["--find", name]), "xcrun").map(|path| path.trim().to_string());
        let build = work.join("build");
        system::create_dir(&build)?;

        // the clang of the Command Line Tools finds the SDK through SDKROOT, which /usr/bin/clang sets for it
        let mut configure = Command::new(sources.join("configure"));
        configure.args(configure_arguments(&target, architecture, &kernel, &sdk))
            .current_dir(&build)
            .env("CC", compiler("clang")?)
            .env("CXX", compiler("clang++")?)
            .env("SDKROOT", &sdk);
        run(&mut configure, &logs("configure"), "configuring GCC")?;

        let jobs = std::thread::available_parallelism().map_or(4, |count| count.get());
        let mut make = Command::new("make");
        make.arg(format!("-j{jobs}"))
            // install_name_tool rewrites the libraries of the runtime when they are installed
            .arg("LDFLAGS_FOR_TARGET=-Wl,-headerpad_max_install_names")
            .current_dir(&build)
            .env("SDKROOT", &sdk);
        run(&mut make, &logs("make"), "building GCC")?;
        run(Command::new("make").arg("install").current_dir(&build).env("SDKROOT", &sdk), &logs("install"), "installing GCC")?;

        system::remove_dir(&work)?;
        system::log_tool("gcc", &format!("Done: GCC {GCC_VERSION} in {}", target.display()));
        return Ok(bin.clone());
    });
}

/// C and C++ only, without the bootstrap that builds GCC three times: clang builds it once
fn configure_arguments(prefix: &Path, architecture: &str, kernel: &str, sdk: &Path) -> Vec<String> {
    return vec![
        format!("--prefix={}", prefix.display()),
        format!("--build={architecture}-apple-darwin{kernel}"),
        format!("--with-sysroot={}", sdk.display()),
        "--enable-languages=c,c++".to_string(),
        "--disable-bootstrap".to_string(),
        "--disable-nls".to_string(),
        "--enable-checking=release".to_string(),
        "--with-gcc-major-version-only".to_string(),
        format!("--program-suffix=-{MAJOR}"),
        "--with-system-zlib".to_string(),
        format!("--with-pkgversion=build-script GCC {GCC_VERSION}"),
    ];
}

/// What a step prints goes to its log, GCC prints a lot: the end of it shows when the step fails
fn run(command: &mut Command, log: &Path, what: &str) -> Result<(), String> {
    let file = fs::File::create(log).map_err(|error| format!("could not write {}: {error}", log.display()))?;
    let error_file = file.try_clone().map_err(|error| format!("could not write {}: {error}", log.display()))?;
    let status = command.stdin(Stdio::null())
        .stdout(file)
        .stderr(error_file)
        .status()
        .map_err(|error| format!("{what}: could not start {:?}: {error}", command.get_program()))?;

    if status.success() {
        return Ok(());
    }

    let text = fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let end = lines[lines.len().saturating_sub(20)..].join("\n");
    return Err(format!("{what} failed ({status}), the end of {}:\n{end}", log.display()));
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{MICROMAMBA, configure_arguments, conda_programs, explicit_file};

    #[test]
    fn micromamba_installs_the_packages_of_each_machine_by_their_sha256() {
        for (subdir, ..) in MICROMAMBA {
            let explicit = explicit_file(subdir).unwrap();
            let mut lines = explicit.lines();
            assert_eq!(lines.next(), Some("@EXPLICIT"));

            let packages: Vec<&str> = lines.collect();
            assert!(packages.iter().any(|line| line.contains(&format!("/gxx_impl_{subdir}-16.2.0-"))), "{subdir}: {explicit}");
            for line in packages {
                let (url, sha256) = line.split_once("#sha256:").unwrap();
                assert!(url.starts_with("https://conda.anaconda.org/conda-forge/"), "{line}");
                assert_eq!(sha256.len(), 64, "{line}");
            }
        }

        assert!(explicit_file("osx-arm64").is_err());
    }

    #[test]
    fn the_programs_of_conda_forge_are_named_for_their_target() {
        let prefix = Path::new("/gcc");
        let linux = conda_programs(prefix, "linux-aarch64");
        assert_eq!(linux.cpp_compiler, prefix.join("bin").join("aarch64-conda-linux-gnu-g++").display().to_string());
        assert!(linux.path.is_none());

        // on Windows in Library/bin, where the assembler and the linker find zstd.dll
        let windows = conda_programs(prefix, "win-64");
        let bin = prefix.join("Library").join("bin");
        assert_eq!(windows.archiver, bin.join("x86_64-w64-mingw32-gcc-ar.exe").display().to_string());
        assert_eq!(windows.path, Some(bin));
    }

    #[test]
    fn gcc_builds_for_the_sdk_without_its_bootstrap() {
        let arguments = configure_arguments(
            Path::new("/home/gcc/16.2.0/macos-aarch64-sdk-15.2"),
            "aarch64",
            "25",
            Path::new("/Library/Developer/CommandLineTools/SDKs/MacOSX15.2.sdk"),
        );

        for expected in [
            "--prefix=/home/gcc/16.2.0/macos-aarch64-sdk-15.2",
            "--build=aarch64-apple-darwin25",
            "--with-sysroot=/Library/Developer/CommandLineTools/SDKs/MacOSX15.2.sdk",
            "--enable-languages=c,c++",
            "--disable-bootstrap",
            "--program-suffix=-16",
        ] {
            assert!(arguments.iter().any(|argument| argument == expected), "{expected} in {arguments:?}");
        }
    }
}
