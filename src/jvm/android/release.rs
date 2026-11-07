// Release builds: R8 shrinks, optimizes and renames the code with the ProGuard rules of build-script,
// of aapt2, of the libraries and of the app, then dexes it. The apk and the app bundle are signed
// with the key of the script.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::common::system;
use crate::jvm::maven::{DependencyResolver, MavenCoordinate, TargetPlatform};
use crate::language::runtime::Arguments;

use super::build::{AndroidBuild, DesugaredLibrary, Signature, unique};

const BUNDLETOOL_VERSION: &str = "1.18.3";
const BUNDLETOOL_MAIN_CLASS: &str = "com.android.tools.build.bundletool.BundleToolMain";

/// What every app needs kept, like proguard-android-optimize.txt of the Android Gradle plugin
const DEFAULT_RULES: &str = "\
-allowaccessmodification
-keepattributes *Annotation*,Signature,InnerClasses,EnclosingMethod
# readable stack traces with mapping.txt
-keepattributes SourceFile,LineNumberTable
-renamesourcefileattribute SourceFile
-keepclasseswithmembernames,includedescriptorclasses class * {
    native <methods>;
}
-keepclassmembers enum * {
    public static **[] values();
    public static ** valueOf(java.lang.String);
}
-keepclassmembers class * implements android.os.Parcelable {
    public static final ** CREATOR;
}
-keepclassmembers class **.R$* {
    public static <fields>;
}
# android:onClick in layouts
-keepclassmembers class * extends android.app.Activity {
    public void *(android.view.View);
}
-keep @androidx.annotation.Keep class * { *; }
-keepclasseswithmembers class * {
    @androidx.annotation.Keep <methods>;
}
-keepclasseswithmembers class * {
    @androidx.annotation.Keep <fields>;
}
-keepclasseswithmembers class * {
    @androidx.annotation.Keep <init>(...);
}
-dontwarn android.support.**
# window extensions and sidecar come with the device, like the platform
-dontwarn androidx.window.extensions.**
-dontwarn androidx.window.sidecar.**
";

/// The key of release apks, from keystore, key_alias, keystore_password_env and key_password_env
#[derive(Debug, Clone)]
pub struct ReleaseKey {
    pub keystore: PathBuf,
    pub alias: String,
    /// environment variables with the passwords, the script only names them
    pub keystore_password_env: String,
    pub key_password_env: String,
}

impl ReleaseKey {
    /// None without a keystore. The keystore is relative to the script.
    pub fn read(args: &Arguments, project_dir: &Path) -> Result<Option<Self>, String> {
        let Some(keystore) = args.get::<Option<String>>("keystore")? else {
            return Ok(None);
        };

        let alias: Option<String> = args.get("key_alias")?;
        let keystore_password_env: Option<String> = args.get("keystore_password_env")?;

        let (Some(alias), Some(keystore_password_env)) = (alias, keystore_password_env) else {
            return Err("a keystore needs key_alias and keystore_password_env".to_string());
        };

        // the key often has the password of the keystore
        let key_password_env = args.get::<Option<String>>("key_password_env")?
            .unwrap_or_else(|| keystore_password_env.clone());

        for name in [&keystore_password_env, &key_password_env] {
            // a password given by mistake must not end up in the error
            let is_name = name.chars().all(|char| char.is_ascii_alphanumeric() || char == '_')
                && !name.starts_with(|char: char| char.is_ascii_digit());

            if !is_name {
                return Err(
                    "keystore_password_env and key_password_env take names of environment variables, not passwords"
                        .to_string(),
                );
            }

            if std::env::var_os(name).is_none() {
                return Err(format!(
                    "the password of the release key comes from {name}, which is not set"
                ));
            }
        }

        let keystore = project_dir.join(keystore);

        if !keystore.is_file() {
            return Err(format!("the keystore {} does not exist", keystore.display()));
        }

        return Ok(Some(ReleaseKey {
            keystore,
            alias,
            keystore_password_env,
            key_password_env,
        }));
    }
}

/// Every ProGuard rule file of a release build: build-script's, those aapt2 wrote for
/// the manifest and the layouts, those of the aars, library modules and jars, those
/// for the services of `java_resources`, then the app's own
pub fn proguard_rule_files(
    build: &AndroidBuild,
    app_rules: &[PathBuf],
    aapt2_rules: &Path,
    java_resources: &Path,
    work_dir: &Path,
) -> Result<Vec<PathBuf>, String> {
    system::recreate_dir(&work_dir.join("libraries"))?;

    let default_rules = work_dir.join("default.pro");
    system::write_file(&default_rules, DEFAULT_RULES)?;

    let service_rules = work_dir.join("services.pro");
    system::write_file(&service_rules, &keep_rules_for_services(java_resources)?)?;

    let mut rules = vec![default_rules, aapt2_rules.to_path_buf()];
    rules.extend(build.library_proguard_rules());

    for jar in unique(&build.jars) {
        for entry in rule_entries_in_jar(&jar)? {
            let file = work_dir.join("libraries")
                .join(format!("{}-{}", rules.len(), entry.replace('/', "-")));

            let content = system::read_zip_entry(&jar, &entry)?;
            system::write_file(&file, &String::from_utf8_lossy(&content))?;
            rules.push(file);
        }
    }

    rules.push(service_rules);
    rules.extend(app_rules.iter().cloned());

    return Ok(rules);
}

/// The rule files a library keeps in its jar: those for R8 in META-INF/com.android.tools,
/// else those for ProGuard in META-INF/proguard
fn rule_entries_in_jar(jar: &Path) -> Result<Vec<String>, String> {
    let entries = system::zip_entries(jar)?;
    let files = entries.iter().filter(|entry| !entry.ends_with('/'));

    let has_r8_rules = entries.iter()
        .any(|entry| entry.starts_with("META-INF/com.android.tools/"));

    if !has_r8_rules {
        return Ok(files.filter(|entry| entry.starts_with("META-INF/proguard/"))
            .cloned()
            .collect());
    }

    // r8-from-<version> and r8-upto-<version> name the R8 versions the rules are for;
    // the R8 of the build tools is newer than every from and than no upto
    return Ok(files.filter(|entry| {
        let directory = entry.strip_prefix("META-INF/com.android.tools/")
            .and_then(|rest| rest.split('/').next())
            .unwrap_or("");

        directory == "r8" || (directory.starts_with("r8-from-") && !directory.contains("upto"))
    })
        .cloned()
        .collect());
}

/// The classes the service files name keep their names: the Java resources are
/// packaged as they are, not by R8
fn keep_rules_for_services(java_resources: &Path) -> Result<String, String> {
    let mut rules = String::new();

    for file in system::files_below(&[java_resources.join("META-INF/services")]) {
        let service = file.file_name()
            .unwrap_or_default()
            .to_string_lossy();
        rules.push_str(&format!("-keepnames class {service}\n"));

        for line in system::read_file(&file)?.lines() {
            let provider = line.split('#')
                .next()
                .unwrap_or("")
                .trim();

            if !provider.is_empty() {
                rules.push_str(&format!("-keep class {provider} {{ <init>(); }}\n"));
            }
        }
    }

    return Ok(rules);
}

/// R8 over the own classes and every library into `output`, with build/outputs/mapping/release.
/// Returns the keep rules for L8 when the app uses the desugared library.
pub fn shrink_with_r8(
    build: &AndroidBuild,
    own_jars: &[PathBuf],
    rules: &[PathBuf],
    main_dex_rules: Option<&Path>,
    desugared: Option<&DesugaredLibrary>,
    output: &Path,
) -> Result<Option<PathBuf>, String> {
    let mapping_dir = build.build_dir.join("outputs/mapping/release");
    system::recreate_dir(&mapping_dir)?;
    system::recreate_dir(output)?;

    let mut r8 = build.r8_command("R8", output)?;
    let command = &mut r8.command;

    command.arg("--release")
        // the Java resources are packaged apart, as they are
        .arg("--no-data-resources")
        .arg("--pg-map-output")
        .arg(mapping_dir.join("mapping.txt"))
        .arg("--pg-conf-output")
        .arg(mapping_dir.join("configuration.txt"));

    for rules_file in rules {
        command.arg("--pg-conf").arg(rules_file);
    }

    if let Some(main_dex_rules) = main_dex_rules {
        command.arg("--main-dex-rules").arg(main_dex_rules);
    }

    let mut library_keep_rules = None;

    if let Some(desugared) = desugared {
        let keep_rules = build.build_dir.join("intermediates/desugar/keep-rules.pro");
        command.arg("--desugared-lib")
            .arg(&desugared.config)
            .arg("--desugared-lib-pg-conf-output")
            .arg(&keep_rules);
        library_keep_rules = Some(keep_rules);
    }

    for jar in &build.compile_only {
        command.arg("--classpath").arg(jar);
    }

    command.args(unique(own_jars.iter().chain(&build.jars)));
    r8.run(|command| build.run_dex_tool(command, "R8"))?;

    system::log(
        &build.inputs.name,
        &format!("Mapping of the renamed classes in {}", mapping_dir.join("mapping.txt").display()),
    );

    return Ok(library_keep_rules);
}

/// The app bundle for Google Play with one module, base: the resources linked in protobuf
/// format, the dex files, the native libraries and the Java resources. Signed with jarsigner
/// like by the Gradle plugin.
pub fn build_app_bundle(
    build: &AndroidBuild,
    proto_resources: &Path,
    dex_dir: &Path,
    java_resources: &Path,
    native_libraries: &Path,
    mapping: Option<&Path>,
    signature: &Signature,
    bundle: &Path,
) -> Result<(), String> {
    let work_dir = build.build_dir.join("intermediates/bundle");
    let module = work_dir.join("base");
    system::recreate_dir(&module)?;
    system::extract(proto_resources, &module)?;

    // bundletool wants the manifest in manifest/, the dex files in dex/, the Java resources in root/
    system::create_dir(&module.join("manifest"))?;
    fs::rename(
        module.join("AndroidManifest.xml"),
        module.join("manifest/AndroidManifest.xml"),
    )
    .map_err(|error| format!("could not move the manifest of the app bundle: {error}"))?;

    for dex in system::files_with_extension(&[dex_dir.to_path_buf()], "dex") {
        let name = dex.file_name().unwrap_or_default();
        system::copy_file(&dex, &module.join("dex").join(name))?;
    }

    system::copy_dir_contents(native_libraries, &module)?;
    system::copy_dir_contents(java_resources, &module.join("root"))?;

    // add_to_zip would add to the one of the last build
    let module_zip = work_dir.join("base.zip");
    let _ = fs::remove_file(&module_zip);
    system::add_to_zip(&module_zip, &system::relative_files(&module), false)?;

    let mut resolver =
        DependencyResolver::new(build.inputs.repositories.clone(), TargetPlatform::Jvm);
    let bundletool = resolver.resolve_with_platforms(
        &[MavenCoordinate::new(
            "com.android.tools.build",
            "bundletool",
            BUNDLETOOL_VERSION,
        )],
        &[],
    )?;

    let unsigned = work_dir.join("unsigned.aab");

    let mut build_bundle = build.inputs.jdk.tool_command("bundletool", &bundletool, BUNDLETOOL_MAIN_CLASS)?;
    let command = &mut build_bundle.command;
    command.arg("build-bundle")
        .arg(format!("--modules={}", module_zip.display()))
        .arg(format!("--output={}", unsigned.display()))
        .arg("--overwrite");

    // Google Play reads the mapping from the bundle and shows readable stack traces
    if let Some(mapping) = mapping {
        command.arg(format!(
            "--metadata-file=com.android.tools.build.obfuscation/proguard.map:{}",
            mapping.display()
        ));
    }

    build_bundle.run(|command| system::run_command_quietly(command, "bundletool build-bundle"))?;

    let mut validate = build.inputs.jdk.tool_command("bundletool", &bundletool, BUNDLETOOL_MAIN_CLASS)?;
    validate.command
        .arg("validate")
        .arg(format!("--bundle={}", unsigned.display()));
    validate.run(|command| system::run_command_quietly(command, "bundletool validate"))?;

    system::create_parent_dir(bundle)?;

    let Signature::Release(key) = signature else {
        return system::copy_file(&unsigned, bundle);
    };

    // jarsigner reads the passwords from the environment, they stay out of the arguments
    let mut sign = Command::new(build.inputs.jdk.tool_path("jarsigner"));
    sign.arg("-keystore")
        .arg(&key.keystore)
        .arg("-storepass:env")
        .arg(&key.keystore_password_env)
        .arg("-keypass:env")
        .arg(&key.key_password_env)
        .arg("-signedjar")
        .arg(bundle)
        .arg(&unsigned)
        .arg(&key.alias);

    return system::run_command_quietly(&mut sign, "jarsigner");
}

#[cfg(test)]
mod tests {

    use crate::common::test_support::ScratchDir;

    use super::*;

    #[test]
    fn classes_of_service_files_keep_their_names() {
        let directory = ScratchDir::new("services-rules");

        system::write_file(
            &directory.join("META-INF/services/com.example.Plugin"),
            "# providers\ncom.example.First\ncom.example.Second # the other one\n",
        )
        .unwrap();

        assert_eq!(
            keep_rules_for_services(&directory).unwrap(),
            "-keepnames class com.example.Plugin\n\
             -keep class com.example.First { <init>(); }\n\
             -keep class com.example.Second { <init>(); }\n"
        );

    }
}
