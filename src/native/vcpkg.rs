// Libraries of C and C++ modules from vcpkg: build-script installs vcpkg at a release, which pins every
// port to one version. Their flags come from their .pc files, of ports without one from their files.
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::common::system::{self, HostArchitecture, HostOperatingSystem};

use super::toolchain::Toolchain;

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
    if toolchain.zig && cfg!(windows) {
        return Err(format!("{module}: vcpkg ports with the zig toolchain are not set up for Windows, the system toolchain builds them"));
    }

    // ports built by zig link with code zig compiled, also on Linux, where the system has libstdc++
    let triplet = match toolchain.zig {
        true => format!("{host}-zig"),
        false => host.to_string(),
    };

    let manifest_dir = build_dir.join("vcpkg").join(&triplet);
    let installed = Installed {
        root: manifest_dir.join("installed"),
        triplet,
    };

    let dependencies: Vec<serde_json::Value> = ports.iter()
        .map(|port| match port_and_features(port) {
            (name, features) if features.is_empty() => serde_json::json!(name),
            (name, features) => serde_json::json!({ "name": name, "features": features }),
        })
        .collect();
    let manifest = serde_json::json!({ "dependencies": dependencies }).to_string();
    let manifest_file = manifest_dir.join("vcpkg.json");

    // vcpkg takes a second even when nothing changed
    let unchanged = fs::read_to_string(&manifest_file).is_ok_and(|existing| existing == manifest);

    if unchanged && installed.root.is_dir() {
        return Ok(Some(installed));
    }

    let vcpkg = install_vcpkg()?;
    let tools = vcpkg.join("build-script");
    system::write_file(&manifest_file, &manifest)?;

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

    if toolchain.zig {
        let triplets = zig_triplets(&tools, &toolchain.c_compiler[0], host)?;
        command.arg(format!("--overlay-triplets={}", triplets.display()));

        for (name, value) in &toolchain.environment {
            command.env(name, value);
        }
    }

    // vcpkg locks its root and gives up when another one holds the lock for long, modules install one by one
    system::installing(&vcpkg, || {
        system::log(module, &format!("Installing {} with vcpkg", ports.join(", ")));
        return system::run_command_quietly(&mut command, "vcpkg install");
    })?;

    return Ok(Some(installed));
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
    let output = system::installing(&vcpkg, || fetch.output().map_err(|error| format!("could not start vcpkg: {error}")))?;

    // the path is the last line
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).lines().last().unwrap_or("").trim());

    if !output.status.success() || !path.is_file() {
        return Err(format!("vcpkg could not fetch {name}: {}", String::from_utf8_lossy(&output.stderr).trim()));
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
                executable(&tools.join(name), script)?;
            }
        }

        let mut bootstrap = Command::new(root.join(if cfg!(windows) { "bootstrap-vcpkg.bat" } else { "bootstrap-vcpkg.sh" }));
        bootstrap.arg("-disableMetrics");
        system::run_command_quietly(&mut bootstrap, "vcpkg bootstrap")?;

        return Ok(root.clone());
    });
}

/// Triplets whose ports zig builds: a CMake toolchain file with zig cc, zig c++, zig ar and zig ranlib
fn zig_triplets(tools: &Path, zig: &str, host: &str) -> Result<PathBuf, String> {
    let zig_dir = tools.join("zig");

    // CMake takes a compiler as one program
    for tool in ["cc", "c++", "ar", "ranlib"] {
        executable(&zig_dir.join(tool), &format!("#!/bin/sh\nexec {zig} {tool} \"$@\"\n"))?;
    }

    let toolchain_file = zig_dir.join("toolchain.cmake");
    let tool = |name: &str| zig_dir.join(name).display().to_string();
    system::write_file(
        &toolchain_file,
        &format!(
            r#"set(CMAKE_C_COMPILER "{}")
set(CMAKE_CXX_COMPILER "{}")
set(CMAKE_AR "{}")
set(CMAKE_RANLIB "{}")
"#,
            tool("cc"),
            tool("c++"),
            tool("ar"),
            tool("ranlib")
        ),
    )?;

    // arm64-osx: arm64 on Darwin
    let (architecture, system_name) = match host.split_once('-') {
        Some((architecture, "osx")) => (architecture, "Darwin"),
        Some((architecture, _)) => (architecture, "Linux"),
        None => return Err(format!("unknown host triplet {host}")),
    };

    let triplets = zig_dir.join("triplets");
    system::write_file(
        &triplets.join(format!("{host}-zig.cmake")),
        &format!(
            r#"set(VCPKG_TARGET_ARCHITECTURE {architecture})
set(VCPKG_OSX_ARCHITECTURES {architecture})
set(VCPKG_CRT_LINKAGE dynamic)
set(VCPKG_LIBRARY_LINKAGE static)
set(VCPKG_CMAKE_SYSTEM_NAME {system_name})
set(VCPKG_CHAINLOAD_TOOLCHAIN_FILE "{}")
set(VCPKG_ENV_PASSTHROUGH ZIG_GLOBAL_CACHE_DIR)
"#,
            toolchain_file.display()
        ),
    )?;

    return Ok(triplets);
}

fn executable(file: &Path, script: &str) -> Result<(), String> {
    system::write_file(file, script)?;

    // Windows has no executable bit
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(file, fs::Permissions::from_mode(0o755))
            .map_err(|error| format!("could not make {} executable: {error}", file.display()))?;
    }

    return Ok(());
}

impl Installed {
    /// -I and -D of the ports, and what to link in order: of the ports' .pc files and of what
    /// they require, of ports without one the include directory and their libraries
    pub fn flags(&self, ports: &[String], release: bool) -> (Vec<String>, Vec<String>) {
        let include = self.root.join(&self.triplet).join("include");
        let mut compile_flags = vec![format!("-I{}", include.display())];
        let mut link_flags = vec![];
        let lib_dir = self.lib_dir(release);

        for port in ports {
            let files = self.files_of(port_and_features(port).0);

            let packages: Vec<String> = files.iter()
                .filter_map(|file| file.strip_prefix(&format!("{lib_dir}/pkgconfig/"))?.strip_suffix(".pc"))
                .map(str::to_string)
                .collect();

            // all of the port, like gtest and gmock, a main only the tests choose: gtest_main
            let chosen: Vec<String> = packages.into_iter()
                .filter(|name| !name.ends_with("main"))
                .collect();

            if chosen.is_empty() {
                let extension = if cfg!(windows) { ".lib" } else { ".a" };
                let libraries = files.iter().filter(|file| file.starts_with(&lib_dir) && file.ends_with(extension));
                link_flags.extend(libraries.map(|file| self.root.join(file).display().to_string()));
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
        return pkg_config(names, &self.root.join(self.lib_dir(release)).join("pkgconfig"));
    }

    /// arm64-osx/lib, of debug builds arm64-osx/debug/lib. Windows has only those of release.
    fn lib_dir(&self, release: bool) -> String {
        return format!("{}/{}", self.triplet, if release || cfg!(windows) { "lib" } else { "debug/lib" });
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

/// Cflags and libs of the packages and of what they require, in link order
fn pkg_config(names: &[String], pc_dir: &Path) -> (Vec<String>, Vec<String>) {
    let mut visited = vec![];
    let mut cflags = vec![];
    let mut libs = vec![];

    for name in names {
        visit_package(name, pc_dir, &mut visited, &mut cflags, &mut libs);
    }

    // packages vcpkg did not install, like the x11 glfw3 requires on Linux, come from the system
    let of_the_system: Vec<&String> = visited.iter()
        .filter(|name| !pc_dir.join(format!("{name}.pc")).is_file())
        .collect();

    if !of_the_system.is_empty() && let Some(program) = system::find_program("pkg-config") {
        let flags = |option: &str| -> Vec<String> {
            let output = Command::new(&program).args(["--static", option]).args(&of_the_system).output();
            return output.ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).split_whitespace().map(str::to_string).collect())
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
        visit_package(required, pc_dir, visited, cflags, libs);
    }

    // a package links before what it requires
    cflags.extend(words("cflags").into_iter().filter_map(gnu_style));
    libs.splice(0..0, [words("libs"), words("libs.private")].concat().into_iter().map(linker_style));
}

/// A compile flag for clang. On Windows the .pc files of ports like abseil have those of MSVC: /D, /U and
/// /I become -D, -U and -I, the others are of MSVC only, like /W3 and /wd4005.
fn gnu_style(flag: String) -> Option<String> {
    if !cfg!(windows) || !flag.starts_with('/') {
        return Some(flag);
    }

    match flag.get(1..2) {
        Some("D" | "U" | "I") => return Some(format!("-{}", &flag[1..])),
        _ => return None,
    }
}

/// A link flag for clang. On Windows the .pc files of ports like abseil have options of link.exe, like
/// -ignore:4221: they go to it through -Wl.
fn linker_style(flag: String) -> String {
    let clang_flag = ["-l", "-L", "-W"].iter().any(|prefix| flag.starts_with(prefix));
    let linker_option = flag.starts_with('/') || (flag.starts_with('-') && flag.contains(':') && !clang_flag);

    if !cfg!(windows) || !linker_option {
        return flag;
    }

    return format!("-Wl,-{}", &flag[1..]);
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let converted: Vec<String> = flags.iter()
            .cloned()
            .filter_map(gnu_style)
            .collect();

        match cfg!(windows) {
            true => assert_eq!(converted, ["-DNOMINMAX", "-DSPDLOG_FMT_EXTERNAL"]),
            false => assert_eq!(converted, flags),
        }

        let libs = ["-ignore:4221", "-LC:/vcpkg/lib", "-labsl_base", "C:/vcpkg/lib/fmt.lib"].map(String::from);
        let converted: Vec<String> = libs.iter()
            .cloned()
            .map(linker_style)
            .collect();

        match cfg!(windows) {
            true => assert_eq!(converted, ["-Wl,-ignore:4221", "-LC:/vcpkg/lib", "-labsl_base", "C:/vcpkg/lib/fmt.lib"]),
            false => assert_eq!(converted, libs),
        }
    }

    #[test]
    fn packages_link_before_what_they_require() {
        let pc_dir = std::env::temp_dir().join(format!("build-script-pc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&pc_dir);

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

        let (cflags, libs) = pkg_config(&["spdlog".to_string()], &pc_dir);
        let lib_dir = format!("-L{}/../../lib", pc_dir.display());

        assert_eq!(cflags, [format!("-I{}/../../include", pc_dir.display()), "-DSPDLOG_FMT_EXTERNAL".to_string()]);
        assert_eq!(libs, [lib_dir.clone(), "-lspdlog".to_string(), "-pthread".to_string(), "-ldl".to_string(), lib_dir, "-lfmt".to_string()]);

        fs::remove_dir_all(&pc_dir).unwrap();
    }
}
