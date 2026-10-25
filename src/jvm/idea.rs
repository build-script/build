// IntelliJ: a module writes its module file into .idea/modules once its dependencies are resolved,
// .idea/modules.xml lists them. `build-script idea` sets it up, every build after keeps it current.
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use quick_xml::escape::escape;

use crate::common::system;
use crate::language::runtime::{Arguments, BuiltModule};

use super::jdk::Jdk;
use super::ksp::SymbolProcessors;
use super::maven::TargetPlatform;
use super::module::{
    ANDROID_JVM_TARGET, Dependencies, ModuleBuildInputs, ResolvedDependencies, Sources,
};
use super::testing::resolve_test_libraries;

/// Relative to the script: build-script keeps the module files current
pub const MARKER: &str = ".idea/build-script";
pub const MARKER_TEXT: &str = "build-script keeps modules.xml and modules/ current when it builds\n";

/// For the script and the files next to it, when no module lives in the script's directory
const ROOT_MODULE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<module type="JAVA_MODULE" version="4">
  <component name="NewModuleRootManager" inherit-compiler-output="true">
    <exclude-output />
    <content url="file://$MODULE_DIR$/../.." />
    <orderEntry type="sourceFolder" forTests="false" />
  </component>
</module>
"#;

/// What a module file is written from
pub struct IdeaModule<'a> {
    pub inputs: &'a ModuleBuildInputs,
    pub resolved: &'a ResolvedDependencies,
    pub platform: TargetPlatform,
    /// of Android modules the jars of the aars, android.jar among compile_only
    pub libraries: &'a [PathBuf],
    pub compile_only: &'a [PathBuf],
    /// besides the sources of KSP and the javac processors, like the R classes
    pub generated_dirs: &'a [PathBuf],
}

/// .idea/modules/<name>.iml
pub fn module_file(project_dir: &Path, name: &str) -> PathBuf {
    return project_dir.join(".idea/modules").join(format!("{name}.iml"));
}

/// The module file of the debug build, only when it changed: IntelliJ reloads every file that changes
pub fn write_module(project_dir: &Path, args: &Arguments, module: &IdeaModule) -> Result<(), String> {
    let inputs = module.inputs;
    let android = module.platform == TargetPlatform::Android;
    let build_dir = inputs.directory.join("build");
    let url = |path: &Path| escape(&format!("file://{}", idea_path(project_dir, path))).into_owned();

    // where KSP and the javac processors write their sources
    let processors = |variant: &str| {
        let ksp = build_dir.join("generated/ksp").join(variant);
        let javac = build_dir.join("generated/sources/annotationProcessor/java");
        return vec![ksp.join("kotlin"), ksp.join("java"), javac.join(variant)];
    };

    let mut generated_dirs = module.generated_dirs.to_vec();
    generated_dirs.extend(processors(if android { "debug" } else { "main" }));

    let mut test_dirs = args.get::<Vec<String>>("test_source_dirs")?;

    if android {
        test_dirs.extend(args.get::<Vec<String>>("android_test_source_dirs")?);
    }

    let test_dirs: Vec<PathBuf> = test_dirs.iter()
        .map(|dir| inputs.directory.join(dir))
        .collect();

    let folders = [
        (&inputs.source_dirs, r#"isTestSource="false""#),
        (&generated_dirs, r#"isTestSource="false" generated="true""#),
        (&test_dirs, r#"isTestSource="true""#),
        (&processors("test"), r#"isTestSource="true" generated="true""#),
        (&inputs.resources.dirs, r#"type="java-resource""#),
        (&vec![inputs.directory.join("src/test/resources")], r#"type="java-test-resource""#),
    ];

    let mut content = String::new();

    for (dirs, attributes) in folders {
        for dir in dirs {
            content += &format!("      <sourceFolder url=\"{}\" {attributes} />\n", url(dir));
        }
    }

    let mut entries = String::new();

    for dependency in &module.resolved.modules {
        let name = escape(&dependency.name);
        entries += &format!("    <orderEntry type=\"module\" module-name=\"{name}\" />\n");
    }

    let test_libraries = test_libraries(inputs, args, module.platform, module.libraries)?;

    let libraries = [
        (module.libraries, ""),
        (module.compile_only, r#" scope="PROVIDED""#),
        (test_libraries.as_slice(), r#" scope="TEST""#),
    ];

    for (jars, scope) in libraries {
        for jar in jars {
            let jar = escape(&idea_path(project_dir, jar)).into_owned();
            entries += &format!(
                r#"    <orderEntry type="module-library"{scope}>
      <library>
        <CLASSES>
          <root url="jar://{jar}!/" />
        </CLASSES>
        <JAVADOC />
        <SOURCES />
      </library>
    </orderEntry>
"#
            );
        }
    }

    let jvm_target = match android {
        true => ANDROID_JVM_TARGET.to_string(),
        false => inputs.jdk.jvm_target(),
    };

    let facet = match inputs.sources {
        Sources::KotlinAndJava => kotlin_facet(project_dir, module, &jvm_target),
        Sources::Java => String::new(),
    };

    let file = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<module type="JAVA_MODULE" version="4">
  <component name="NewModuleRootManager" LANGUAGE_LEVEL="JDK_{level}" inherit-compiler-output="false">
    <output url="{output}" />
    <output-test url="{output_test}" />
    <exclude-output />
    <content url="{directory}">
{content}      <excludeFolder url="{build}" />
    </content>
    <orderEntry type="jdk" jdkName="{jdk}" jdkType="JavaSDK" />
    <orderEntry type="sourceFolder" forTests="false" />
{entries}  </component>
{facet}</module>
"#,
        // JDK_21, JDK_1_8
        level = jvm_target.replace('.', "_"),
        // apart from build-script's classes, when IntelliJ compiles itself
        output = url(&build_dir.join("idea/main")),
        output_test = url(&build_dir.join("idea/test")),
        directory = url(&inputs.directory),
        build = url(&build_dir),
        jdk = escape(&jdk_name(&inputs.jdk)),
    );

    return write_if_changed(&module_file(project_dir, &inputs.name), &file);
}

/// IntelliJ's Kotlin plugin takes the compiler plugins from the facet, like Compose
fn kotlin_facet(project_dir: &Path, module: &IdeaModule, jvm_target: &str) -> String {
    // 2.4 of 2.4.21
    let version: Vec<&str> = module.inputs.kotlin_version.splitn(3, '.')
        .take(2)
        .collect();
    let version = version.join(".");
    let mut plugins = String::new();

    if !module.resolved.plugins.is_empty() {
        let args = |values: Vec<String>| -> String {
            return values.iter()
                .map(|value| format!("                <arg>{}</arg>\n", escape(value)))
                .collect();
        };

        let classpath = module.resolved.plugins.iter().map(|jar| idea_path(project_dir, jar));

        plugins = format!(
            r#"          <arrayArguments>
            <arrayArg name="pluginClasspaths">
              <args>
{}              </args>
            </arrayArg>
            <arrayArg name="pluginOptions">
              <args>
{}              </args>
            </arrayArg>
          </arrayArguments>
"#,
            args(classpath.collect()),
            args(module.resolved.plugin_options.clone()),
        );
    }

    return format!(
        r#"  <component name="FacetManager">
    <facet type="kotlin-language" name="Kotlin">
      <configuration version="5" platform="JVM {jvm_target}" allPlatforms="JVM [{jvm_target}]" useProjectSettings="false">
        <compilerArguments>
          <stringArguments>
            <stringArg name="jvmTarget" arg="{jvm_target}" />
            <stringArg name="apiVersion" arg="{version}" />
            <stringArg name="languageVersion" arg="{version}" />
          </stringArguments>
{plugins}        </compilerArguments>
      </configuration>
    </facet>
  </component>
"#
    );
}

/// .idea/modules.xml with every module file. After a build that worked, the script's directory
/// gets a module when no module lives there, and modules gone from the script lose their file.
pub fn write_project(project_dir: &Path, built: Option<&[Arc<BuiltModule>]>) -> Result<(), String> {
    let modules_dir = project_dir.join(".idea/modules");

    if let Some(built) = built {
        let mut names: Vec<String> = built.iter()
            .map(|module| module.name.clone())
            .collect();
        let project = absolute(project_dir);
        let root = project.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        if !built.iter().any(|module| absolute(&module.directory) == project) && !names.contains(&root) {
            write_if_changed(&module_file(project_dir, &root), ROOT_MODULE)?;
            names.push(root);
        }

        for file in system::files_with_extension(&[modules_dir.clone()], "iml") {
            if !names.iter().any(|name| file.file_stem() == Some(name.as_ref())) {
                fs::remove_file(&file)
                    .map_err(|error| format!("could not delete {}: {error}", file.display()))?;
            }
        }
    }

    let mut modules = String::new();

    for file in system::files_with_extension(&[modules_dir], "iml") {
        let name = escape(file.file_name().unwrap_or_default().to_string_lossy()).into_owned();
        let path = format!("$PROJECT_DIR$/.idea/modules/{name}");
        modules += &format!("      <module fileurl=\"file://{path}\" filepath=\"{path}\" />\n");
    }

    let file = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="ProjectModuleManager">
    <modules>
{modules}    </modules>
  </component>
</project>
"#
    );

    return write_if_changed(&project_dir.join(".idea/modules.xml"), &file);
}

/// What the tests need besides the module's libraries, resolved like for running them:
/// test_dependencies and, of Android modules, android_test_dependencies
fn test_libraries(
    inputs: &ModuleBuildInputs,
    args: &Arguments,
    platform: TargetPlatform,
    libraries: &[PathBuf],
) -> Result<Vec<PathBuf>, String> {
    let mut lists = vec![("test_dependencies", args.get::<Option<String>>("test_main_class")?.is_none())];

    // instrumented tests run on the device, not on the JUnit Platform
    if platform == TargetPlatform::Android {
        lists.push(("android_test_dependencies", false));
    }

    let mut files: Vec<PathBuf> = vec![];

    for (name, on_junit_platform) in lists {
        let dependencies = Dependencies::read(args, name, &inputs.kotlin_version)?;

        // without tests the JUnit Platform needs nothing
        if dependencies.is_empty() {
            continue;
        }

        let resolved = resolve_test_libraries(
            inputs,
            &dependencies,
            &SymbolProcessors::none(),
            &Dependencies::default(),
            on_junit_platform,
            platform,
        )?;

        for file in resolved.files {
            if !libraries.contains(&file) && !files.contains(&file) {
                files.push(file);
            }
        }
    }

    return Ok(files);
}

/// IntelliJ keeps its JDKs by name, outside the project. A JDK build-script installed is
/// named like IntelliJ names it, temurin-21, any other jdk-21.
fn jdk_name(jdk: &Jdk) -> String {
    let installed = jdk.home.strip_prefix(system::data_dir("jdks"))
        .ok()
        .and_then(|rest| rest.components().next())
        .map(|dir| dir.as_os_str().to_string_lossy().to_string());

    match installed {
        // <distribution>-<version>-<os>-<arch>
        Some(dir) => return dir.rsplitn(3, '-').last().unwrap_or(&dir).to_string(),
        None => return format!("jdk-{}", jdk.jvm_target()),
    }
}

/// A path like IntelliJ writes it into module files, so it has no reason to write it back:
/// below the project relative to .idea/modules, below the home directory with $USER_HOME$
fn idea_path(project_dir: &Path, path: &Path) -> String {
    let path = absolute(path);
    let home = std::env::var_os("HOME").map(|home| absolute(Path::new(&home)));
    let in_home = home.as_ref().and_then(|home| path.strip_prefix(home).ok());

    let (base, rest) = match (path.strip_prefix(absolute(project_dir)), in_home) {
        (Ok(rest), _) => ("$MODULE_DIR$/../..", rest),
        (_, Some(rest)) => ("$USER_HOME$", rest),
        _ => return path.display().to_string(),
    };

    if rest.as_os_str().is_empty() {
        return base.to_string();
    }

    return format!("{base}/{}", rest.display());
}

/// Absolute and without . and .., the files need not exist
fn absolute(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in std::env::current_dir().unwrap_or_default().join(path).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other),
        }
    }

    return normalized;
}

fn write_if_changed(file: &Path, content: &str) -> Result<(), String> {
    if fs::read_to_string(file).is_ok_and(|existing| existing == content) {
        return Ok(());
    }

    return system::write_file(file, content);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jvm::JvmOutput;
    use crate::language::runtime::ModuleOutput;

    #[test]
    fn paths_like_intellij_writes_them() {
        let project = Path::new("/work/shop");
        let home = PathBuf::from(std::env::var("HOME").unwrap());

        assert_eq!(idea_path(project, Path::new("/work/shop/app/src")), "$MODULE_DIR$/../../app/src");
        assert_eq!(idea_path(project, Path::new("/work/shop/./app/../core")), "$MODULE_DIR$/../../core");
        assert_eq!(idea_path(project, project), "$MODULE_DIR$/../..");
        assert_eq!(idea_path(project, &home.join(".build-script/a.jar")), "$USER_HOME$/.build-script/a.jar");
        assert_eq!(idea_path(project, Path::new("/opt/jdk")), "/opt/jdk");
    }

    #[test]
    fn modules_gone_from_the_script_lose_their_file_after_a_build_that_worked() {
        let project = std::env::temp_dir().join(format!("build-script-idea-{}", std::process::id()));
        let _ = fs::remove_dir_all(&project);

        let modules_dir = project.join(".idea/modules");
        system::write_file(&modules_dir.join("gone.iml"), "<module/>").unwrap();
        system::write_file(&modules_dir.join("app.iml"), "<module/>").unwrap();

        // a failed build keeps every file
        write_project(&project, None).unwrap();
        let modules_xml = system::read_file(&project.join(".idea/modules.xml")).unwrap();
        assert!(modules_xml.contains("$PROJECT_DIR$/.idea/modules/gone.iml"));

        let app = Arc::new(BuiltModule {
            name: "app".to_string(),
            directory: project.join("app"),
            artifact: project.join("app/build/libs/app.jar"),
            modules: vec![],
            output: ModuleOutput::Jvm(JvmOutput {
                classes: None,
                libraries: vec![],
                platforms: vec![],
                android: None,
                launch: None,
            }),
            tests: vec![],
        });

        write_project(&project, Some(&[app])).unwrap();

        let root = project.file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(!modules_dir.join("gone.iml").exists());
        assert!(modules_dir.join("app.iml").is_file());
        // the script lives outside the modules
        assert!(modules_dir.join(format!("{root}.iml")).is_file());

        fs::remove_dir_all(&project).unwrap();
    }
}
