// Libraries of C and C++ modules from vcpkg: build-script installs vcpkg at a release, which pins every
// port to one version. Their flags come from their .pc files, of ports without one from their files.
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::common::system::{self, HostArchitecture, HostOperatingSystem};
use crate::common::target::{Architecture, OperatingSystem};

use super::toolchain::{Kind, Ndk, Toolchain, write_executable};

const VCPKG_RELEASE: &str = "2026.10.16";

/// vcpkg checks its .pc files with pkg-config and fixes the RPATHs of Linux programs with patchelf,
/// it does not bring them on macOS and Linux. Neither matters for static libraries. On Windows it
/// downloads pkg-config itself.
const PKG_CONFIG: &str = "#!/bin/sh\ncase \"$1\" in --version) echo 2.1.0;; esac\nexit 0\n";
const PATCHELF: &str = "#!/bin/sh\ncase \"$1\" in --version) echo patchelf 0.18.0;; esac\nexit 0\n";

/// The ports of a module, installed into build/vcpkg/<triplet>
pub struct Installed {
    triplet: String,
    root: PathBuf,
    /// built by MSVC on Windows: <name>.lib, release only, flags of MSVC in their .pc files
    msvc: bool,
    /// for this machine: libraries vcpkg does not have may come from pkg-config of the system
    of_the_machine: bool,
}

/// Installs the ports, only when they changed. None without ports.
pub fn install(
    module: &str,
    ports: &[String],
    toolchain: &Toolchain,
    build_dir: &Path,
) -> Result<Option<Installed>, String> {
    if ports.is_empty() {
        return Ok(None);
    }

    let host = match (system::host_operating_system()?, system::host_architecture()?) {
        (HostOperatingSystem::MacOs, HostArchitecture::Aarch64) => "arm64-osx",
        (HostOperatingSystem::MacOs, HostArchitecture::X86_64) => "x64-osx",
        (HostOperatingSystem::Linux, HostArchitecture::Aarch64) => "arm64-linux",
        (HostOperatingSystem::Linux, HostArchitecture::X86_64) => "x64-linux",
        // with the static C library, which clang links by default, and only for release: debug builds link
        // them too, the C library of debug builds would not match
        (HostOperatingSystem::Windows, HostArchitecture::Aarch64) => "arm64-windows-static-release",
        (HostOperatingSystem::Windows, HostArchitecture::X86_64) => "x64-windows-static-release",
    };

    // vcpkg builds the ports of Windows with MSVC, zig's are of MinGW
    if toolchain.kind == Kind::Zig && cfg!(windows) {
        return Err(format!("{module}: vcpkg ports with the zig toolchain are not set up for Windows, the system toolchain builds them"));
    }

    let triplet = triplet(toolchain, host);
    let manifest_dir = build_dir.join("vcpkg").join(&triplet);
    let installed = Installed {
        root: manifest_dir.join("installed"),
        triplet,
        msvc: toolchain.msvc(),
        of_the_machine: toolchain.target.is_host(),
    };

    let dependencies: Vec<serde_json::Value> = ports.iter()
        .map(|port| match port_and_features(port) {
            (name, features) if features.is_empty() => serde_json::json!(name),
            (name, features) => serde_json::json!({ "name": name, "features": features }),
        })
        .collect();
    let manifest = serde_json::json!({ "dependencies": dependencies }).to_string();
    let manifest_file = manifest_dir.join("vcpkg.json");
    // the manifest of the last install that succeeded: vcpkg.json is written before it, also for one that fails
    let installed_manifest = manifest_dir.join("installed.json");

    // vcpkg takes a second even when nothing changed
    let unchanged = fs::read_to_string(&installed_manifest).is_ok_and(|existing| existing == manifest);

    if unchanged && installed.root.is_dir() {
        return Ok(Some(installed));
    }

    let vcpkg = install_vcpkg()?;
    let tools = vcpkg.join("build-script");
    system::write_file(&manifest_file, &manifest)?;
    let _ = fs::remove_file(&installed_manifest);

    // vcpkg wants its caches to exist
    for cache in ["archives", "downloads", "registries"] {
        system::create_dir(&system::cache_dir("vcpkg").join(cache))?;
    }

    let mut command = Command::new(vcpkg.join("vcpkg"));
    command.arg("install")
        .arg(format!("--triplet={}", installed.triplet))
        .arg(format!("--host-triplet={host}"))
        .arg(format!("--x-manifest-root={}", manifest_dir.display()))
        .arg(format!("--x-install-root={}", installed.root.display()))
        .env("VCPKG_ROOT", &vcpkg)
        .env("VCPKG_DEFAULT_BINARY_CACHE", system::cache_dir("vcpkg/archives"))
        .env("VCPKG_DOWNLOADS", system::cache_dir("vcpkg/downloads"))
        .env("X_VCPKG_REGISTRIES_CACHE", system::cache_dir("vcpkg/registries"))
        .env("VCPKG_DISABLE_METRICS", "1");

    if !cfg!(windows) {
        let path = std::env::var("PATH").unwrap_or_default();
        command.env("PKG_CONFIG", tools.join("pkg-config"))
            .env("PATH", format!("{}:{path}", tools.display()));
    }

    if matches!(toolchain.kind, Kind::Zig | Kind::Gcc) {
        let triplets = chainloaded_triplets(&tools, toolchain, &installed.triplet)?;
        command.arg(format!("--overlay-triplets={}", triplets.display()));

        for (name, value) in &toolchain.environment {
            command.env(name, value);
        }
    }

    if let Kind::Ndk(ndk) = &toolchain.kind {
        let triplets = ndk_triplets(&tools, toolchain, ndk, &installed.triplet)?;
        command.arg(format!("--overlay-triplets={}", triplets.display()))
            .env("ANDROID_NDK_HOME", &ndk.home);
    }

    // vcpkg locks its root and gives up when another one holds the lock for long, modules install one by one
    system::installing(&vcpkg, || {
        system::log(module, &format!("Installing {} with vcpkg", ports.join(", ")));
        return system::run_command_quietly(&mut command, "vcpkg install");
    })?;

    system::write_file(&installed_manifest, &manifest)?;
    return Ok(Some(installed));
}

/// The triplet the ports of the toolchain are built for. Ports built by zig link with code zig compiled, also on
/// Linux, where the system has libstdc++, for another target zig builds them for that one. GCC builds its own,
/// with libstdc++ also on macOS. The NDK builds them for an Android target at the API level of the apps.
fn triplet(toolchain: &Toolchain, host: &str) -> String {
    // vcpkg names triplets in lowercase letters, digits and hyphens: zig-linux-x86-64-musl
    let target = toolchain.target.name().replace('_', "-");

    match (&toolchain.kind, toolchain.target.is_host()) {
        (Kind::Ndk(ndk), _) => return format!("ndk-{target}-{}", ndk.api_level),
        (Kind::System, _) => return host.to_string(),
        (Kind::Zig, true) => return format!("{host}-zig"),
        (Kind::Zig, false) => return format!("zig-{target}"),
        (Kind::Gcc, _) => return format!("{host}-gcc"),
    }
}

/// How vcpkg names the processor of a target
fn architecture(arch: Architecture) -> &'static str {
    match arch {
        Architecture::X86_64 => return "x64",
        Architecture::Aarch64 => return "arm64",
        Architecture::Armv7 => return "arm",
        Architecture::X86 => return "x86",
    }
}

/// The triplet of the ports the NDK builds for an Android target, like arm64-android of vcpkg, at the API level of
/// the apps and with the static libc++, which the libraries of build_c and build_cpp link for Android
fn ndk_triplets(tools: &Path, toolchain: &Toolchain, ndk: &Ndk, triplet: &str) -> Result<PathBuf, String> {
    let target = toolchain.target;
    let (Some(host), Some(abi)) = (target.ndk_triple(), target.android_abi()) else {
        return Err(format!("the NDK builds for Android, not for {}", target.name()));
    };

    let content = format!(
        r#"set(VCPKG_TARGET_ARCHITECTURE {})
set(VCPKG_CRT_LINKAGE static)
set(VCPKG_LIBRARY_LINKAGE static)
set(VCPKG_CMAKE_SYSTEM_NAME Android)
set(VCPKG_CMAKE_SYSTEM_VERSION {})
set(VCPKG_MAKE_BUILD_TRIPLET "--host={host}")
set(VCPKG_CMAKE_CONFIGURE_OPTIONS -DANDROID_ABI={abi})
set(VCPKG_ENV_PASSTHROUGH ANDROID_NDK_HOME)
"#,
        architecture(target.arch),
        ndk.api_level,
    );

    let triplets = tools.join("triplets").join(triplet);
    system::write_file_if_changed(&triplets.join(format!("{triplet}.cmake")), &content)?;

    return Ok(triplets);
}

/// "llvm[clang,lld]": the port and the features it is built with
pub fn port_and_features(port: &str) -> (&str, Vec<&str>) {
    let Some((name, features)) = port.split_once('[') else {
        return (port, vec![]);
    };

    let features = features.trim_end_matches(']')
        .split(',')
        .map(str::trim)
        .filter(|feature| !feature.is_empty());
    return (name, features.collect());
}

/// The port, once vcpkg has it and its features. Without a version: the release of vcpkg pins those.
pub fn checked_port(port: &str) -> Result<String, String> {
    if let Some((name, _)) = port.split_once([':', '@']) {
        return Err(format!("vcpkg {VCPKG_RELEASE} pins every port to one version: install {name}"));
    }

    let (name, features) = port_and_features(port);
    let manifest = install_vcpkg()?
        .join("ports")
        .join(name)
        .join("vcpkg.json");

    let Ok(manifest) = fs::read_to_string(&manifest) else {
        return Err(format!("vcpkg {VCPKG_RELEASE} has no port {name}, see https://vcpkg.io/en/packages"));
    };

    let manifest: serde_json::Value = serde_json::from_str(&manifest).map_err(|error| format!("the port {name}: {error}"))?;
    let known: Vec<&str> = manifest["features"].as_object()
        .map(|features| features.keys().map(String::as_str).collect())
        .unwrap_or_default();

    if let Some(unknown) = features.iter().find(|feature| !known.contains(feature)) {
        return Err(format!("the port {name} has no feature {unknown}, it has: {}", known.join(", ")));
    }

    return Ok(port.to_string());
}

/// A tool vcpkg downloads for its ports, like "cmake" and "ninja"
pub fn tool(name: &str) -> Result<PathBuf, String> {
    let vcpkg = install_vcpkg()?;
    system::create_dir(&system::cache_dir("vcpkg/downloads"))?;

    let mut fetch = Command::new(vcpkg.join("vcpkg"));
    fetch.args(["fetch", name])
        .env("VCPKG_ROOT", &vcpkg)
        .env("VCPKG_DOWNLOADS", system::cache_dir("vcpkg/downloads"))
        .env("VCPKG_DISABLE_METRICS", "1");

    // like install, one vcpkg at a time
    let output = system::installing(&vcpkg, || system::command_output(&mut fetch, &format!("vcpkg fetch {name}")))?;

    // the path is the last line
    let path = PathBuf::from(output.lines().last().unwrap_or("").trim());

    if !path.is_file() {
        return Err(format!("vcpkg fetch {name} named no program: {}", output.trim()));
    }

    return Ok(path);
}

/// vcpkg at its release into <build home>/vcpkg/<release>, its tool bootstrapped once
fn install_vcpkg() -> Result<PathBuf, String> {
    let root = system::data_dir("vcpkg").join(VCPKG_RELEASE);

    return system::installing(&root, || {
        if root.join(format!("vcpkg{}", std::env::consts::EXE_SUFFIX)).is_file() {
            return Ok(root.clone());
        }

        system::log_tool("vcpkg", &format!("Installing vcpkg {VCPKG_RELEASE}"));
        system::remove_dir(&root)?;

        let mut clone = Command::new("git");
        clone.args(["clone", "--quiet", "--depth", "1", "--branch", VCPKG_RELEASE])
            .arg("https://github.com/microsoft/vcpkg")
            .arg(&root);
        system::run_command_quietly(&mut clone, "git clone of vcpkg")?;

        let tools = root.join("build-script");

        // Windows takes its own, its programs have no RPATH
        if !cfg!(windows) {
            for (name, script) in [("pkg-config", PKG_CONFIG), ("patchelf", PATCHELF)] {
                write_executable(&tools.join(name), script)?;
            }
        }

        let mut bootstrap = Command::new(root.join(if cfg!(windows) { "bootstrap-vcpkg.bat" } else { "bootstrap-vcpkg.sh" }));
        bootstrap.arg("-disableMetrics");
        system::run_command_quietly(&mut bootstrap, "vcpkg bootstrap")?;

        return Ok(root.clone());
    });
}

/// The triplet of the ports zig or GCC builds, for this machine or another target, through the toolchain file of
/// CMake
fn chainloaded_triplets(tools: &Path, toolchain: &Toolchain, triplet: &str) -> Result<PathBuf, String> {
    let toolchain_file = toolchain.cmake_toolchain_file()?;
    let target = toolchain.target;

    let architecture = architecture(target.arch);

    // vcpkg names Windows without Visual Studio MinGW, its ports then name their libraries lib<name>.a
    let system_name = match target.os {
        OperatingSystem::MacOs => "Darwin",
        OperatingSystem::Windows => "MinGW",
        OperatingSystem::Linux | OperatingSystem::Android => "Linux",
    };

    let mut content = format!(
        r#"set(VCPKG_TARGET_ARCHITECTURE {architecture})
set(VCPKG_CRT_LINKAGE dynamic)
set(VCPKG_LIBRARY_LINKAGE static)
set(VCPKG_CMAKE_SYSTEM_NAME {system_name})
set(VCPKG_CHAINLOAD_TOOLCHAIN_FILE "{}")
set(VCPKG_ENV_PASSTHROUGH ZIG_GLOBAL_CACHE_DIR)
"#,
        // CMake reads a \ in a path as an escape
        toolchain_file.display().to_string().replace('\\', "/")
    );

    // the GCC of Windows finds the DLLs of its assembler and linker through the PATH
    if toolchain.kind == Kind::Gcc && target.os == OperatingSystem::Windows {
        content.push_str("set(VCPKG_ENV_PASSTHROUGH_UNTRACKED PATH)\n");
    }

    // GCC builds for its own processor alone, it takes no -arch
    if target.os == OperatingSystem::MacOs && toolchain.kind == Kind::Zig {
        content.push_str(&format!("set(VCPKG_OSX_ARCHITECTURES {})\n", if target.arch == Architecture::Aarch64 { "arm64" } else { "x86_64" }));
    }

    content.push_str(port_options(&toolchain.kind));

    let triplets = tools.join("triplets").join(triplet);
    system::write_file_if_changed(&triplets.join(format!("{triplet}.cmake")), &content)?;

    return Ok(triplets);
}

impl Installed {
    /// -I and -D of the ports, and what to link in order: of the ports' .pc files and of what
    /// they require, of ports without one the include directory and their libraries
    pub fn flags(&self, ports: &[String], release: bool) -> (Vec<String>, Vec<String>) {
        let include = self.root.join(&self.triplet).join("include");
        let mut compile_flags = vec![format!("-I{}", include.display())];
        let mut link_flags = vec![];
        let lib_dir = self.lib_dir(release);

        let mut ports: Vec<String> = ports.iter()
            .map(|port| port_and_features(port).0.to_string())
            .collect();
        let mut seen = HashSet::new();

        while !ports.is_empty() {
            let port = ports.remove(0);

            if !seen.insert(port.clone()) {
                continue;
            }

            let files = self.files_of(&port);

            let packages: Vec<String> = files.iter()
                .filter_map(|file| file.strip_prefix(&format!("{lib_dir}/pkgconfig/"))?.strip_suffix(".pc"))
                .map(str::to_string)
                .collect();

            // all of the port, like gtest and gmock, a main only the tests choose: gtest_main
            let chosen: Vec<String> = packages.into_iter()
                .filter(|name| !name.ends_with("main"))
                .collect();

            if chosen.is_empty() {
                let extension = if self.msvc { ".lib" } else { ".a" };
                let libraries: Vec<&String> = files.iter()
                    .filter(|file| file.starts_with(&lib_dir) && file.ends_with(extension))
                    .collect();

                // a port of nothing but others, like vulkan of vulkan-headers and vulkan-loader: theirs
                if libraries.is_empty() {
                    ports.extend(self.dependencies_of(&port));
                }

                link_flags.extend(libraries.iter().map(|file| self.root.join(file).display().to_string()));
                continue;
            }

            let (cflags, libs) = self.pkg_config(&chosen, release);
            compile_flags.extend(cflags);
            link_flags.extend(libs);
        }

        return (compile_flags, link_flags);
    }

    /// Like pkg-config --cflags --libs
    pub fn pkg_config(&self, names: &[String], release: bool) -> (Vec<String>, Vec<String>) {
        return pkg_config(names, &self.root.join(self.lib_dir(release)).join("pkgconfig"), self.msvc, self.of_the_machine);
    }

    /// arm64-osx/lib, of debug builds arm64-osx/debug/lib. The ports of MSVC have only those of release.
    fn lib_dir(&self, release: bool) -> String {
        return format!("{}/{}", self.triplet, if release || self.msvc { "lib" } else { "debug/lib" });
    }

    /// The shared libraries of its ports, which the programs linking them load: those vcpkg builds only that
    /// way, like the Vulkan loader. Of debug builds those of debug/, on Windows the DLLs in bin.
    pub fn shared_libraries(&self, release: bool) -> Vec<PathBuf> {
        let debug = !release && !self.msvc;
        let mut libraries = vec![];

        for name in ["bin", "lib"] {
            let directory = self.root.join(&self.triplet).join(if debug { format!("debug/{name}") } else { name.to_string() });
            let Ok(entries) = fs::read_dir(&directory) else { continue };
            let mut found: Vec<PathBuf> = entries.flatten()
                .map(|entry| entry.path())
                .filter(|path| is_shared_library(path))
                .collect();
            found.sort();
            libraries.extend(found);
        }

        return libraries;
    }

    /// The ports the port depends on, as vcpkg wrote them into its status: without features and host triplets
    fn dependencies_of(&self, port: &str) -> Vec<String> {
        let status = fs::read_to_string(self.root.join("vcpkg/status")).unwrap_or_default();
        return dependencies_in_status(&status, port, &self.triplet);
    }

    /// The files vcpkg installed for the port, relative to the install root
    fn files_of(&self, port: &str) -> Vec<String> {
        let list = system::files_with_extension(&[self.root.join("vcpkg/info")], "list")
            .into_iter()
            .find(|file| {
                let name = file.file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                let triplet = &self.triplet;
                return name.starts_with(&format!("{port}_")) && name.ends_with(&format!("_{triplet}.list"));
            });

        return list.and_then(|list| fs::read_to_string(list).ok())
            .map(|text| text.lines().map(str::to_string).collect())
            .unwrap_or_default();
    }
}

/// What some ports need of a triplet of a toolchain. The loader of Vulkan reads what -S writes as assembly when vcpkg
/// names the system, zig writes an object for -c -S: without its assembly it forwards no unknown functions of devices,
/// which no program of the core API calls.
fn port_options(kind: &Kind) -> &'static str {
    match kind {
        Kind::Zig => return "if(PORT STREQUAL \"vulkan-loader\")\n    list(APPEND VCPKG_CMAKE_CONFIGURE_OPTIONS -DUSE_GAS=OFF)\nendif()\n",
        _ => return "",
    }
}

/// libvulkan.1.dylib, libvulkan.so.1, vulkan-1.dll: a library programs load when they start
fn is_shared_library(path: &Path) -> bool {
    let name = path.file_name()
        .unwrap_or_default()
        .to_string_lossy();
    return name.ends_with(".dll") || name.ends_with(".dylib") || name.ends_with(".so") || name.contains(".so.");
}

/// Of the paragraphs of vcpkg's status the Depends of the port for the triplet, its features' too:
/// "vulkan-headers, vcpkg-cmake:arm64-osx, fmt[core](linux)" are vulkan-headers, vcpkg-cmake and fmt
fn dependencies_in_status(status: &str, port: &str, triplet: &str) -> Vec<String> {
    let mut dependencies = vec![];

    for paragraph in status.split("\n\n") {
        let field = |name: &str| paragraph.lines().find_map(|line| line.strip_prefix(&format!("{name}: ")));

        if field("Package") != Some(port) || field("Architecture") != Some(triplet) {
            continue;
        }

        for dependency in field("Depends").unwrap_or_default().split(',') {
            let name = dependency.trim()
                .split(['[', ':', '(', ' '])
                .next()
                .unwrap_or_default();

            if !name.is_empty() && !dependencies.iter().any(|known: &String| known == name) {
                dependencies.push(name.to_string());
            }
        }
    }

    return dependencies;
}

/// Cflags and libs of the packages and of what they require, in link order. Ports of MSVC have its flags.
fn pkg_config(names: &[String], pc_dir: &Path, msvc: bool, of_the_machine: bool) -> (Vec<String>, Vec<String>) {
    let mut visited = vec![];
    let mut cflags = vec![];
    let mut libs = vec![];

    for name in names {
        visit_package(name, pc_dir, msvc, &mut visited, &mut cflags, &mut libs);
    }

    // packages vcpkg did not install, like the x11 glfw3 requires on Linux, come from the system, for this machine
    let of_the_system: Vec<&String> = visited.iter()
        .filter(|name| !pc_dir.join(format!("{name}.pc")).is_file())
        .collect();

    if of_the_machine && !of_the_system.is_empty() && let Some(program) = system::find_program("pkg-config") {
        let flags = |option: &str| -> Vec<String> {
            let output = system::command_output(Command::new(&program).args(["--static", option]).args(&of_the_system), "pkg-config");
            return output.map(|output| output.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default();
        };

        cflags.extend(flags("--cflags"));
        libs.extend(flags("--libs"));
    }

    return (cflags, libs);
}

fn visit_package(
    name: &str,
    pc_dir: &Path,
    msvc: bool,
    visited: &mut Vec<String>,
    cflags: &mut Vec<String>,
    libs: &mut Vec<String>,
) {
    if visited.iter().any(|known| known == name) {
        return;
    }

    visited.push(name.to_string());

    let Ok(text) = fs::read_to_string(pc_dir.join(format!("{name}.pc"))) else {
        return;
    };

    let mut variables = HashMap::from([("pcfiledir".to_string(), pc_dir.display().to_string())]);
    let mut fields = HashMap::new();

    // ${name} of the variables defined before
    let expand = |value: &str, variables: &HashMap<String, String>| {
        let mut value = value.to_string();
        for (variable, replacement) in variables {
            value = value.replace(&format!("${{{variable}}}"), replacement);
        }
        return value;
    };

    for line in text.lines() {
        let variable = line.split_once('=').filter(|(key, _)| !key.contains([':', ' ']));

        if let Some((key, value)) = variable {
            let value = expand(value, &variables);
            variables.insert(key.to_string(), value);
        } else if let Some((key, value)) = line.split_once(':') {
            fields.insert(key.trim().to_lowercase(), expand(value.trim(), &variables));
        }
    }

    let words = |field: &str| -> Vec<String> {
        return fields.get(field)
            .map(|value| {
                // "" of an empty variable is no flag
                let words = value.split_whitespace().map(|word| word.trim_matches('"').to_string());
                return words.filter(|word| !word.is_empty()).collect();
            })
            .unwrap_or_default();
    };

    // the libraries are static: what they need themselves too, like pkg-config --static
    let requires: Vec<String> = ["requires", "requires.private"].iter()
        .filter_map(|field| fields.get(*field))
        .map(|names| names.replace(',', " "))
        .collect();

    for required in requires.join(" ").split_whitespace() {
        visit_package(required, pc_dir, msvc, visited, cflags, libs);
    }

    // a package links before what it requires
    cflags.extend(words("cflags").into_iter().filter_map(|flag| gnu_style(flag, msvc)));
    libs.splice(0..0, [words("libs"), words("libs.private")].concat().into_iter().map(|flag| linker_style(flag, msvc)));
}

/// A compile flag for clang. On Windows the .pc files of ports like abseil have those of MSVC: /D, /U and
/// /I become -D, -U and -I, the others are of MSVC only, like /W3 and /wd4005.
fn gnu_style(flag: String, windows: bool) -> Option<String> {
    if !windows || !flag.starts_with('/') {
        return Some(flag);
    }

    match flag.get(1..2) {
        Some("D" | "U" | "I") => return Some(format!("-{}", &flag[1..])),
        _ => return None,
    }
}

/// A link flag for clang. On Windows the .pc files of ports like abseil have options of link.exe, like
/// -ignore:4221: they go to it through -Wl.
fn linker_style(flag: String, windows: bool) -> String {
    let clang_flag = ["-l", "-L", "-W"].iter().any(|prefix| flag.starts_with(prefix));
    let linker_option = flag.starts_with('/') || (flag.starts_with('-') && flag.contains(':') && !clang_flag);

    if !windows || !linker_option {
        return flag;
    }

    return format!("-Wl,-{}", &flag[1..]);
}

#[cfg(test)]
mod tests {
    use crate::common::test_support::ScratchDir;

    use super::*;

    #[test]
    fn the_ndk_builds_the_ports_of_android_targets_at_the_api_level_of_the_apps() {
        use crate::common::target::Target;

        let toolchain = |target: &str, kind: Kind| Toolchain {
            kind,
            target: Target::named(target).unwrap(),
            c_compiler: vec![],
            cpp_compiler: vec![],
            archiver: vec![],
            environment: vec![],
        };
        let ndk = Ndk { home: PathBuf::from("/sdk/ndk/28.2.13676358"), api_level: 24 };

        assert_eq!(triplet(&toolchain("android-aarch64", Kind::Ndk(ndk.clone())), "arm64-osx"), "ndk-android-aarch64-24");
        assert_eq!(triplet(&toolchain("android-x86_64", Kind::Ndk(ndk.clone())), "arm64-osx"), "ndk-android-x86-64-24");
        assert_eq!(triplet(&toolchain("linux-x86_64-musl", Kind::Zig), "arm64-osx"), "zig-linux-x86-64-musl");
        assert_eq!(triplet(&toolchain(&Target::host().name(), Kind::System), "arm64-osx"), "arm64-osx");
        assert_eq!(triplet(&toolchain(&Target::host().name(), Kind::Gcc), "x64-linux"), "x64-linux-gcc");

        let directory = ScratchDir::new("ndk-triplets");
        let armv7 = toolchain("android-armv7", Kind::Ndk(ndk.clone()));
        let triplets = ndk_triplets(&directory, &armv7, &ndk, "ndk-android-armv7-24").unwrap();
        let content = system::read_file(&triplets.join("ndk-android-armv7-24.cmake")).unwrap();

        for line in [
            "set(VCPKG_TARGET_ARCHITECTURE arm)",
            "set(VCPKG_CRT_LINKAGE static)",
            "set(VCPKG_CMAKE_SYSTEM_NAME Android)",
            "set(VCPKG_CMAKE_SYSTEM_VERSION 24)",
            "set(VCPKG_MAKE_BUILD_TRIPLET \"--host=armv7a-linux-androideabi\")",
            "set(VCPKG_CMAKE_CONFIGURE_OPTIONS -DANDROID_ABI=armeabi-v7a)",
        ] {
            assert!(content.contains(line), "{line} is missing in\n{content}");
        }
    }

    #[test]
    fn ports_of_nothing_but_others_take_theirs_and_their_shared_libraries_are_found() {
        let status = "Package: vulkan\nVersion: 2023-12-17\nDepends: vcpkg-cmake, vulkan-headers, vulkan-loader\nArchitecture: arm64-osx\n\n\
                      Package: vulkan\nVersion: 2023-12-17\nDepends: vulkan-loader\nArchitecture: x64-linux\n\n\
                      Package: app\nFeature: tools\nDepends: fmt[core](linux), vcpkg-cmake:arm64-osx, zlib\nArchitecture: arm64-osx\n";
        assert_eq!(dependencies_in_status(status, "vulkan", "arm64-osx"), ["vcpkg-cmake", "vulkan-headers", "vulkan-loader"]);
        assert_eq!(dependencies_in_status(status, "app", "arm64-osx"), ["fmt", "vcpkg-cmake", "zlib"]);
        assert!(dependencies_in_status(status, "zlib", "arm64-osx").is_empty());

        // debug builds take those of debug/, the DLLs of Windows are in bin
        let directory = ScratchDir::new("port-shared-libraries");
        for file in ["arm64-osx/lib/libvulkan.1.dylib", "arm64-osx/lib/libfmt.a", "arm64-osx/debug/lib/libvulkan.1.dylib", "arm64-osx/lib/libz.so.1", "arm64-osx/bin/vulkan-1.dll", "arm64-osx/bin/glslc"] {
            system::write_file(&directory.join(file), "").unwrap();
        }
        let installed = Installed { root: directory.to_path_buf(), triplet: "arm64-osx".to_string(), msvc: false, of_the_machine: true };
        let names = |release: bool| -> Vec<String> {
            installed.shared_libraries(release)
                .iter()
                .map(|library| library.strip_prefix(&*directory).unwrap().to_string_lossy().replace('\\', "/"))
                .collect()
        };
        assert_eq!(names(true), ["arm64-osx/bin/vulkan-1.dll", "arm64-osx/lib/libvulkan.1.dylib", "arm64-osx/lib/libz.so.1"]);
        assert_eq!(names(false), ["arm64-osx/debug/lib/libvulkan.1.dylib"]);
    }

    #[test]
    fn zig_builds_the_loader_of_vulkan_without_its_assembly() {
        assert_eq!(port_options(&Kind::Zig), "if(PORT STREQUAL \"vulkan-loader\")\n    list(APPEND VCPKG_CMAKE_CONFIGURE_OPTIONS -DUSE_GAS=OFF)\nendif()\n");
        assert_eq!(port_options(&Kind::Gcc), "");
    }

    #[test]
    fn ports_take_features_but_no_versions() {
        let error = checked_port("fmt:10.2.1").unwrap_err();
        assert!(error.contains("install fmt"), "{error}");

        assert_eq!(port_and_features("llvm[clang, lld]"), ("llvm", vec!["clang", "lld"]));
        assert_eq!(port_and_features("fmt"), ("fmt", vec![]));
    }

    #[test]
    fn flags_of_msvc_become_those_of_clang_on_windows() {
        let flags = ["/DNOMINMAX", "/wd4005", "/bigobj", "-DSPDLOG_FMT_EXTERNAL"].map(String::from);
        let compiled = |windows: bool| -> Vec<String> {
            return flags.iter()
                .cloned()
                .filter_map(|flag| gnu_style(flag, windows))
                .collect();
        };

        assert_eq!(compiled(true), ["-DNOMINMAX", "-DSPDLOG_FMT_EXTERNAL"]);
        assert_eq!(compiled(false), flags);

        let libs = ["-ignore:4221", "-LC:/vcpkg/lib", "-labsl_base", "C:/vcpkg/lib/fmt.lib"].map(String::from);
        let linked = |windows: bool| -> Vec<String> {
            return libs.iter()
                .cloned()
                .map(|flag| linker_style(flag, windows))
                .collect();
        };

        assert_eq!(linked(true), ["-Wl,-ignore:4221", "-LC:/vcpkg/lib", "-labsl_base", "C:/vcpkg/lib/fmt.lib"]);
        assert_eq!(linked(false), libs);
    }

    #[test]
    fn packages_link_before_what_they_require() {
        let pc_dir = ScratchDir::new("pc");

        system::write_file(
            &pc_dir.join("fmt.pc"),
            "prefix=${pcfiledir}/../..\nlibdir=${prefix}/lib\nincludedir=${prefix}/include\n\nName: fmt\nCflags: -I${includedir} \"\"\nLibs: -L${libdir} -lfmt\n",
        )
        .unwrap();
        system::write_file(
            &pc_dir.join("spdlog.pc"),
            "prefix=${pcfiledir}/../..\nlibdir=${prefix}/lib\n\nName: libspdlog\nCFlags: -DSPDLOG_FMT_EXTERNAL\nLibs: \"-L${libdir}\" -lspdlog -pthread\nLibs.private: -ldl\nRequires.private: fmt build-script-no-package\n",
        )
        .unwrap();

        let (cflags, libs) = pkg_config(&["spdlog".to_string()], &pc_dir, false, true);
        let lib_dir = format!("-L{}/../../lib", pc_dir.display());

        assert_eq!(cflags, [format!("-I{}/../../include", pc_dir.display()), "-DSPDLOG_FMT_EXTERNAL".to_string()]);
        assert_eq!(libs, [lib_dir.clone(), "-lspdlog".to_string(), "-pthread".to_string(), "-ldl".to_string(), lib_dir, "-lfmt".to_string()]);

    }
}
