// JVM: Kotlin and Java modules and Android apps, built with tools and libraries from Maven repositories
pub mod android;
mod generator;
pub mod idea;
pub mod install;
mod jar;
mod jar_merge;
mod jdk;
mod ksp;
pub mod maven;
mod module;
mod resources;
pub mod templates;
mod testing;

use std::path::PathBuf;
use std::process::Command;

use crate::language::runtime::{BuiltModule, ModuleOutput, Platform, Value};

use android::AndroidResources;
use maven::MavenCoordinate;

pub use jdk::jni_include_dirs;

/// `use kotlin`, `use java` and `use android`
pub const PLATFORMS: &[Platform] = &[
    Platform {
        name: "kotlin",
        functions: || [jar::kotlin_builtins(), shared_functions()].concat(),
    },
    Platform {
        name: "java",
        functions: || [jar::java_builtins(), shared_functions()].concat(),
    },
    Platform {
        name: "android",
        functions: || [android::builtins(), shared_functions()].concat(),
    },
];

/// What JVM modules depending on a module need from it
#[derive(Debug, Hash)]
pub struct JvmOutput {
    /// jar with the compiled classes, None for Android apps
    pub classes: Option<PathBuf>,
    /// Maven libraries and platforms (BOMs) the module depends on, consumers get them as well
    pub libraries: Vec<MavenCoordinate>,
    pub platforms: Vec<MavenCoordinate>,
    /// set for Android libraries
    pub android: Option<AndroidResources>,
    /// None if it cannot be started
    pub launch: Option<JvmLaunch>,
    /// of the shared C and C++ libraries below, java.library.path of `run` and the tests
    pub native_library_dirs: Vec<PathBuf>,
}

/// How `build run` starts a JVM module
#[derive(Debug, Hash)]
pub enum JvmLaunch {
    /// `java -jar` with the JDK the module was built with
    Jar { java: PathBuf },
    /// installed on a connected device or emulator, then its launcher activity is started
    Apk {
        sdk_root: PathBuf,
        application_id: String,
        /// full class name, None if the manifest has no launcher activity
        activity: Option<String>,
        /// lowest API level an emulator for the app may have
        min_sdk: i64,
        /// release apks without a key in the script are not, Android does not install those
        signed: bool,
    },
}

/// Returns the exit code. Jars get the arguments and stdin.
pub fn launch(module: &BuiltModule, output: &JvmOutput, args: &[String]) -> Result<i32, String> {
    match &output.launch {
        None => {
            return Err(format!(
                "{} cannot be run, only modules with a main_class and Android apps can",
                module.name
            ));
        }
        Some(JvmLaunch::Jar { java }) => {
            let mut command = Command::new(java);

            if !output.native_library_dirs.is_empty() {
                command.arg(format!("-Djava.library.path={}", jdk::join_classpath(&output.native_library_dirs)?.to_string_lossy()));
            }

            let status = command.arg("-jar")
                .arg(&module.artifact)
                .args(args)
                .current_dir(&module.directory)
                .status()
                .map_err(|error| format!("could not start {}: {error}", java.display()))?;

            // killed by a signal: like a shell, report failure
            return Ok(status.code().unwrap_or(1));
        }
        Some(JvmLaunch::Apk {
            sdk_root,
            application_id,
            activity,
            min_sdk,
            signed,
        }) => {
            if !signed {
                return Err(format!(
                    "{} is not signed, Android only installs signed apps: give build_android a keystore",
                    module.artifact.display()
                ));
            }

            if !args.is_empty() {
                println!("Note: arguments are ignored for Android apps");
            }

            android::install_and_start(
                &module.artifact,
                sdk_root,
                application_id,
                activity.as_deref(),
                *min_sdk,
            )?;

            return Ok(0);
        }
    }
}

impl BuiltModule {
    pub fn jvm(&self) -> &JvmOutput {
        match &self.output {
            ModuleOutput::Jvm(output) => return output,
            // the JVM builds take no other modules, see Dependencies::read
            ModuleOutput::Native(_) => unreachable!("{} is a C or C++ module", self.name),
        }
    }
}

/// Repositories and platforms, JDKs and generated sources, for every JVM platform
fn shared_functions() -> Vec<(&'static str, Value)> {
    return [maven::builtins(), jdk::builtins(), generator::builtins()].concat();
}
