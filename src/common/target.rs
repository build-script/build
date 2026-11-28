// What xkit builds for: the machine it runs on, or another system. A target is named
// <system>-<processor>, Linux also with -musl, like linux-aarch64, linux-x86_64-musl or windows-x86_64.

/// The system a target runs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperatingSystem {
    Linux,
    MacOs,
    Windows,
    Android,
}

/// The processor a target runs on
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Architecture {
    X86_64,
    Aarch64,
    Armv7,
    X86,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Target {
    pub os: OperatingSystem,
    pub arch: Architecture,
    /// a Linux with musl instead of glibc: programs bring their libc along and run on every distribution
    pub musl: bool,
}

/// What CMake and Visual Studio link every program of Windows with
const WINDOWS_LIBRARIES: &[&str] = &["kernel32", "user32", "gdi32", "winspool", "shell32", "ole32", "oleaut32", "uuid", "comdlg32", "advapi32"];

/// The oldest glibc that programs for another Linux than this machine run on: Debian 10, Ubuntu 18.10, RHEL 8
const GLIBC: &str = "2.28";

const fn target(os: OperatingSystem, arch: Architecture, musl: bool) -> Target {
    return Target { os, arch, musl };
}

/// Every target, in the order help and errors name them
pub const TARGETS: &[Target] = &[
    target(OperatingSystem::Linux, Architecture::X86_64, false),
    target(OperatingSystem::Linux, Architecture::Aarch64, false),
    target(OperatingSystem::Linux, Architecture::X86_64, true),
    target(OperatingSystem::Linux, Architecture::Aarch64, true),
    target(OperatingSystem::MacOs, Architecture::Aarch64, false),
    target(OperatingSystem::MacOs, Architecture::X86_64, false),
    target(OperatingSystem::Windows, Architecture::X86_64, false),
    target(OperatingSystem::Windows, Architecture::Aarch64, false),
    target(OperatingSystem::Android, Architecture::Aarch64, false),
    target(OperatingSystem::Android, Architecture::Armv7, false),
    target(OperatingSystem::Android, Architecture::X86, false),
    target(OperatingSystem::Android, Architecture::X86_64, false),
];

impl Target {
    /// The machine xkit runs on. On a system it has no release for, the closest one: a script still
    /// learns the truth from host_operating_system() and host_architecture().
    pub fn host() -> Target {
        let os = match std::env::consts::OS {
            "macos" => OperatingSystem::MacOs,
            "windows" => OperatingSystem::Windows,
            _ => OperatingSystem::Linux,
        };

        let arch = match std::env::consts::ARCH {
            "aarch64" => Architecture::Aarch64,
            "arm" => Architecture::Armv7,
            "x86" => Architecture::X86,
            _ => Architecture::X86_64,
        };

        return Target { os, arch, musl: false };
    }

    /// One of the targets, by its name
    pub fn named(name: &str) -> Option<Target> {
        return TARGETS.iter()
            .find(|target| target.name() == name)
            .copied();
    }

    /// The names of every target, for help and the hint at a misspelled one
    pub fn names() -> Vec<String> {
        return TARGETS.iter()
            .map(Target::name)
            .collect();
    }

    pub fn name(&self) -> String {
        let musl = if self.musl { "-musl" } else { "" };
        return format!("{}-{}{musl}", self.os.name(), self.arch.name());
    }

    pub fn is_host(&self) -> bool {
        return *self == Target::host();
    }

    /// Whether this machine starts its programs itself: those of its own target, of x86_64 on Apple silicon
    /// (Rosetta) and on Windows on ARM, and of Linux for its processor, the static ones of musl too
    pub fn runs_on(&self, machine: Target) -> bool {
        use Architecture::*;
        use OperatingSystem::*;

        return match (machine.os, machine.arch, self.os, self.arch) {
            (MacOs, Aarch64, MacOs, X86_64) | (Windows, Aarch64, Windows, X86_64) => true,
            (Linux, ours, Linux, theirs) => ours == theirs,
            (ours, ours_arch, theirs, theirs_arch) => ours == theirs && ours_arch == theirs_arch,
        };
    }

    /// What zig builds for with -target, none for Android, which the NDK builds for
    pub fn zig_triple(&self) -> Option<String> {
        let arch = match self.arch {
            Architecture::X86_64 => "x86_64",
            Architecture::Aarch64 => "aarch64",
            Architecture::Armv7 => "arm",
            Architecture::X86 => "x86",
        };

        match (self.os, self.musl) {
            (OperatingSystem::Linux, true) => return Some(format!("{arch}-linux-musl")),
            (OperatingSystem::Linux, false) => return Some(format!("{arch}-linux-gnu.{GLIBC}")),
            (OperatingSystem::MacOs, _) => return Some(format!("{arch}-macos")),
            // MinGW: the libc of Windows without Visual Studio
            (OperatingSystem::Windows, _) => return Some(format!("{arch}-windows-gnu")),
            (OperatingSystem::Android, _) => return None,
        }
    }

    /// What the clang of the NDK builds for, before the API level: aarch64-linux-android24
    pub fn ndk_triple(&self) -> Option<&'static str> {
        if self.os != OperatingSystem::Android {
            return None;
        }

        match self.arch {
            Architecture::Aarch64 => return Some("aarch64-linux-android"),
            Architecture::Armv7 => return Some("armv7a-linux-androideabi"),
            Architecture::X86 => return Some("i686-linux-android"),
            Architecture::X86_64 => return Some("x86_64-linux-android"),
        }
    }

    /// The ABI Android names it by, the directory of its libraries in an apk
    pub fn android_abi(&self) -> Option<&'static str> {
        if self.os != OperatingSystem::Android {
            return None;
        }

        match self.arch {
            Architecture::Aarch64 => return Some("arm64-v8a"),
            Architecture::Armv7 => return Some("armeabi-v7a"),
            Architecture::X86 => return Some("x86"),
            Architecture::X86_64 => return Some("x86_64"),
        }
    }

    /// The directory below include/ of a JDK with its jni_md.h
    pub fn jni_system(&self) -> &'static str {
        match self.os {
            OperatingSystem::MacOs => return "darwin",
            OperatingSystem::Windows => return "win32",
            OperatingSystem::Linux | OperatingSystem::Android => return "linux",
        }
    }
}

impl OperatingSystem {
    /// What operating_system() says
    pub fn name(self) -> &'static str {
        match self {
            OperatingSystem::Linux => return "linux",
            OperatingSystem::MacOs => return "macos",
            OperatingSystem::Windows => return "windows",
            OperatingSystem::Android => return "android",
        }
    }

    pub fn executable(self, name: &str) -> String {
        match self {
            OperatingSystem::Windows => return format!("{name}.exe"),
            _ => return name.to_string(),
        }
    }

    /// On Windows also the import library that links a shared one
    pub fn static_library(self, name: &str) -> String {
        match self {
            OperatingSystem::Windows => return format!("{name}.lib"),
            _ => return format!("lib{name}.a"),
        }
    }

    pub fn shared_library(self, name: &str) -> String {
        match self {
            OperatingSystem::MacOs => return format!("lib{name}.dylib"),
            OperatingSystem::Windows => return format!("{name}.dll"),
            _ => return format!("lib{name}.so"),
        }
    }

    /// Where in build/ shared libraries lie: Windows loads them from the directory of the program
    pub fn shared_dir(self) -> &'static str {
        match self {
            OperatingSystem::Windows => return "bin",
            _ => return "lib",
        }
    }

    /// How an rpath names the directory of the program or library, Windows has none
    pub fn rpath_origin(self) -> Option<&'static str> {
        match self {
            OperatingSystem::MacOs => return Some("@loader_path"),
            OperatingSystem::Windows => return None,
            _ => return Some("$ORIGIN"),
        }
    }

    /// Shared libraries of Linux link only position independent code, also that of the static ones in them
    pub fn needs_position_independent_code(self) -> bool {
        return matches!(self, OperatingSystem::Linux | OperatingSystem::Android);
    }

    /// C programs of glibc link the math library, the C++ standard library brings it along, on macOS
    /// libSystem has it
    pub fn links_math_library(self) -> bool {
        return self == OperatingSystem::Linux;
    }

    /// What every program links besides
    pub fn system_libraries(self) -> &'static [&'static str] {
        match self {
            OperatingSystem::Windows => return WINDOWS_LIBRARIES,
            _ => return &[],
        }
    }
}

impl Architecture {
    /// What architecture() says
    pub fn name(self) -> &'static str {
        match self {
            Architecture::X86_64 => return "x86_64",
            Architecture::Aarch64 => return "aarch64",
            Architecture::Armv7 => return "armv7",
            Architecture::X86 => return "x86",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_are_named_by_system_processor_and_libc() {
        assert_eq!(Target::names().len(), TARGETS.len());

        for name in Target::names() {
            assert_eq!(Target::named(&name).map(|target| target.name()), Some(name.clone()), "{name} names itself");
        }

        assert_eq!(Target::named("linux-x86_64-musl"), Some(target(OperatingSystem::Linux, Architecture::X86_64, true)));
        assert_eq!(Target::named("linux-amd64"), None);
        assert!(Target::host().is_host());
    }

    #[test]
    fn every_target_has_a_compiler_that_builds_for_it() {
        let triple = |name: &str| Target::named(name).unwrap().zig_triple();

        assert_eq!(triple("linux-aarch64").as_deref(), Some("aarch64-linux-gnu.2.28"));
        assert_eq!(triple("linux-x86_64-musl").as_deref(), Some("x86_64-linux-musl"));
        assert_eq!(triple("macos-aarch64").as_deref(), Some("aarch64-macos"));
        assert_eq!(triple("windows-x86_64").as_deref(), Some("x86_64-windows-gnu"));
        assert_eq!(triple("android-aarch64"), None);

        // zig builds for the desktop systems, the NDK for Android, by the ABIs of apks
        for target in TARGETS {
            assert_eq!(target.ndk_triple().is_some(), target.os == OperatingSystem::Android, "{}", target.name());
            assert_eq!(target.zig_triple().is_some(), target.os != OperatingSystem::Android, "{}", target.name());
        }

        let armv7 = Target::named("android-armv7").unwrap();
        assert_eq!((armv7.android_abi(), armv7.ndk_triple()), (Some("armeabi-v7a"), Some("armv7a-linux-androideabi")));
        assert_eq!(Target::named("windows-aarch64").unwrap().jni_system(), "win32");
    }

    #[test]
    fn every_system_names_its_programs_and_libraries() {
        let names = |os: OperatingSystem| {
            return (os.executable("app"), os.static_library("core"), os.shared_library("core"), os.shared_dir());
        };

        assert_eq!(names(OperatingSystem::MacOs), ("app".into(), "libcore.a".into(), "libcore.dylib".into(), "lib"));
        assert_eq!(names(OperatingSystem::Linux), ("app".into(), "libcore.a".into(), "libcore.so".into(), "lib"));
        assert_eq!(names(OperatingSystem::Android), ("app".into(), "libcore.a".into(), "libcore.so".into(), "lib"));
        assert_eq!(names(OperatingSystem::Windows), ("app.exe".into(), "core.lib".into(), "core.dll".into(), "bin"));

        // Windows finds shared libraries next to the program, without an rpath
        assert_eq!(OperatingSystem::MacOs.rpath_origin(), Some("@loader_path"));
        assert_eq!(OperatingSystem::Windows.rpath_origin(), None);

        // flags of the target, not of the host that builds for it
        assert!(OperatingSystem::Android.needs_position_independent_code());
        assert!(!OperatingSystem::MacOs.needs_position_independent_code());
        assert!(OperatingSystem::Linux.links_math_library());
        assert!(!OperatingSystem::Android.links_math_library());
        assert_eq!(OperatingSystem::Windows.system_libraries().first(), Some(&"kernel32"));
    }
}
