// The clang of the NDK for the Android targets of C and C++ modules. Apps pack the shared libraries the
// evaluations for android-aarch64 and the other ABIs build, like jniLibs, with the lowest min_sdk of those apps.
use crate::common::system::{self, HostOperatingSystem};
use crate::common::target::Target;
use crate::jvm::android;
use crate::language::runtime::AndroidSettings;

use super::toolchain::{Ndk, Toolchain};

/// What a module named for an Android target without an app that packs it builds with: the defaults of
/// build_android
pub fn default_settings() -> AndroidSettings {
    return AndroidSettings {
        min_sdk: android::DEFAULT_MIN_SDK,
        ndk_version: android::DEFAULT_NDK_VERSION.to_string(),
        accept_sdk_license: false,
    };
}

/// clang, clang++ and llvm-ar of the NDK, for the ABI of the target and the API level of min_sdk
pub fn toolchain(target: Target, settings: &AndroidSettings) -> Result<Toolchain, String> {
    let ndk = android::ndk(&settings.ndk_version, settings.accept_sdk_license)?;

    // the NDK has one prebuilt clang for both Mac architectures
    let host_tag = match system::host_operating_system()? {
        HostOperatingSystem::MacOs => "darwin-x86_64",
        HostOperatingSystem::Linux => "linux-x86_64",
        HostOperatingSystem::Windows => "windows-x86_64",
    };
    let bin = ndk.join("toolchains/llvm/prebuilt")
        .join(host_tag)
        .join("bin");
    let tool = |name: &str| bin.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)).display().to_string();

    let triple = target.ndk_triple().ok_or_else(|| format!("the NDK builds for Android, not for {}", target.name()))?;
    let clang_target = format!("--target={triple}{}", settings.min_sdk);

    return Ok(Toolchain {
        zig: false,
        target,
        c_compiler: vec![tool("clang"), clang_target.clone()],
        cpp_compiler: vec![tool("clang++"), clang_target],
        archiver: vec![tool("llvm-ar")],
        environment: vec![],
        ndk: Some(Ndk { home: ndk.clone(), api_level: settings.min_sdk }),
    });
}
