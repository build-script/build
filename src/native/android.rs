// Shared libraries of C and C++ modules for Android apps: build_android builds them for its ABIs and its
// min_sdk with the clang of the NDK, into build/android/<debug|release>/<abi>/lib<name>.so of the module, and
// packs them like jniLibs. The module's own sources, headers and defines: vcpkg ports and the libraries of other modules
// are built for this machine only.
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::common::system::{self, HostOperatingSystem};
use crate::common::up_to_date::InputFingerprint;
use crate::jvm::android;

use super::compile::{self, Compilation, Language};
use super::toolchain::{NativeTarget, Toolchain};

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

/// build/android/<debug|release>/ of the module, with a directory of the library per ABI
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
        HostOperatingSystem::Windows => "windows-x86_64",
    };
    let bin = ndk.join("toolchains/llvm/prebuilt")
        .join(host_tag)
        .join("bin");
    let name = &sources.name;
    let build_type = if release { "release" } else { "debug" };

    // of its own per build type: an app packs the libraries of the one it is built for
    let output_dir = sources.build_dir.join("android").join(build_type);

    for abi in abis {
        let Some((_, target)) = TARGETS.iter().find(|(known, _)| known == abi) else {
            return Err(format!("{abi} is no Android ABI"));
        };

        let target = format!("--target={target}{min_sdk}");
        let tool = |name: &str| bin.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)).display().to_string();
        let toolchain = Toolchain {
            zig: false,
            target: NativeTarget::Android,
            c_compiler: vec![tool("clang"), target.clone()],
            cpp_compiler: vec![tool("clang++"), target],
            archiver: vec![tool("llvm-ar")],
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
            objects_dir: sources.build_dir.join("obj").join(name).join(format!("android-{abi}")).join(build_type),
            tests: false,
            candidates: OnceLock::new(),
            generated_sources: vec![],
        };

        let (objects, _) = compilation.run()?;
        let library = output_dir.join(abi).join(format!("lib{name}.so"));

        // libc++ inside, the app needs no libc++_shared.so; liblog for __android_log_print
        let cpp = sources.language == Language::Cpp;
        let mut link_flags = vec!["-shared".to_string(), format!("-Wl,-soname,lib{name}.so"), "-llog".to_string()];
        link_flags.extend(cpp.then(|| "-static-libstdc++".to_string()));

        // linked again when an object changed, also when a source is gone
        let mut fingerprint = InputFingerprint::for_module(&sources.module_dir, &format!("android/{name}-{abi}-{build_type}"));
        fingerprint.add(&toolchain.cpp_compiler);
        fingerprint.add(&link_flags);
        fingerprint.add_files(&objects)?;

        if !fingerprint.is_up_to_date(&[&library])? {
            compile::link(&toolchain, cpp, &objects, &link_flags, &library)?;
            fingerprint.store_done(name, &library)?;
        }
    }

    return Ok(output_dir);
}
