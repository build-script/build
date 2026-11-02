// Shared libraries of C and C++ modules for Android apps: build_android builds them for its ABIs and its
// min_sdk with the clang of the NDK, into build/android/<abi>/lib<name>.so of the module, and packs them like
// jniLibs. The module's own sources, headers and defines: vcpkg ports and the libraries of other modules
// are built for this machine only.
use std::path::PathBuf;

use crate::common::system::{self, HostOperatingSystem};
use crate::jvm::android;

use super::compile::{self, Compilation, Language};
use super::toolchain::Toolchain;

/// ABI and the target of clang without its API level
const TARGETS: &[(&str, &str)] = &[
    ("arm64-v8a", "aarch64-linux-android"),
    ("armeabi-v7a", "armv7a-linux-androideabi"),
    ("x86", "i686-linux-android"),
    ("x86_64", "x86_64-linux-android"),
];

/// What a shared library of build_cpp compiles for Android
#[derive(Debug, Hash)]
pub struct AndroidSources {
    pub name: String,
    pub language: Language,
    pub module_dir: PathBuf,
    pub source_patterns: Vec<String>,
    pub exclude_patterns: Vec<String>,
    pub c_standard: String,
    pub cpp_standard: String,
    /// its headers and defines, without what is of this machine
    pub flags: Vec<String>,
    pub build_dir: PathBuf,
}

/// build/android/ of the module, with a directory of the library per ABI
pub fn build(
    sources: &AndroidSources,
    abis: &[&str],
    min_sdk: i64,
    ndk_version: &str,
    accept_license: bool,
    release: bool,
) -> Result<PathBuf, String> {
    let ndk = android::ndk(ndk_version, accept_license)?;

    // the NDK has one prebuilt clang for both Mac architectures
    let host_tag = match system::host_operating_system()? {
        HostOperatingSystem::MacOs => "darwin-x86_64",
        HostOperatingSystem::Linux => "linux-x86_64",
        HostOperatingSystem::Windows => return Err("Android libraries are not set up for Windows hosts".into()),
    };
    let bin = ndk.join("toolchains/llvm/prebuilt")
        .join(host_tag)
        .join("bin");
    let name = &sources.name;
    let output_dir = sources.build_dir.join("android");

    for abi in abis {
        let Some((_, target)) = TARGETS.iter().find(|(known, _)| known == abi) else {
            return Err(format!("{abi} is no Android ABI"));
        };

        let target = format!("--target={target}{min_sdk}");
        let toolchain = Toolchain {
            zig: false,
            windows: false,
            c_compiler: vec![bin.join("clang").display().to_string(), target.clone()],
            cpp_compiler: vec![bin.join("clang++").display().to_string(), target],
            archiver: vec![bin.join("llvm-ar").display().to_string()],
            environment: vec![],
        };

        let compilation = Compilation {
            module_name: name,
            language: sources.language,
            toolchain: &toolchain,
            module_dir: &sources.module_dir,
            source_patterns: &sources.source_patterns,
            exclude_patterns: &sources.exclude_patterns,
            flags: sources.flags.clone(),
            c_standard: &sources.c_standard,
            cpp_standard: &sources.cpp_standard,
            objects_dir: sources.build_dir.join("obj").join(name).join(format!("android-{abi}")).join(if release { "release" } else { "debug" }),
            tests: false,
        };

        let (objects, compiled) = compilation.run()?;
        let library = output_dir.join(abi).join(format!("lib{name}.so"));

        if compiled || !library.is_file() {
            // libc++ inside, the app needs no libc++_shared.so; liblog for __android_log_print
            let cpp = sources.language == Language::Cpp;
            let mut link_flags = vec!["-shared".to_string(), format!("-Wl,-soname,lib{name}.so"), "-llog".to_string()];
            link_flags.extend(cpp.then(|| "-static-libstdc++".to_string()));

            compile::link(&toolchain, cpp, &objects, &link_flags, &library)?;
            system::log(name, &format!("Done: {}", library.display()));
        }
    }

    return Ok(output_dir);
}
