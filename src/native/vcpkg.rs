// Libraries of C and C++ modules from vcpkg: build-script installs vcpkg at a release, which pins every
// port to one version. Their flags come from their .pc files, of ports without one from their files.
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::common::system::{self, HostArchitecture, HostOperatingSystem};

const VCPKG_RELEASE: &str = "2026.10.16";

/// vcpkg checks its .pc files with pkg-config and fixes the RPATHs of Linux programs with patchelf,
/// it does not bring them on macOS and Linux. Neither matters for static libraries.
const PKG_CONFIG: &str = "#!/bin/sh\ncase \"$1\" in --version) echo 2.1.0;; esac\nexit 0\n";
const PATCHELF: &str = "#!/bin/sh\ncase \"$1\" in --version) echo patchelf 0.18.0;; esac\nexit 0\n";

/// The ports of a module, installed into build/vcpkg/<triplet>
pub struct Installed {
    triplet: String,
    root: PathBuf,
}

/// Installs the ports, only when they changed. None without ports.
pub fn install(module: &str, ports: &[String], build_dir: &Path) -> Result<Option<Installed>, String> {
    if ports.is_empty() {
        return Ok(None);
    }

    let host = match (system::host_operating_system()?, system::host_architecture()?) {
        (HostOperatingSystem::MacOs, HostArchitecture::Aarch64) => "arm64-osx",
        (HostOperatingSystem::MacOs, HostArchitecture::X86_64) => "x64-osx",
        (HostOperatingSystem::Linux, HostArchitecture::Aarch64) => "arm64-linux",
        (HostOperatingSystem::Linux, HostArchitecture::X86_64) => "x64-linux",
        (HostOperatingSystem::Windows, _) => return Err("C and C++ are not set up for Windows".into()),
    };

    let manifest_dir = build_dir.join("vcpkg").join(host);
    let installed = Installed {
        root: manifest_dir.join("installed"),
        triplet: host.to_string(),
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

    let path = std::env::var("PATH").unwrap_or_default();
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
        .env("VCPKG_DISABLE_METRICS", "1")
        .env("PKG_CONFIG", tools.join("pkg-config"))
        .env("PATH", format!("{}:{path}", tools.display()));

    system::log(module, &format!("Installing {} with vcpkg", ports.join(", ")));
    system::run_command_quietly(&mut command, "vcpkg install")?;

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

/// vcpkg at its release into <build home>/vcpkg/<release>, its tool bootstrapped once
fn install_vcpkg() -> Result<PathBuf, String> {
    let root = system::data_dir("vcpkg").join(VCPKG_RELEASE);

    return system::installing(|| {
        if root.join("vcpkg").is_file() {
            return Ok(root.clone());
        }

        system::log("vcpkg", &format!("Installing vcpkg {VCPKG_RELEASE}"));
        system::remove_dir(&root)?;

        let mut clone = Command::new("git");
        clone.args(["clone", "--quiet", "--depth", "1", "--branch", VCPKG_RELEASE])
            .arg("https://github.com/microsoft/vcpkg")
            .arg(&root);
        system::run_command_quietly(&mut clone, "git clone of vcpkg")?;

        let tools = root.join("build-script");

        for (name, script) in [("pkg-config", PKG_CONFIG), ("patchelf", PATCHELF)] {
            executable(&tools.join(name), script)?;
        }

        let mut bootstrap = Command::new(root.join("bootstrap-vcpkg.sh"));
        bootstrap.arg("-disableMetrics");
        system::run_command_quietly(&mut bootstrap, "vcpkg bootstrap")?;

        return Ok(root.clone());
    });
}

fn executable(file: &Path, script: &str) -> Result<(), String> {
    system::write_file(file, script)?;

    return fs::set_permissions(file, fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("could not make {} executable: {error}", file.display()));
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
                let libraries = files.iter().filter(|file| file.starts_with(&lib_dir) && file.ends_with(".a"));
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

    /// arm64-osx/lib, of debug builds arm64-osx/debug/lib
    fn lib_dir(&self, release: bool) -> String {
        return format!("{}/{}", self.triplet, if release { "lib" } else { "debug/lib" });
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
            .map(|value| value.split_whitespace().map(|word| word.trim_matches('"').to_string()).collect())
            .unwrap_or_default();
    };

    for required in fields.get("requires").cloned().unwrap_or_default().replace(',', " ").split_whitespace() {
        visit_package(required, pc_dir, visited, cflags, libs);
    }

    // a package links before what it requires
    cflags.extend(words("cflags"));
    libs.splice(0..0, words("libs"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_take_features() {
        assert_eq!(port_and_features("llvm[clang, lld]"), ("llvm", vec!["clang", "lld"]));
        assert_eq!(port_and_features("fmt"), ("fmt", vec![]));
    }

    #[test]
    fn packages_link_before_what_they_require() {
        let pc_dir = std::env::temp_dir().join(format!("build-script-pc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&pc_dir);

        system::write_file(
            &pc_dir.join("fmt.pc"),
            "prefix=${pcfiledir}/../..\nlibdir=${prefix}/lib\nincludedir=${prefix}/include\n\nName: fmt\nCflags: -I${includedir}\nLibs: -L${libdir} -lfmt\n",
        )
        .unwrap();
        system::write_file(
            &pc_dir.join("spdlog.pc"),
            "prefix=${pcfiledir}/../..\nlibdir=${prefix}/lib\n\nName: libspdlog\nCFlags: -DSPDLOG_FMT_EXTERNAL\nLibs: \"-L${libdir}\" -lspdlog -pthread\nRequires: fmt\n",
        )
        .unwrap();

        let (cflags, libs) = pkg_config(&["spdlog".to_string()], &pc_dir);
        let lib_dir = format!("-L{}/../../lib", pc_dir.display());

        assert_eq!(cflags, [format!("-I{}/../../include", pc_dir.display()), "-DSPDLOG_FMT_EXTERNAL".to_string()]);
        assert_eq!(libs, [lib_dir.clone(), "-lspdlog".to_string(), "-pthread".to_string(), lib_dir, "-lfmt".to_string()]);

        fs::remove_dir_all(&pc_dir).unwrap();
    }
}
