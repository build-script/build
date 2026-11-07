// Android builds: resources with aapt2, Kotlin and Java against android.jar, native libraries with CMake, dex with d8, a signed apk
use std::collections::HashSet;
use std::hash::Hash;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::common::xml::{self, XmlElement};
use crate::common::{system, up_to_date};
use crate::jvm::idea::{self, IdeaModule};
use crate::jvm::jdk::{Jdk, ToolCommand};
use crate::jvm::maven::{DependencyResolver, MavenCoordinate, TargetPlatform};
use crate::jvm::module::{
    self, Compilation, ModuleBuildInputs, ResolvedDependencies, Sources, create_jar,
};
use crate::language::runtime::{Arguments, BuildScriptContext, BuiltModule, ModuleOutput};
use crate::native;

use super::generated::{
    SymbolTable, binding_class, build_config_java_fields, layouts_by_name, view_binding_source,
    write_r_class,
};
use super::manifest::{
    expand_class_names, merge_manifest, move_aliases_last, read_manifest_with_placeholders,
    remove_entries_marked_for_removal,
};
use super::release::ReleaseKey;
use super::sdk::{self, AndroidSdk};

pub const DEFAULT_NDK_VERSION: &str = "28.2.13676358";

/// from this API level on Android loads several dex files natively
const NATIVE_MULTIDEX_API: i64 = 21;
/// runtime of generated view bindings, which implement its ViewBinding interface
const VIEW_BINDING_VERSION: &str = "8.1.0";
/// the CMake of the SDK the Android Gradle plugin uses by default
const CMAKE_VERSION: &str = "3.22.1";
/// what the Android Gradle plugin builds without abiFilters
const ABIS: &[&str] = &["armeabi-v7a", "arm64-v8a", "x86", "x86_64"];
/// the CMake builds, and the libraries in libs/<abi> like jniLibs
const CMAKE_DIR: &str = "intermediates/cmake";
/// of lib/apksigner.jar in the build tools
const APKSIGNER_MAIN_CLASS: &str = "com.android.apksigner.ApkSignerTool";

/// Debug, or release with `build --release`
#[derive(Clone, Copy, PartialEq)]
pub enum BuildType {
    Debug,
    Release,
}

impl BuildType {
    pub fn of(context: &BuildScriptContext) -> Self {
        return match context.release {
            true => BuildType::Release,
            false => BuildType::Debug,
        };
    }

    /// Of the source set over src/main and of the output directories
    pub fn name(self) -> &'static str {
        return match self {
            BuildType::Debug => "debug",
            BuildType::Release => "release",
        };
    }
}

/// What the apk and the app bundle are signed with
pub enum Signature<'a> {
    /// the debug key every Android tool uses
    Debug,
    Release(&'a ReleaseKey),
    /// a release build without a key in the script
    Unsigned,
}

/// What aapt2 links: the app, or a library with or without the resources of its libraries
#[derive(Clone, Copy, PartialEq)]
pub enum LinkedResources {
    App,
    LibraryWithItsLibraries,
    LibraryAlone,
}

/// What app and library builds share up to compiling Kotlin
pub struct AndroidBuild {
    pub inputs: ModuleBuildInputs,
    pub namespace: String,
    pub min_sdk: i64,
    pub compile_sdk: i64,
    pub sdk: AndroidSdk,
    pub main_dir: PathBuf,
    pub build_type: BuildType,
    /// debug, release, or debugAndroidTest for the module with its instrumented tests
    variant: &'static str,
    pub build_dir: PathBuf,
    /// own res directories that contain files, lowest priority first
    pub res_dirs: Vec<PathBuf>,
    /// own assets directories that contain files, lowest priority first
    asset_dirs: Vec<PathBuf>,
    aidl_dirs: Vec<PathBuf>,
    /// aidl/ directories of aars, for imports of their parcelables
    aidl_imports: Vec<PathBuf>,
    pub view_binding: bool,
    /// like android.nonTransitiveRClass: R classes have only the package's own resources
    non_transitive_r: bool,
    /// ${name} in manifests, ${applicationId} is always there
    manifest_placeholders: Vec<(String, String)>,
    /// build_config_fields: "Type NAME value"
    build_config_fields: Vec<String>,
    /// compiler, plugins, processors and the libraries
    resolved: ResolvedDependencies,
    /// to compile against and to dex: Maven jars, classes of aars and library modules
    pub jars: Vec<PathBuf>,
    /// of `jars` the classes of library modules, they change with the app
    module_jars: Vec<PathBuf>,
    /// only to compile against
    pub compile_only: Vec<PathBuf>,
    /// aars and library modules with resources, lowest priority first
    libraries: Vec<AndroidResourceLibrary>,
    /// jniLibs directories and the libraries built with CMake
    native_library_dirs: Vec<PathBuf>,
    cmake_lists: Option<PathBuf>,
    ndk_version: String,
    accept_license: bool,
}

/// desugar_jdk_libs: its d8 configuration and its jars
pub struct DesugaredLibrary {
    pub config: PathBuf,
    jars: Vec<PathBuf>,
}

/// Like with the Gradle plugin, view bindings come with their runtime, which
/// consumers of a library need as well
pub fn read_android_inputs(
    context: &BuildScriptContext,
    args: &Arguments,
) -> Result<ModuleBuildInputs, String> {
    let mut inputs = ModuleBuildInputs::read(context, args, Sources::KotlinAndJava)?;
    let build_type = BuildType::of(context);

    inputs.source_dirs = inputs.source_dirs.iter()
        .map(|dir| dir_of_build_type(&inputs.directory, dir, build_type))
        .collect();

    inputs.resources.dirs = inputs.resources.dirs.iter()
        .map(|dir| dir_of_build_type(&inputs.directory, dir, build_type))
        .collect();

    if args.get::<bool>("view_binding")? {
        inputs.dependencies.libraries.push(MavenCoordinate::new(
            "androidx.databinding",
            "viewbinding",
            VIEW_BINDING_VERSION,
        ));
    }

    return Ok(inputs);
}

/// The module's directories of the list argument `name` that contain files, lowest priority first
pub fn existing_dirs(
    inputs: &ModuleBuildInputs,
    args: &Arguments,
    name: &str,
    build_type: BuildType,
) -> Result<Vec<PathBuf>, String> {
    let dirs: Vec<String> = args.get(name)?;
    return Ok(dirs.iter()
        .map(|dir| dir_of_build_type(&inputs.directory, dir, build_type))
        .filter(|dir| system::contains_files(dir))
        .collect());
}

/// `dir` of the module. The defaults name src/debug, a release build takes src/release instead.
fn dir_of_build_type(module_dir: &Path, dir: impl AsRef<Path>, build_type: BuildType) -> PathBuf {
    let dir = module_dir.join(dir);

    if build_type == BuildType::Release {
        if let Ok(rest) = dir.strip_prefix(module_dir.join("src/debug")) {
            return module_dir.join("src/release").join(rest);
        }
    }

    return dir;
}

/// The module's jniLibs, with a CMakeLists.txt the libraries built from it, and the shared libraries of
/// build_c and build_cpp, built here for every ABI and the module's min_sdk
pub fn native_library_dirs(
    inputs: &ModuleBuildInputs,
    args: &Arguments,
    build_type: BuildType,
) -> Result<Vec<PathBuf>, String> {
    let mut dirs = source_set_dirs_with_files(inputs, "jniLibs", build_type);

    if args.get::<Option<String>>("cmake")?.is_some() {
        dirs.push(system::build_dir(&inputs.directory).join(CMAKE_DIR).join("libs"));
    }

    for module in &inputs.dependencies.native {
        let ModuleOutput::Native(output) = &module.output else { continue };

        let sources = output.android.as_ref().ok_or_else(|| {
            format!("{} has vcpkg ports or libraries of modules, which are built for this machine only", module.name)
        })?;

        let release = build_type == BuildType::Release;
        let ndk_version: String = args.get("ndk_version")?;
        dirs.push(native::build_for_android(sources, ABIS, args.get("min_sdk")?, &ndk_version, args.get("accept_sdk_license")?, release)?);
    }

    return Ok(dirs);
}

/// What the outputs of an Android module depend on besides the sources:
/// resources, AIDL, manifests, assets, native libraries, Java resources
pub fn android_input_paths(
    inputs: &ModuleBuildInputs,
    args: &Arguments,
    build_type: BuildType,
) -> Result<Vec<PathBuf>, String> {
    let mut paths: Vec<PathBuf> = ["res_dirs", "aidl_dirs", "asset_dirs"].iter()
        .map(|name| args.get::<Vec<String>>(name))
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .flatten()
        .map(|dir| dir_of_build_type(&inputs.directory, dir, build_type))
        .collect();

    for source_set in ["main", build_type.name()] {
        let source_set_dir = inputs.directory.join("src").join(source_set);
        paths.push(source_set_dir.join("AndroidManifest.xml"));
        paths.push(source_set_dir.join("jniLibs"));
    }

    paths.extend(inputs.resources.inputs());

    // native sources, where Android Studio keeps them
    if let Some(cmake) = args.get::<Option<String>>("cmake")? {
        paths.push(inputs.directory.join(cmake));
        paths.push(inputs.directory.join("src/main/cpp"));
    }

    return Ok(paths);
}

impl AndroidBuild {
    /// `native_library_dirs` as native_library_dirs() returns them, a library has them already
    pub fn prepare(
        inputs: ModuleBuildInputs,
        args: &Arguments,
        build_type: BuildType,
        native_library_dirs: Vec<PathBuf>,
    ) -> Result<Self, String> {
        let build_dir = system::build_dir(&inputs.directory);
        return Self::prepare_variant(inputs, args, build_type, build_type.name(), build_dir, native_library_dirs);
    }

    /// The module with its instrumented tests, built apart from the module: the
    /// resources and assets of src/androidTest lie over those of the module
    pub fn prepare_android_tests(
        inputs: ModuleBuildInputs,
        args: &Arguments,
    ) -> Result<Self, String> {
        let build_dir = system::build_dir(&inputs.directory).join("androidTest");
        let native_library_dirs = native_library_dirs(&inputs, args, BuildType::Debug)?;
        let mut build = Self::prepare_variant(
            inputs,
            args,
            BuildType::Debug,
            "debugAndroidTest",
            build_dir,
            native_library_dirs,
        )?;
        let test_dir = build.inputs.directory.join("src/androidTest");
        build.res_dirs.extend(
            [test_dir.join("res")].into_iter()
                .filter(|dir| system::contains_files(dir)),
        );
        build.asset_dirs.extend(
            [test_dir.join("assets")].into_iter()
                .filter(|dir| system::contains_files(dir)),
        );

        return Ok(build);
    }

    fn prepare_variant(
        inputs: ModuleBuildInputs,
        args: &Arguments,
        build_type: BuildType,
        variant: &'static str,
        build_dir: PathBuf,
        native_library_dirs: Vec<PathBuf>,
    ) -> Result<Self, String> {
        let compile_sdk: i64 = args.get("compile_sdk")?;
        let build_tools: String = args.get("build_tools")?;
        let accept_license: bool = args.get("accept_sdk_license")?;
        let build_config_fields: Vec<String> = args.get("build_config_fields")?;
        // checked before the build starts
        build_config_java_fields(&build_config_fields)?;

        let manifest_placeholders = args.assignments("manifest_placeholders")?
            .into_iter()
            .map(|(name, value)| (name, value.trim().to_string()))
            .collect();

        for module in BuiltModule::transitive(&inputs.dependencies.modules) {
            if module.jvm().classes.is_none() {
                return Err(format!(
                    "{} is an Android app and cannot be a dependency",
                    module.name
                ));
            }
        }

        let sdk = AndroidSdk::ensure(compile_sdk, &build_tools, accept_license)?;
        system::log(
            &inputs.name,
            &format!(
                "Android SDK {} (android-{compile_sdk}, build-tools {build_tools})",
                sdk.root.display(),
            ),
        );
        system::log(&inputs.name, &format!("JDK {}", inputs.jdk.home.display()));

        let resolved = inputs.resolve(TargetPlatform::Android)?;
        let unpacked = UnpackedLibraries::unpack(&resolved, &sdk, &build_dir)?;

        return Ok(AndroidBuild {
            namespace: args.get("namespace")?,
            min_sdk: args.get("min_sdk")?,
            compile_sdk,
            sdk,
            main_dir: inputs.directory.join("src/main"),
            build_type,
            variant,
            build_dir,
            res_dirs: existing_dirs(&inputs, args, "res_dirs", build_type)?,
            asset_dirs: existing_dirs(&inputs, args, "asset_dirs", build_type)?,
            aidl_dirs: args.get::<Vec<String>>("aidl_dirs")?
                .iter()
                .map(|dir| dir_of_build_type(&inputs.directory, dir, build_type))
                .collect(),
            aidl_imports: unpacked.aidl_imports,
            view_binding: args.get("view_binding")?,
            non_transitive_r: args.get("non_transitive_r_class")?,
            manifest_placeholders,
            build_config_fields,
            resolved,
            jars: unpacked.jars,
            module_jars: unpacked.module_jars,
            compile_only: unpacked.compile_only,
            libraries: unpacked.libraries,
            native_library_dirs,
            cmake_lists: args.get::<Option<String>>("cmake")?
                .map(|cmake| inputs.directory.join(cmake)),
            ndk_version: args.get("ndk_version")?,
            accept_license,
            inputs,
        });
    }

    pub fn compile_own_resources(&self) -> Result<Vec<PathBuf>, String> {
        let mut outputs = vec![];

        for (index, res_dir) in self.res_dirs.iter().enumerate() {
            let name = format!("{}-own-{index}", self.inputs.name);
            outputs.push(compiled_resources(
                &self.sdk,
                res_dir,
                &self.build_dir.join("intermediates/res"),
                &name,
            )?);
        }

        return Ok(outputs);
    }

    /// The manifest for aapt2 with its package, for apps merged with those of all
    /// libraries. `overlay` lies over it, like the manifest of instrumented tests.
    pub fn write_manifest(
        &self,
        source: Option<&Path>,
        overlay: Option<&XmlElement>,
        application_id: &str,
        merge: bool,
        output: &Path,
    ) -> Result<XmlElement, String> {
        let mut manifest = match source {
            Some(source) => read_manifest_with_placeholders(
                source,
                application_id,
                &self.manifest_placeholders,
            )?,
            None => xml::parse(
                r#"<manifest xmlns:android="http://schemas.android.com/apk/res/android"/>"#,
            )?,
        };

        // the source set of the build type wins over main
        let build_type_manifest = self
            .inputs
            .directory
            .join(format!("src/{}/AndroidManifest.xml", self.build_type.name()));

        if build_type_manifest.is_file() {
            let mut over_main = read_manifest_with_placeholders(
                &build_type_manifest,
                application_id,
                &self.manifest_placeholders,
            )?;

            for (key, value) in &manifest.attributes {
                if over_main.attribute(key).is_none() {
                    over_main.attributes.push((key.clone(), value.clone()));
                }
            }

            merge_manifest(&mut over_main, &manifest);
            manifest = over_main;
        }

        if let Some(overlay) = overlay {
            merge_manifest(&mut manifest, overlay);
        }

        match manifest.attribute("package") {
            Some(package) if package != self.namespace => {
                return Err(format!(
                    "the manifest declares package {package}, but the namespace is {}",
                    self.namespace
                ));
            }
            Some(_) => {}
            None => manifest
                .attributes
                .push(("package".to_string(), self.namespace.clone())),
        }

        if merge {
            for library in &self.libraries {
                let Some(path) = &library.manifest else {
                    continue;
                };

                let mut library_manifest = read_manifest_with_placeholders(
                    path,
                    application_id,
                    &self.manifest_placeholders,
                )?;

                expand_class_names(&mut library_manifest, &library.namespace);
                merge_manifest(&mut manifest, &library_manifest);
            }

            remove_entries_marked_for_removal(&mut manifest);
        }

        // min_sdk and target_sdk come from the build, like with the Gradle plugin
        manifest.children.retain(|child| child.name != "uses-sdk");
        move_aliases_last(&mut manifest);

        let content = xml::write(&manifest, &|attribute| {
            attribute.starts_with("tools:") || attribute == "xmlns:tools"
        });

        system::write_file(output, &content)?;

        return Ok(manifest);
    }

    /// The symbols of a library, its R classes are written from them. Like with Gradle,
    /// it may use resources only the app has: aapt2 checks references when linking the app.
    pub fn link_library(
        &self,
        manifest: &Path,
        own: &[PathBuf],
        work_dir: &Path,
        symbols: &Path,
    ) -> Result<(), String> {
        let with_libraries = work_dir.join("R-with-libraries.txt");
        self.link(
            manifest,
            own,
            LinkedResources::LibraryWithItsLibraries,
            &work_dir.join("library.ap_"),
            &with_libraries,
            &[],
        )?;

        if !self.non_transitive_r {
            return system::copy_file(&with_libraries, symbols);
        }

        let own_only = work_dir.join("R-own.txt");
        self.link(
            manifest,
            own,
            LinkedResources::LibraryAlone,
            &work_dir.join("library-own.ap_"),
            &own_only,
            &[],
        )?;

        return SymbolTable::write_own(
            &SymbolTable::read(&with_libraries)?,
            &SymbolTable::read(&own_only)?,
            symbols,
        );
    }

    /// For the module and every library with resources. With `ids`, the app's symbols,
    /// the fields are final with the app's ids, otherwise they are for compiling against.
    pub fn write_r_classes(
        &self,
        own_symbols: &Path,
        ids: Option<&SymbolTable>,
        output: &Path,
    ) -> Result<Vec<PathBuf>, String> {
        system::remove_dir(output)?;
        let mut packages: Vec<(&str, &Path)> = vec![(&self.namespace, own_symbols)];

        for library in &self.libraries {
            if let Some(symbols) = &library.symbols {
                if !packages.iter()
                    .any(|(package, _)| *package == library.namespace)
                {
                    packages.push((&library.namespace, symbols));
                }
            }
        }

        let mut files = vec![];

        for (package, symbols) in packages {
            files.push(write_r_class(
                package,
                &SymbolTable::read(symbols)?,
                ids,
                output,
            )?);
        }

        return Ok(files);
    }

    /// In the namespace, like the Gradle plugin writes it: `fields` ("Type NAME value"),
    /// then build_config_fields
    pub fn write_build_config(&self, fields: &[String]) -> Result<PathBuf, String> {
        let mut declarations = vec![
            format!("boolean DEBUG {}", self.build_type == BuildType::Debug),
            format!("String BUILD_TYPE {}", self.build_type.name()),
        ];

        declarations.extend(fields.iter().cloned());
        declarations.extend(self.build_config_fields.iter().cloned());
        let content = format!(
            "package {};\n\npublic final class BuildConfig {{\n{}}}\n",
            self.namespace,
            build_config_java_fields(&declarations)?,
        );

        let file = self.build_dir.join("intermediates/generated/buildconfig")
            .join(self.namespace.replace('.', "/"))
            .join("BuildConfig.java");

        system::write_file(&file, &content)?;

        return Ok(file);
    }

    /// Highest priority first, aapt2 takes the first file it finds: debug over main
    /// over the libraries, like the Gradle plugin merges them.
    pub fn merged_asset_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = self.asset_dirs.clone();
        dirs.reverse();
        dirs.extend(
            self.libraries.iter()
                .rev()
                .flat_map(|library| library.asset_dirs.iter().cloned()),
        );

        return dirs;
    }

    /// From the module's CMakeLists.txt, if it has one
    pub fn build_native_libraries(&self) -> Result<(), String> {
        let Some(cmake_lists) = &self.cmake_lists else {
            return Ok(());
        };
        return CMakeBuild {
            cmake_lists,
            ndk_version: &self.ndk_version,
            min_sdk: self.min_sdk,
            build_type: self.build_type,
            work_dir: system::build_dir(&self.inputs.directory).join(CMAKE_DIR),
            accept_license: self.accept_license,
        }
        .run(&|message| system::log(&self.inputs.name, message));
    }

    /// Those of the libraries and of the module to `output`/lib/<abi>, the module's win
    pub fn merge_native_libraries(&self, output: &Path) -> Result<(), String> {
        system::remove_dir(output)?;

        let mut dirs: Vec<PathBuf> = self.libraries.iter()
            .flat_map(|library| library.native_library_dirs.iter().cloned())
            .collect();

        dirs.extend(self.native_library_dirs.iter().cloned());
        for dir in dirs {
            for library in system::files_with_extension(&[dir.clone()], "so") {
                let relative = library.strip_prefix(&dir)
                    .map_err(|error| error.to_string())?;

                system::copy_file(&library, &output.join("lib").join(relative))?;
            }
        }

        return Ok(());
    }

    /// R and BuildConfig
    pub fn compile_generated_java(
        &self,
        sources: &[PathBuf],
        classpath: &[PathBuf],
        output: &Path,
    ) -> Result<(), String> {
        system::remove_dir(output)?;
        return module::compile_java(&self.inputs.jdk, sources, classpath, output, "11", None);
    }

    /// android.jar, the given class directories and all library jars
    pub fn android_classpath(&self, extra: &[PathBuf]) -> Vec<PathBuf> {
        let mut classpath = vec![self.sdk.android_jar.clone()];
        classpath.extend(extra.iter().cloned());
        classpath.extend(self.jars.iter().cloned());
        classpath.extend(self.compile_only.iter().cloned());

        return classpath;
    }

    /// Kotlin and Java with view bindings, AIDL and what symbol processors generate,
    /// against `generated` like R and BuildConfig. Returns the processors' resource directories.
    pub fn compile_sources(
        &self,
        generated: &[PathBuf],
        output: &Path,
    ) -> Result<Vec<PathBuf>, String> {
        // bindings may use the module's views, AIDL its classes; symbol processors skip
        // classes with types they cannot resolve, like bindings
        let mut source_dirs = self.inputs.source_dirs.clone();
        source_dirs.push(self.generate_view_bindings()?);
        source_dirs.push(self.generate_aidl()?);

        return Compilation {
            inputs: &self.inputs,
            tools: &self.resolved.tools,
            variant: self.variant,
            source_dirs: &source_dirs,
            classpath: &self.android_classpath(generated),
            friends: &[],
            symbol_processors: self.resolved.symbol_processors.as_ref(),
            annotation_processors: &self.resolved.annotation_processor_path,
            target: TargetPlatform::Android,
            output,
        }
        .run();
    }

    pub fn jar(&self, directory: &Path, jar: &Path) -> Result<PathBuf, String> {
        create_jar(&self.inputs.jdk, jar, None, &[directory])?;
        return Ok(jar.to_path_buf());
    }

    /// desugar_jdk_libs and its d8 configuration, like isCoreLibraryDesugaringEnabled
    pub fn desugared_library(
        &self,
        version: &str,
        work_dir: &Path,
    ) -> Result<DesugaredLibrary, String> {
        let mut resolver =
            DependencyResolver::new(self.inputs.repositories.clone(), TargetPlatform::Jvm);

        let jars = resolver.resolve_with_platforms(
            &[MavenCoordinate::new(
                "com.android.tools",
                "desugar_jdk_libs",
                version,
            )],
            &[],
        )?;

        let configuration_jar = jars.iter()
            .find(|jar| {
                jar.file_name().is_some_and(|name| {
                    name.to_string_lossy()
                        .starts_with("desugar_jdk_libs_configuration")
                })
            })
            .ok_or("desugar_jdk_libs came without its configuration")?;

        let configuration =
            system::read_zip_entry(configuration_jar, "META-INF/desugar/d8/desugar.json")?;

        let config = work_dir.join("desugar.json");
        system::write_file(&config, &String::from_utf8_lossy(&configuration))?;

        return Ok(DesugaredLibrary { config, jars });
    }

    /// With L8, added after the app's dex files. Kept in `work_dir` until the
    /// library or how it is dexed changes. `keep_rules` of R8 shrink it.
    pub fn dex_desugared_library(
        &self,
        library: &DesugaredLibrary,
        keep_rules: Option<&Path>,
        dex_dir: &Path,
        work_dir: &Path,
    ) -> Result<(), String> {
        let keep_rules_content = match keep_rules {
            Some(rules) => system::read_file(rules)?,
            None => String::new(),
        };

        let inputs = (
            up_to_date::files_hash(&library.jars)?,
            system::read_file(&library.config)?,
            keep_rules_content,
        );

        let dexed = self.cached_dex(work_dir, inputs, |partial| {
            let mut l8 = self.r8_command("L8", partial)?;
            let command = &mut l8.command;

            command.arg(self.dex_mode())
                .arg("--desugared-lib")
                .arg(&library.config);

            if let Some(rules) = keep_rules {
                command.arg("--pg-conf").arg(rules);
            }

            command.args(&library.jars);
            return l8.run(|command| system::run_command(command, "L8"));
        })?;

        let mut next = system::files_with_extension(&[dex_dir.to_path_buf()], "dex").len() + 1;
        for dex in dex_files(&dexed) {
            system::copy_file(&dex, &dex_dir.join(format!("classes{next}.dex")))?;
            next += 1;
        }

        return Ok(());
    }

    /// The own classes and those of library modules on every build, other libraries once
    /// and kept until one changes, like the Gradle plugin. Native multidex loads both.
    pub fn dex(
        &self,
        own: &[PathBuf],
        output: &Path,
        desugared: Option<&DesugaredLibrary>,
    ) -> Result<(), String> {
        // like Android Studio does for debug builds: below API 21 one dex file
        // would have to hold everything, so dex for native multidex instead
        if self.min_api() != self.min_sdk {
            system::log(
                &self.inputs.name,
                &format!(
                    "Note: min_sdk {} is below {NATIVE_MULTIDEX_API}, the debug apk uses native multidex and needs Android 5.0 or newer",
                    self.min_sdk,
                ),
            );
        }

        let modules: HashSet<&PathBuf> = self.module_jars.iter().collect();
        let own = unique(own.iter().chain(&self.module_jars));
        let libraries = unique(self.jars.iter().filter(|jar| !modules.contains(jar)));

        let libraries_dex = self.dexed_libraries(&libraries, desugared)?;
        let own_dex = self.build_dir.join("intermediates/dex-own");
        system::recreate_dir(&own_dex)?;
        self.run_d8(&own, &libraries, &own_dex, desugared)?;

        // the own classes first, in classes.dex
        system::recreate_dir(output)?;
        for (index, dex) in dex_files(&own_dex)
            .into_iter()
            .chain(dex_files(&libraries_dex))
            .enumerate()
        {
            let name = if index == 0 {
                "classes.dex".to_string()
            } else {
                format!("classes{}.dex", index + 1)
            };

            system::copy_file(&dex, &output.join(name))?;
        }

        return Ok(());
    }

    /// In intermediates/dex-libraries/<hash of the libraries and of how d8 runs>
    fn dexed_libraries(
        &self,
        libraries: &[PathBuf],
        desugared: Option<&DesugaredLibrary>,
    ) -> Result<PathBuf, String> {
        let desugared_config = desugared.map(|desugared| system::read_file(&desugared.config)).transpose()?;
        let inputs = (up_to_date::files_hash(libraries)?, desugared_config);
        let mut dexed_now = false;

        let dex = self.cached_dex(&self.build_dir.join("intermediates/dex-libraries"), inputs, |partial| {
            dexed_now = true;

            if libraries.is_empty() {
                return Ok(());
            }

            system::log(
                &self.inputs.name,
                &format!("Dexing {} libraries, again only when they change", libraries.len()),
            );

            return self.run_d8(libraries, &[], partial, desugared);
        })?;

        if !dexed_now {
            system::log(&self.inputs.name, "Libraries dexed before");
        }

        return Ok(dex);
    }

    /// What `dex` writes, in `directory`/<hash of `inputs` and of how d8 runs>. Kept until
    /// one of them changes, only that of the current inputs stays.
    fn cached_dex(
        &self,
        directory: &Path,
        inputs: impl Hash,
        dex: impl FnOnce(&Path) -> Result<(), String>,
    ) -> Result<PathBuf, String> {
        let key = up_to_date::hash_hex((
            &self.sdk.d8,
            &self.sdk.android_jar,
            self.min_api(),
            self.build_type.name(),
            inputs,
        ));
        let dexed = directory.join(key);

        if dexed.is_dir() {
            return Ok(dexed);
        }

        system::recreate_dir(directory)?;

        system::produce_atomically(&dexed, |partial| {
            system::create_dir(partial)?;
            return dex(partial);
        })?;

        return Ok(dexed);
    }

    /// D8, L8 or R8 of the build tools, against android.jar into `output`
    pub fn r8_command(&self, tool: &str, output: &Path) -> Result<ToolCommand, String> {
        let main_class = format!("com.android.tools.r8.{tool}");
        let mut r8 = self.inputs.jdk.tool_command(&tool.to_ascii_lowercase(), &[self.sdk.d8.clone()], &main_class)?;

        r8.command
            .arg("--min-api")
            .arg(self.min_api().to_string())
            .arg("--lib")
            .arg(&self.sdk.android_jar)
            .arg("--output")
            .arg(output);

        return Ok(r8);
    }

    /// `classpath` only to desugar against, like the libraries for the own classes
    fn run_d8(
        &self,
        inputs: &[PathBuf],
        classpath: &[PathBuf],
        output: &Path,
        desugared: Option<&DesugaredLibrary>,
    ) -> Result<(), String> {
        let mut d8 = self.r8_command("D8", output)?;
        let command = &mut d8.command;
        command.arg(self.dex_mode());

        for jar in classpath {
            command.arg("--classpath").arg(jar);
        }

        if let Some(desugared) = desugared {
            command.arg("--desugared-lib").arg(&desugared.config);
        }

        command.args(inputs);

        return d8.run(|command| self.run_dex_tool(command, "d8"));
    }

    /// d8 or R8, with the notes split_d8_noise hides only counted
    pub fn run_dex_tool(&self, command: &mut Command, tool: &str) -> Result<(), String> {
        let result = command.stdin(Stdio::null())
            .output()
            .map_err(|error| format!("could not start {tool}: {error}"))?;

        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );

        if !result.status.success() {
            eprint!("{text}");
            return Err(format!("{tool} failed ({})", result.status));
        }

        let (shown, hidden) = split_d8_noise(&text);

        for line in shown {
            eprintln!("{line}");
        }

        if hidden > 0 {
            let message = format!(
                "{tool}: hid {hidden} lines of notes, most about Kotlin metadata newer than this {tool} knows"
            );
            system::log(&self.inputs.name, &message);
        }

        return Ok(());
    }

    /// Resources, dex, Java resources and native libraries, aligned and signed as `signature` says
    pub fn package(
        &self,
        linked: &Path,
        dex_dir: &Path,
        java_resources: &Path,
        native_libraries: &Path,
        work_dir: &Path,
        signature: &Signature,
        apk: &Path,
    ) -> Result<(), String> {
        let unaligned = work_dir.join("unaligned.apk");
        let aligned = work_dir.join("aligned.apk");
        system::copy_file(linked, &unaligned)?;

        let dex_files: Vec<(String, PathBuf)> = system::files_with_extension(&[dex_dir.to_path_buf()], "dex")
            .into_iter()
            .map(|dex| (dex.file_name().unwrap_or_default().to_string_lossy().to_string(), dex))
            .collect();
        system::add_to_zip(&unaligned, &dex_files, false)?;
        system::add_to_zip(&unaligned, &system::relative_files(java_resources), false)?;

        // stored and page aligned, so they load from the apk and extractNativeLibs="false" works
        system::add_to_zip(&unaligned, &system::relative_files(native_libraries), true)?;

        self.zipalign(&unaligned, &aligned)?;
        system::create_parent_dir(apk)?;

        return self.sign(&aligned, signature, apk);
    }

    fn zipalign(&self, unaligned: &Path, aligned: &Path) -> Result<(), String> {
        let _ = fs::remove_file(aligned);
        let mut zipalign = Command::new(&self.sdk.zipalign);

        // 16 KB pages like Android 15 devices may have, older zipaligns only know 4 KB
        match self.sdk.build_tools.split('.')
            .next()
            .and_then(|major| major.parse::<u32>().ok())
        {
            Some(major) if major >= 35 => zipalign.args(["-P", "16"]),
            _ => zipalign.arg("-p"),
        };

        zipalign.args(["-f", "4"])
            .arg(unaligned)
            .arg(aligned);

        return system::run_command(&mut zipalign, "zipalign");
    }

    /// With the debug key, the release key or not at all, verified after
    fn sign(&self, aligned: &Path, signature: &Signature, apk: &Path) -> Result<(), String> {
        let mut sign = self.inputs.jdk.tool_command("apksigner", &[self.sdk.apksigner.clone()], APKSIGNER_MAIN_CLASS)?;
        let command = &mut sign.command;
        command.arg("sign");

        match signature {
            Signature::Unsigned => return system::copy_file(aligned, apk),
            Signature::Debug => {
                command.arg("--ks")
                    .arg(debug_keystore(&self.inputs.jdk)?)
                    // the well known password of the debug key every Android tool uses
                    .args([
                        "--ks-pass",
                        "pass:android",
                        "--key-pass",
                        "pass:android",
                        "--ks-key-alias",
                        "androiddebugkey",
                    ]);
            }
            // apksigner reads the passwords from the environment, they stay out of the arguments
            Signature::Release(key) => {
                command.arg("--ks")
                    .arg(&key.keystore)
                    .arg("--ks-key-alias")
                    .arg(&key.alias)
                    .arg("--ks-pass")
                    .arg(format!("env:{}", key.keystore_password_env))
                    .arg("--key-pass")
                    .arg(format!("env:{}", key.key_password_env));
            }
        }

        command.arg("--min-sdk-version")
            .arg(self.min_sdk.to_string())
            .arg("--out")
            .arg(apk)
            .arg(aligned);
        sign.run(|command| system::run_command(command, "apksigner sign"))?;

        // a sanity check, its warnings about META-INF files outside the v2/v3 signature are expected
        let mut verify = self.inputs.jdk.tool_command("apksigner", &[self.sdk.apksigner.clone()], APKSIGNER_MAIN_CLASS)?;
        verify.command
            .arg("verify")
            .arg(apk);

        return verify.run(|command| system::run_command_quietly(command, "apksigner verify"));
    }

    /// aapt2 link of the module's resources `own`, the ids in `symbols`
    pub fn link(
        &self,
        manifest: &Path,
        own: &[PathBuf],
        resources: LinkedResources,
        output: &Path,
        symbols: &Path,
        extra: &[String],
    ) -> Result<(), String> {
        system::create_parent_dir(output)?;
        system::create_parent_dir(symbols)?;

        let mut command = Command::new(&self.sdk.aapt2);
        command.arg("link")
            .arg("-I")
            .arg(&self.sdk.android_jar)
            .arg("--manifest")
            .arg(manifest)
            .arg("-o")
            .arg(output)
            .arg("--output-text-symbols")
            .arg(symbols)
            .arg("--auto-add-overlay")
            .arg("--min-sdk-version")
            .arg(self.min_sdk.to_string())
            .args(extra);

        if resources != LinkedResources::App {
            command.arg("--non-final-ids");
            // aapt2 links stubs of the framework itself only as an app
            if self.namespace != "android" {
                command.args(["--static-lib", "--merge-only"]);
            }
        }

        // later overlays win: libraries, then the module itself
        let libraries = if resources == LinkedResources::LibraryAlone {
            &[][..]
        } else {
            &self.libraries[..]
        };

        for compiled in libraries.iter()
            .flat_map(|library| &library.compiled)
            .chain(own)
        {
            command.arg("-R").arg(compiled);
        }

        return system::run_command(&mut command, "aapt2 link");
    }

    /// Java interfaces for the .aidl files, like the Gradle plugin's aidl task.
    /// Returns the directory they are in.
    fn generate_aidl(&self) -> Result<PathBuf, String> {
        let output = self.build_dir.join("intermediates/generated/aidl");
        system::remove_dir(&output)?;
        let files = system::files_with_extension(&self.aidl_dirs, "aidl");

        if files.is_empty() {
            return Ok(output);
        }

        system::log(
            &self.inputs.name,
            &format!("Generating Java for {} AIDL file(s)", files.len()),
        );
        for file in &files {
            let mut command = Command::new(&self.sdk.aidl);
            command.arg(format!("-p{}", self.sdk.framework_aidl.display()));

            for include in self.aidl_dirs.iter().chain(&self.aidl_imports) {
                command.arg(format!("-I{}", include.display()));
            }

            command.arg(format!("-o{}", output.display())).arg(file);
            system::run_command(&mut command, &format!("aidl {}", file.display()))?;
        }

        return Ok(output);
    }

    /// <Layout>Binding classes for the own layouts, like view_binding = true.
    /// Returns the directory they are in.
    fn generate_view_bindings(&self) -> Result<PathBuf, String> {
        let output = self.build_dir.join("intermediates/generated/viewbinding");
        system::remove_dir(&output)?;

        if !self.view_binding {
            return Ok(output);
        }

        let layouts = layouts_by_name(&self.res_dirs)?;
        // <include>s may bind layouts of library modules with their classes,
        // searched from the last module on, which wins like for resources
        let mut libraries = vec![];

        for module in BuiltModule::transitive(&self.inputs.dependencies.modules)
            .iter()
            .rev()
        {
            if let Some(android) = module
                .jvm().android.as_ref()
                .filter(|android| android.view_binding)
            {
                libraries.push((
                    android.namespace.clone(),
                    layouts_by_name(&android.res_dirs)?,
                ));
            }
        }

        let directory = output.join(self.namespace.replace('.', "/"))
            .join("databinding");

        for (layout, variants) in &layouts {
            if let Some(source) =
                view_binding_source(&self.namespace, layout, variants, &layouts, &libraries)
            {
                system::write_file(
                    &directory.join(format!("{}.java", binding_class(layout))),
                    &source,
                )?;
            }
        }

        return Ok(output);
    }

    /// --debug keeps the debug information in the dex files, --release does not
    fn dex_mode(&self) -> &'static str {
        return match self.build_type {
            BuildType::Debug => "--debug",
            BuildType::Release => "--release",
        };
    }

    /// The module file for IntelliJ: the jars of the aars, android.jar, the R and BuildConfig classes
    pub fn write_idea_module(&self, project_dir: &Path, args: &Arguments) -> Result<(), String> {
        let generated = self.build_dir.join("intermediates/generated");
        let generated_dirs = ["r", "buildconfig", "aidl", "viewbinding"].map(|kind| generated.join(kind));

        // library modules are modules of IntelliJ, not jars
        let libraries = unique(self.jars.iter().filter(|jar| !self.module_jars.contains(jar)));
        let mut compile_only = self.compile_only.clone();
        compile_only.push(self.sdk.android_jar.clone());

        let module = IdeaModule {
            inputs: &self.inputs,
            resolved: &self.resolved,
            platform: TargetPlatform::Android,
            libraries: &libraries,
            compile_only: &compile_only,
            generated_dirs: &generated_dirs,
        };

        return idea::write_module(project_dir, args, &module);
    }

    /// The ProGuard rules the aars and library modules bring for the apps using them
    pub fn library_proguard_rules(&self) -> Vec<PathBuf> {
        return self.libraries.iter()
            .flat_map(|library| library.proguard_rules.iter().cloned())
            .collect();
    }

    /// API level the dex files are made for. R8 can put what starts the app
    /// into the first dex file, d8 cannot.
    pub fn min_api(&self) -> i64 {
        return match self.build_type {
            BuildType::Debug => self.min_sdk.max(NATIVE_MULTIDEX_API),
            BuildType::Release => self.min_sdk,
        };
    }
}

/// Each path once, in the order they come
pub fn unique<'a>(paths: impl IntoIterator<Item = &'a PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    return paths.into_iter()
        .filter(|path| seen.insert(*path))
        .cloned()
        .collect();
}

/// classes.dex, classes2.dex, ... of a d8 output, in that order
fn dex_files(directory: &Path) -> Vec<PathBuf> {
    let mut files = system::files_with_extension(&[directory.to_path_buf()], "dex");
    files.sort_by_key(|file| {
        let stem = file.file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        stem.trim_start_matches("classes")
            .parse::<usize>()
            .unwrap_or(1)
    });

    return files;
}

/// Resources of an aar or a library module
struct AndroidResourceLibrary {
    namespace: String,
    /// outputs of aapt2 compile, empty without resources
    compiled: Vec<PathBuf>,
    manifest: Option<PathBuf>,
    /// R.txt: what its R class has
    symbols: Option<PathBuf>,
    asset_dirs: Vec<PathBuf>,
    /// directories with <abi>/*.so: jni/ of aars, jniLibs/ of modules
    native_library_dirs: Vec<PathBuf>,
    /// for the apps using it: proguard.txt of aars, consumer_proguard_files of modules
    proguard_rules: Vec<PathBuf>,
}

/// What the libraries of a module bring: classes to compile against and to
/// dex, resources to link, parcelables for AIDL
struct UnpackedLibraries {
    jars: Vec<PathBuf>,
    /// of `jars` those of library modules
    module_jars: Vec<PathBuf>,
    compile_only: Vec<PathBuf>,
    aidl_imports: Vec<PathBuf>,
    /// aars and library modules with resources, lowest priority first
    libraries: Vec<AndroidResourceLibrary>,
}

impl UnpackedLibraries {
    fn unpack(
        resolved: &ResolvedDependencies,
        sdk: &AndroidSdk,
        build_dir: &Path,
    ) -> Result<Self, String> {
        let mut unpacked = UnpackedLibraries {
            jars: vec![],
            module_jars: vec![],
            compile_only: vec![],
            aidl_imports: vec![],
            libraries: vec![],
        };

        let mut aars = vec![];

        for file in &resolved.libraries {
            if file.extension().is_some_and(|extension| extension == "aar") {
                aars.push(unpacked.unpack_aar(file, sdk)?);
            } else {
                unpacked.jars.push(file.clone());
            }
        }

        // resolution order is direct dependencies first, overlays need the reverse
        aars.reverse();
        unpacked.libraries.extend(aars);

        for module in &resolved.modules {
            let output = module.jvm();
            unpacked.jars.extend(output.classes.iter().cloned());
            unpacked.module_jars.extend(output.classes.iter().cloned());

            let Some(android) = &output.android else {
                continue;
            };

            let mut compiled = vec![];

            for (index, res_dir) in android.res_dirs.iter().enumerate() {
                compiled.push(compiled_resources(
                    sdk,
                    res_dir,
                    &build_dir.join("intermediates/res"),
                    &format!("{}-{index}", module.name),
                )?);
            }

            unpacked.libraries.push(AndroidResourceLibrary {
                namespace: android.namespace.clone(),
                compiled,
                manifest: android.manifest.clone(),
                symbols: android.symbols.clone(),
                asset_dirs: android.asset_dirs.clone(),
                native_library_dirs: android.native_library_dirs.clone(),
                proguard_rules: android.consumer_proguard_files.clone(),
            });
        }

        // compile only aars contribute their classes, not their resources
        for file in &resolved.compile_only {
            if file.extension().is_some_and(|extension| extension == "aar") {
                let extract = |target: &Path| system::extract(file, target);
                let extracted = system::kept_in_cache("extracted", file, extract)?;
                unpacked
                    .compile_only
                    .extend(system::files_with_extension(&[extracted], "jar"));
            } else {
                unpacked.compile_only.push(file.clone());
            }
        }

        return Ok(unpacked);
    }

    /// Extracted once into the cache, a released aar never changes
    fn unpack_aar(
        &mut self,
        aar: &Path,
        sdk: &AndroidSdk,
    ) -> Result<AndroidResourceLibrary, String> {
        let stem = aar.file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        let extract = |target: &Path| system::extract(aar, target);
        let target = system::kept_in_cache("extracted", aar, extract)?;

        let classes = target.join("classes.jar");

        if classes.is_file() {
            self.jars.push(classes);
        }

        if target.join("aidl").is_dir() {
            self.aidl_imports.push(target.join("aidl"));
        }

        self.jars
            .extend(system::files_with_extension(&[target.join("libs")], "jar"));

        let manifest = target.join("AndroidManifest.xml");

        let namespace = xml::parse(&system::read_file(&manifest)?)
            .map_err(|error| format!("{}: {error}", manifest.display()))?
            .attribute("package")
            .ok_or_else(|| format!("{} has no package", manifest.display()))?
            .to_string();

        // compiled once per extracted aar: its directory changes with the aar, also a local one overwritten in place
        let res_dir = target.join("res");
        let mut compiled = vec![];

        if system::contains_files(&res_dir) {
            let cached = system::cache_dir("android/res")
                .join(&sdk.build_tools)
                .join(format!("{stem}-{}.zip", up_to_date::hash_hex(&target)));

            if !cached.is_file() {
                compile_resources(&sdk.aapt2, &res_dir, &cached)?;
            }

            compiled.push(cached);
        }

        return Ok(AndroidResourceLibrary {
            namespace,
            compiled,
            manifest: Some(manifest),
            symbols: Some(target.join("R.txt")).filter(|symbols| symbols.is_file()),
            asset_dirs: [target.join("assets")].into_iter()
                .filter(|dir| system::contains_files(dir))
                .collect(),
            native_library_dirs: [target.join("jni")].into_iter()
                .filter(|dir| system::contains_files(dir))
                .collect(),
            proguard_rules: [target.join("proguard.txt")].into_iter()
                .filter(|rules| rules.is_file())
                .collect(),
        });
    }
}

/// The shared libraries of one CMakeLists.txt, like externalNativeBuild in the Android Gradle plugin
struct CMakeBuild<'a> {
    cmake_lists: &'a Path,
    ndk_version: &'a str,
    min_sdk: i64,
    build_type: BuildType,
    /// CMAKE_DIR of the module
    work_dir: PathBuf,
    accept_license: bool,
}

impl CMakeBuild<'_> {
    /// Installs the NDK and CMake if needed, then configures and builds the
    /// libraries for every ABI, like the Gradle plugin
    fn run(&self, log: &dyn Fn(&str)) -> Result<(), String> {
        let ndk = sdk::ndk(self.ndk_version, self.accept_license)?;
        let cmake_home = sdk::find_or_install(&format!("cmake;{CMAKE_VERSION}"), self.accept_license)?;

        let cmake = cmake_home.join("bin")
            .join(format!("cmake{}", std::env::consts::EXE_SUFFIX));

        let ninja = cmake_home.join("bin")
            .join(format!("ninja{}", std::env::consts::EXE_SUFFIX));

        let source_dir = self.cmake_lists.parent()
            .ok_or_else(|| format!("{} has no directory", self.cmake_lists.display()))?;

        let libraries = self.work_dir.join("libs");
        system::remove_dir(&libraries)?;

        for abi in ABIS {
            log(&format!(
                "Building native libraries for {abi} (CMake, NDK {})",
                self.ndk_version
            ));
            let build_dir = self.work_dir.join("build").join(abi);
            let mut configure = Command::new(&cmake);
            configure.arg("-S")
                .arg(source_dir)
                .arg("-B")
                .arg(&build_dir)
                .args(["-G", "Ninja"])
                .arg(format!("-DCMAKE_MAKE_PROGRAM={}", ninja.display()))
                .arg(format!(
                    "-DCMAKE_TOOLCHAIN_FILE={}",
                    ndk.join("build/cmake/android.toolchain.cmake").display()
                ))
                .arg(format!("-DANDROID_NDK={}", ndk.display()))
                .arg(format!("-DANDROID_ABI={abi}"))
                .arg(format!("-DANDROID_PLATFORM=android-{}", self.min_sdk))
                .arg(match self.build_type {
                    BuildType::Debug => "-DCMAKE_BUILD_TYPE=Debug",
                    BuildType::Release => "-DCMAKE_BUILD_TYPE=Release",
                })
                .arg(format!(
                    "-DCMAKE_LIBRARY_OUTPUT_DIRECTORY={}",
                    libraries.join(abi).display()
                ));
            system::run_command(&mut configure, &format!("cmake for {abi}"))?;

            let mut build = Command::new(&cmake);
            build.arg("--build").arg(&build_dir);
            system::run_command(&mut build, &format!("native build for {abi}"))?;
        }

        return Ok(());
    }
}

/// aapt2 compile of a res directory into <name>-<hash of its files>.zip in `res_cache`,
/// again only when they change: an app compiles those of all its library modules
fn compiled_resources(
    sdk: &AndroidSdk,
    res_dir: &Path,
    res_cache: &Path,
    name: &str,
) -> Result<PathBuf, String> {
    let key = up_to_date::hash_hex((&sdk.build_tools, up_to_date::files_hash(&[res_dir.to_path_buf()])?));
    let output = res_cache.join(format!("{name}-{key}.zip"));

    if output.is_file() {
        return Ok(output);
    }

    // what the directory compiled to before
    let earlier = |file: &PathBuf| {
        file.file_name()
            .and_then(|file_name| {
                file_name.to_str()?
                    .strip_prefix(&format!("{name}-"))?
                    .strip_suffix(".zip")
                    .map(str::to_string)
            })
            .is_some_and(|hash| {
                hash.len() == 16 && hash.chars().all(|char| char.is_ascii_hexdigit())
            })
    };

    for file in system::files_with_extension(&[res_cache.to_path_buf()], "zip")
        .iter()
        .filter(|file| earlier(file))
    {
        fs::remove_file(file)
            .map_err(|error| format!("could not delete {}: {error}", file.display()))?;
    }

    compile_resources(&sdk.aapt2, res_dir, &output)?;

    return Ok(output);
}

fn compile_resources(aapt2: &Path, res_dir: &Path, output: &Path) -> Result<(), String> {
    return system::produce_atomically(output, |partial| {
        let mut command = Command::new(aapt2);
        command.arg("compile")
            .arg("--dir")
            .arg(res_dir)
            .arg("-o")
            .arg(partial);

        return system::run_command(&mut command, &format!("aapt2 compile of {}", res_dir.display()));
    });
}

/// `name` (jniLibs) of the main source set and of the build type's where it has files, lowest priority first
fn source_set_dirs_with_files(
    inputs: &ModuleBuildInputs,
    name: &str,
    build_type: BuildType,
) -> Vec<PathBuf> {
    return ["main", build_type.name()].iter()
        .map(|source_set| inputs.directory.join("src").join(source_set).join(name))
        .filter(|dir| system::contains_files(dir))
        .collect();
}

/// Output of d8 and R8 worth showing, and how many lines of noise it hid: they warn
/// about every class with Kotlin metadata newer than they know, harmless for dexing
fn split_d8_noise(text: &str) -> (Vec<&str>, usize) {
    // a diagnostic starts with Warning/Info/Error and ends at the next one or an empty line
    let mut blocks: Vec<Vec<&str>> = vec![vec![]];

    for line in text.lines() {
        if line.trim().is_empty() {
            blocks.push(vec![]);
            continue;
        }

        if line.starts_with("Warning") || line.starts_with("Info") || line.starts_with("Error") {
            blocks.push(vec![]);
        }

        blocks.last_mut()
            .unwrap()
            .push(line);
    }

    let mut shown = vec![];
    let mut hidden = 0;

    for block in blocks.into_iter().filter(|block| !block.is_empty()) {
        let noise = block[0].starts_with("Info")
            || block.iter()
                .any(|line| line.contains("otlin metadata") || line.contains("kotlin.Metadata"));

        if noise {
            hidden += block.len();
        } else {
            shown.extend(block);
        }
    }

    return (shown, hidden);
}

/// Like ~/.android/debug.keystore, created on first use
fn debug_keystore(jdk: &Jdk) -> Result<PathBuf, String> {
    let keystore = system::data_dir("android").join("debug.keystore");

    if keystore.is_file() {
        return Ok(keystore);
    }

    system::create_parent_dir(&keystore)?;
    let mut command = Command::new(jdk.tool_path("keytool"));
    command.arg("-genkeypair")
        .arg("-keystore")
        .arg(&keystore)
        .args([
            "-storepass",
            "android",
            "-keypass",
            "android",
            "-alias",
            "androiddebugkey",
        ])
        .args(["-keyalg", "RSA", "-keysize", "2048", "-validity", "10000"])
        .args(["-dname", "CN=Android Debug,O=Android,C=US"]);
    system::run_command(&mut command, "keytool")?;

    return Ok(keystore);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d8_noise_is_hidden() {
        let output = "Warning: An error occurred when parsing kotlin metadata. This normally happens ...
Warning: Unexpected error during rewriting of Kotlin metadata for class 'kotlin.ranges.UIntProgression':
com.android.tools.r8.internal.wc4: Should never be called

Info in /x/coroutines.jar:kotlinx/coroutines/A.class:
Methods with invalid locals information:
  void a()
Warning in /x/leakcanary.jar:leakcanary/B.class:
Type `androidx.work.Worker` was not found, it is required for default or static interface methods desugaring
";

        let (shown, hidden) = split_d8_noise(output);
        assert_eq!(
            shown,
            [
                "Warning in /x/leakcanary.jar:leakcanary/B.class:",
                "Type `androidx.work.Worker` was not found, it is required for default or static interface methods desugaring",
            ]
        );
        assert_eq!(hidden, 6);
    }
}
