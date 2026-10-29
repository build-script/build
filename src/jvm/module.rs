// JVM module builds: what every module function reads from its arguments, resolving it, compiling it with kotlinc and javac, jars
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use crate::common::system;
use crate::common::up_to_date::InputFingerprint;
use crate::language::runtime::{
    Arguments, BuildScriptContext, BuiltModule, FunctionParameters, ModuleOutput, TestSuite, Type,
    Value,
};

use super::android::AndroidResources;
use super::jdk::{self, Jdk, join_classpath};
use super::ksp::{
    DEFAULT_KSP_VERSION, ResolvedSymbolProcessors, SymbolProcessing, SymbolProcessors,
};
use super::maven::{
    self, DependencyNotation, DependencyResolver, MavenCoordinate, MavenRepository, TargetPlatform,
};
use super::resources::JavaResources;
use super::{JvmLaunch, JvmOutput};

pub const DEFAULT_KOTLIN_VERSION: &str = "2.4.21";

const KOTLIN_COMPILER_MAIN_CLASS: &str = "org.jetbrains.kotlin.cli.jvm.K2JVMCompiler";
/// bytecode level handed to d8, the default of current Android Gradle plugins
pub const ANDROID_JVM_TARGET: &str = "17";
/// Short names for plugins of Kotlin itself: artifact of org.jetbrains.kotlin, runtime library, options
const KOTLIN_COMPILER_PLUGINS: &[(&str, &str, Option<&str>, &[&str])] = &[
    (
        "compose",
        "kotlin-compose-compiler-plugin-embeddable",
        None,
        &[],
    ),
    (
        "serialization",
        "kotlin-serialization-compiler-plugin-embeddable",
        None,
        &[],
    ),
    (
        "parcelize",
        "kotlin-parcelize-compiler",
        Some("kotlin-parcelize-runtime"),
        &[],
    ),
    (
        "spring",
        "kotlin-allopen-compiler-plugin-embeddable",
        None,
        &["plugin:org.jetbrains.kotlin.allopen:preset=spring"],
    ),
    (
        "jpa",
        "kotlin-noarg-compiler-plugin-embeddable",
        None,
        &["plugin:org.jetbrains.kotlin.noarg:preset=jpa"],
    ),
];

/// What the sources of a module may be: build_kotlin and Android modules compile both
/// with kotlinc and javac and bring kotlin-stdlib, build_java only uses javac
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sources {
    KotlinAndJava,
    Java,
}

/// What every module function reads from its arguments
pub struct ModuleBuildInputs {
    pub name: String,
    pub sources: Sources,
    pub directory: PathBuf,
    /// with the platforms of compile_only, they apply to everything
    pub dependencies: Dependencies,
    /// on the classpath for compiling only, like compileOnly in Gradle
    pub compile_only: Dependencies,
    pub repositories: Vec<MavenRepository>,
    pub kotlin_version: String,
    pub jdk: Jdk,
    /// Kotlin and Java sources, the generated ones included
    pub source_dirs: Vec<PathBuf>,
    pub plugins: Vec<KotlinCompilerPlugin>,
    /// -P options of the plugins besides those of their short names
    pub plugin_options: Vec<String>,
    /// like freeCompilerArgs in Gradle
    pub kotlin_args: Vec<String>,
    /// javac processors, like annotationProcessor in Gradle
    pub annotation_processors: Dependencies,
    pub symbol_processors: SymbolProcessors,
    pub resources: JavaResources,
}

/// A dependency list of the script: Maven libraries, platforms (BOMs) and modules built before
#[derive(Debug, Clone, Default)]
pub struct Dependencies {
    pub libraries: Vec<MavenCoordinate>,
    pub platforms: Vec<MavenCoordinate>,
    pub modules: Vec<Arc<BuiltModule>>,
    /// shared libraries of build_cpp, for System.loadLibrary
    pub native: Vec<Arc<BuiltModule>>,
}

pub struct ResolvedDependencies {
    /// jars of kotlin-compiler-embeddable
    pub compiler: Vec<PathBuf>,
    /// jars of the compiler plugins, and their options
    pub plugins: Vec<PathBuf>,
    pub plugin_options: Vec<String>,
    /// Maven artifacts: jars and, for Android, aars
    pub libraries: Vec<PathBuf>,
    /// jars and aars only to compile against
    pub compile_only: Vec<PathBuf>,
    /// all modules depended on, dependencies first
    pub modules: Vec<Arc<BuiltModule>>,
    /// jars of the javac processors and what they need
    pub annotation_processor_path: Vec<PathBuf>,
    pub symbol_processors: Option<ResolvedSymbolProcessors>,
}

/// A Kotlin compiler plugin, like kotlin("plugin.serialization") in Gradle
#[derive(Debug, Clone)]
pub struct KotlinCompilerPlugin {
    /// built for kotlin-compiler-embeddable, passed as -Xplugin
    pub coordinate: MavenCoordinate,
    /// library the code needs at runtime, like the @Parcelize annotation
    pub runtime: Option<MavenCoordinate>,
    /// passed as -P
    pub options: Vec<String>,
}

/// One compilation of a module or its tests: symbol processors, kotlinc (which reads
/// the Java sources too), then javac with the annotation processors
pub struct Compilation<'a> {
    pub inputs: &'a ModuleBuildInputs,
    /// the compiler and its plugins
    pub tools: &'a ResolvedDependencies,
    /// main, test or debug: names the directories the generated sources go to
    pub variant: &'a str,
    pub source_dirs: &'a [PathBuf],
    pub classpath: &'a [PathBuf],
    /// of the classpath, what the sources see the internal declarations of: the module for its tests
    pub friends: &'a [PathBuf],
    pub symbol_processors: Option<&'a ResolvedSymbolProcessors>,
    pub annotation_processors: &'a [PathBuf],
    /// Android compiles against android.jar instead of the JDK
    pub target: TargetPlatform,
    pub output: &'a Path,
}

/// The annotation processors of a javac run and where they write their
/// sources, javac compiles those in the same run
pub struct AnnotationProcessing<'a> {
    pub processor_path: &'a [PathBuf],
    pub generated_dir: &'a Path,
}

/// `list<str | module>`: Maven coordinates and modules built before
pub fn dependencies_type() -> Type {
    return Type::list(Type::Union(vec![Type::String, Type::Module]));
}

/// src/<source set>/<kind> for every source set and kind, like "src/main/kotlin"
pub fn source_set_dirs(source_sets: &[&str], kinds: &[&str]) -> Value {
    let dirs: Vec<String> = source_sets.iter()
        .flat_map(|source_set| {
            kinds.iter()
                .map(move |kind| format!("src/{source_set}/{kind}"))
        })
        .collect();
    return Value::List(dirs.into_iter().map(Value::String).collect());
}

/// The parameters ModuleBuildInputs reads, Kotlin's only for Kotlin sources. Sources
/// and resources come from `source_sets`, later ones lie over earlier ones.
pub fn module_parameters(
    parameters: FunctionParameters,
    source_sets: &[&str],
    sources: Sources,
) -> FunctionParameters {
    let strings = || Type::list(Type::String);
    let none = || Value::string_list(&[]);
    let kotlin = sources == Sources::KotlinAndJava;
    let mut parameters = parameters.with_default("compile_only", dependencies_type(), none());

    if kotlin {
        parameters = parameters
            // short names like "compose" or Maven coordinates of compiler plugins
            .with_default("plugins", strings(), none())
            // passed to the plugins as -P, like plugin:org.jetbrains.kotlin.allopen:annotation=com.example.Open
            .with_default("plugin_options", strings(), none())
            .with_default("kotlin_args", strings(), none());
    }

    // like annotationProcessor in Gradle: javac processors for the Java sources
    parameters = parameters.with_default("annotation_processors", dependencies_type(), none());

    if kotlin {
        parameters = parameters
            // like ksp in Gradle: Kotlin symbol processors, run before kotlinc
            .with_default("ksp", dependencies_type(), none())
            // "key=value" for the symbol processors
            .with_default("ksp_options", strings(), none())
            .with_default("ksp_version", Type::String, DEFAULT_KSP_VERSION)
            .with_default("kotlin_version", Type::String, DEFAULT_KOTLIN_VERSION);
    }

    return parameters.with_default("jdk", Type::String, jdk::DEFAULT_JAVA_VERSION)
        .with_default("directory", Type::String, ".")
        .with_default(
            "source_dirs",
            strings(),
            source_set_dirs(source_sets, source_kinds(sources)),
        )
        // output directories of generate_sources, compiled with the sources
        .with_default("generated_sources", strings(), none())
        // like sourceSets.main.resources.srcDirs in Gradle
        .with_default(
            "resource_dirs",
            strings(),
            source_set_dirs(source_sets, &["resources"]),
        )
        // "path in the jar=file or directory", like from(...) in processResources
        .with_default("resource_files", strings(), none())
        // "path in the jar=content", files the build writes
        .with_default("resource_texts", strings(), none())
        // "name=value": @name@ in text resources becomes value, like filter(ReplaceTokens)
        .with_default("resource_tokens", strings(), none());
}

/// build/classes/<kind>/<source set>, like Gradle names them
pub fn classes_kind(sources: Sources) -> &'static str {
    match sources {
        Sources::KotlinAndJava => return "kotlin",
        Sources::Java => return "java",
    }
}

/// The directories below src/<source set>/ the sources are in
pub fn source_kinds(sources: Sources) -> &'static [&'static str] {
    match sources {
        Sources::KotlinAndJava => return &["kotlin", "java"],
        Sources::Java => return &["java"],
    }
}

/// The `directory` argument below the project, which has to exist
pub fn module_directory(context: &BuildScriptContext, args: &Arguments) -> Result<PathBuf, String> {
    let directory = context.project_dir.join(args.get::<String>("directory")?);

    if !directory.is_dir() {
        return Err(format!("directory {} does not exist", directory.display()));
    }

    return Ok(directory);
}

/// The Kotlin and the Java files below the directories, in one walk through them
pub fn kotlin_and_java_sources(directories: &[PathBuf]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let extension_is =
        |file: &PathBuf, extension: &str| file.extension().is_some_and(|found| found == extension);
    return system::files_below(directories)
        .into_iter()
        .filter(|file| extension_is(file, "kt") || extension_is(file, "java"))
        .partition(|file| extension_is(file, "kt"));
}

/// javac into `output`, the Kotlin classes there on the classpath. Only the given
/// annotation processors run, none that libraries bring along.
pub fn compile_java(
    jdk: &Jdk,
    sources: &[PathBuf],
    classpath: &[PathBuf],
    output: &Path,
    release: &str,
    processing: Option<&AnnotationProcessing>,
) -> Result<(), String> {
    system::create_dir(output)?;
    let mut java_classpath = vec![output.to_path_buf()];
    java_classpath.extend(classpath.iter().cloned());

    let mut command = Command::new(jdk.tool_path("javac"));
    command.arg("-d")
        .arg(output)
        .args([
            "--release",
            release.trim_start_matches("1."),
            "-nowarn",
            "-Xlint:none",
            "-encoding",
            "UTF-8",
        ])
        .arg("-classpath")
        .arg(join_classpath(&java_classpath)?);

    match processing {
        Some(processing) if !processing.processor_path.is_empty() => {
            system::recreate_dir(processing.generated_dir)?;
            command.arg("-processorpath")
                .arg(join_classpath(processing.processor_path)?)
                .arg("-s")
                .arg(processing.generated_dir);
        }
        _ => {
            command.arg("-proc:none");
        }
    }

    command.args(sources);

    return system::run_command(&mut command, "javac");
}

/// Everything in `directories`, with `manifest` or without one
pub fn create_jar(
    jdk: &Jdk,
    jar_file: &Path,
    manifest: Option<&Path>,
    directories: &[&Path],
) -> Result<(), String> {
    system::create_parent_dir(jar_file)?;

    let mut command = Command::new(jdk.tool_path("jar"));

    match manifest {
        Some(manifest) => command.arg("cfm").arg(jar_file).arg(manifest),
        None => command.arg("cfM").arg(jar_file),
    };

    for directory in directories {
        command.arg("-C")
            .arg(directory)
            .arg(".");
    }

    return system::run_command(&mut command, &format!("creating {}", jar_file.display()));
}

impl ModuleBuildInputs {
    pub fn read(
        context: &BuildScriptContext,
        args: &Arguments,
        sources: Sources,
    ) -> Result<Self, String> {
        let kotlin = sources == Sources::KotlinAndJava;

        // Java modules have no Kotlin version of their own, kotlin("test") and the like get this one
        let kotlin_version = if kotlin {
            args.get("kotlin_version")?
        } else {
            DEFAULT_KOTLIN_VERSION.to_string()
        };

        let plugins = match kotlin {
            true => args.get::<Vec<String>>("plugins")?
                .iter()
                .map(|plugin| KotlinCompilerPlugin::find(plugin, &kotlin_version))
                .collect::<Result<Vec<_>, _>>()?,
            false => vec![],
        };

        let mut dependencies = Dependencies::read(args, "dependencies", &kotlin_version)?;
        let mut compile_only = Dependencies::read(args, "compile_only", &kotlin_version)?;
        dependencies.platforms.append(&mut compile_only.platforms);

        let repositories = maven::repositories(
            &args.get::<Vec<String>>("repositories")?,
            &context.project_dir,
        )?;

        let directory = module_directory(context, args)?;

        // generated sources are absolute paths, joining keeps them as they are
        let source_dirs = args.get::<Vec<String>>("source_dirs")?
            .iter()
            .chain(&args.get::<Vec<String>>("generated_sources")?)
            .map(|source_dir| directory.join(source_dir))
            .collect();

        return Ok(ModuleBuildInputs {
            name: args.get("name")?,
            sources,
            dependencies,
            compile_only,
            repositories,
            source_dirs,
            plugins,
            plugin_options: if kotlin {
                args.get("plugin_options")?
            } else {
                vec![]
            },
            kotlin_args: if kotlin {
                args.get("kotlin_args")?
            } else {
                vec![]
            },
            annotation_processors: Dependencies::read(
                args,
                "annotation_processors",
                &kotlin_version,
            )?,
            symbol_processors: if kotlin {
                SymbolProcessors::read(args, "ksp", &kotlin_version)?
            } else {
                SymbolProcessors::none()
            },
            resources: JavaResources::read(args, &directory)?,
            jdk: Jdk::resolve(
                &args.get::<String>("jdk")?,
                jdk::DEFAULT_JDK_DISTRIBUTION,
                &context.project_dir,
            )?,
            directory,
            kotlin_version,
        });
    }

    /// Compiler, plugins, processors and libraries: the Maven artifacts of this module and
    /// the modules below in one resolution so versions agree, plus kotlin-stdlib like Gradle
    pub fn resolve(&self, platform: TargetPlatform) -> Result<ResolvedDependencies, String> {
        let mut resolver = DependencyResolver::new(self.repositories.clone(), platform);
        // the compiler, its plugins and the processors run in the build, on the
        // JVM, whatever the module targets: Dagger needs Guava's jre variant
        let mut tools = DependencyResolver::new(self.repositories.clone(), TargetPlatform::Jvm);

        let (compiler_modules, compiler) = match self.sources {
            Sources::KotlinAndJava => {
                system::log(
                    &self.name,
                    &format!("Resolving dependencies (Kotlin {})", self.kotlin_version),
                );
                let compiler = MavenCoordinate::new(
                    "org.jetbrains.kotlin",
                    "kotlin-compiler-embeddable",
                    &self.kotlin_version,
                );

                let compiler_modules = tools.resolve_graph(&[compiler], &[])?;
                let compiler = tools.artifacts(&compiler_modules)?;
                (compiler_modules, compiler)
            }
            // javac comes with the JDK
            Sources::Java => {
                system::log(&self.name, "Resolving dependencies");
                (vec![], vec![])
            }
        };

        let mut plugins = vec![];
        let mut plugin_options = vec![];

        for plugin in &self.plugins {
            // the plugin and what it needs besides the compiler, which is there already
            let own_modules: Vec<MavenCoordinate> = tools.resolve_graph(&[plugin.coordinate.clone()], &[])?
                .into_iter()
                .filter(|module| {
                    !compiler_modules.iter().any(|known| {
                        known.group == module.group && known.artifact == module.artifact
                    })
                })
                .collect();

            plugins.extend(tools.artifacts(&own_modules)?);
            plugin_options.extend(plugin.options.iter().cloned());
        }

        plugin_options.extend(self.plugin_options.iter().cloned());

        let (roots, platforms) = self.runtime_dependencies();
        let libraries = resolver.resolve_with_platforms(&roots, &platforms)?;

        // compile only: whatever the runtime classpath does not have already
        let mut compile_only =
            resolver.resolve_with_platforms(&self.compile_only.libraries, &platforms)?;

        compile_only.retain(|file| !libraries.contains(file));

        for module in BuiltModule::transitive(&self.compile_only.modules) {
            compile_only.extend(module.jvm().classes.clone());
        }

        return Ok(ResolvedDependencies {
            compiler,
            plugins,
            plugin_options,
            libraries,
            compile_only,
            modules: BuiltModule::transitive(&self.dependencies.modules),
            // processors have classpaths of their own, like their configurations in Gradle
            annotation_processor_path: self
                .annotation_processors
                .resolve_processor_path(&mut tools, &self.kotlin_version)?,
            symbol_processors: self
                .symbol_processors
                .resolve(&mut tools, &self.kotlin_version)?,
        });
    }

    /// Maven roots and platforms of the runtime classpath: this module's, those
    /// of the modules it depends on, plugin runtimes and kotlin-stdlib
    pub fn runtime_dependencies(&self) -> (Vec<MavenCoordinate>, Vec<MavenCoordinate>) {
        let mut roots = self.exported_libraries();
        let mut platforms = self.dependencies.platforms.clone();

        for plugin in &self.plugins {
            roots.extend(plugin.runtime.iter().cloned());
        }

        for module in BuiltModule::transitive(&self.dependencies.modules) {
            roots.extend(module.jvm().libraries.iter().cloned());
            platforms.extend(module.jvm().platforms.iter().cloned());
        }

        return (roots, platforms);
    }

    /// What modules depending on this one get on their classpath: its libraries
    /// and, for Kotlin, the standard library like the Gradle plugin adds it
    fn exported_libraries(&self) -> Vec<MavenCoordinate> {
        let mut libraries = self.dependencies.libraries.clone();

        if self.sources == Sources::KotlinAndJava {
            libraries.push(MavenCoordinate::new(
                "org.jetbrains.kotlin",
                "kotlin-stdlib",
                &self.kotlin_version,
            ));
        }

        return libraries;
    }

    /// The runtime libraries as JSON report of the Gradle license plugin
    pub fn license_report(&self, platform: TargetPlatform) -> Result<String, String> {
        let mut resolver = DependencyResolver::new(self.repositories.clone(), platform);
        let (roots, platforms) = self.runtime_dependencies();
        let libraries = resolver.license_entries(&roots, &platforms)?;
        system::log(&self.name, &format!("License report of {} libraries", libraries.len()));

        return Ok(maven::license_report(&libraries).to_string());
    }

    /// What the outputs depend on: arguments, JDK, sources, `extra_inputs` like resources,
    /// the modules below and the newest SNAPSHOT builds. Kept in build/fingerprints/`name`.
    pub fn input_fingerprint(
        &self,
        name: &str,
        args: &Arguments,
        extra_inputs: &[PathBuf],
    ) -> Result<InputFingerprint, String> {
        let mut fingerprint =
            InputFingerprint::new(system::build_dir(&self.directory).join("fingerprints").join(name));

        fingerprint.add(args);
        fingerprint.add(&self.jdk.home);
        fingerprint.add_files(&self.source_dirs)?;
        fingerprint.add_files(extra_inputs)?;

        let snapshots: Vec<MavenCoordinate> = self.dependencies.libraries.iter()
            .chain(&self.compile_only.libraries)
            .chain(&self.annotation_processors.libraries)
            .filter(|coordinate| coordinate.is_snapshot())
            .cloned()
            .collect();

        if !snapshots.is_empty() {
            let mut resolver =
                DependencyResolver::new(self.repositories.clone(), TargetPlatform::Jvm);
            fingerprint.add(resolver.snapshot_poms(&snapshots)?);
        }

        for module in BuiltModule::transitive(&args.modules()) {
            fingerprint.add(&module.name);
            fingerprint.add_files(&[module.artifact.clone()])?;

            // a shared library of build_cpp counts by its file
            let ModuleOutput::Jvm(output) = &module.output else { continue };
            fingerprint.add(output);

            // apps compile the resources of their libraries
            if let Some(android) = &output.android {
                fingerprint.add_files(&android.res_dirs)?;
                fingerprint.add_files(&android.manifest.iter().cloned().collect::<Vec<_>>())?;
                fingerprint.add_files(&android.consumer_proguard_files)?;
            }
        }

        return Ok(fingerprint);
    }

    /// Says so in the log when it is
    pub fn is_up_to_date(
        &self,
        fingerprint: &InputFingerprint,
        outputs: &[&Path],
    ) -> Result<bool, String> {
        let up_to_date = fingerprint.is_up_to_date(outputs)?;

        if up_to_date {
            system::log(&self.name, "Up to date");
        }

        return Ok(up_to_date);
    }

    /// Remembers the fingerprint of the successful build and names what it produced
    pub fn finish(&self, fingerprint: &InputFingerprint, artifact: &Path) -> Result<(), String> {
        fingerprint.store()?;
        system::log(&self.name, &format!("Done: {}", artifact.display()));

        return Ok(());
    }

    pub fn built_module(
        &self,
        classes: Option<PathBuf>,
        android: Option<AndroidResources>,
        artifact: PathBuf,
        launch: Option<JvmLaunch>,
        tests: Vec<TestSuite>,
    ) -> Arc<BuiltModule> {
        return Arc::new(BuiltModule {
            name: self.name.clone(),
            directory: self.directory.clone(),
            artifact,
            modules: self.dependencies.modules.clone(),
            output: ModuleOutput::Jvm(JvmOutput {
                classes,
                libraries: self.exported_libraries(),
                platforms: self.dependencies.platforms.clone(),
                android,
                launch,
                native_library_dirs: self.dependencies.native_library_dirs(),
            }),
            tests,
        });
    }
}

impl Dependencies {
    /// The list argument `name`. Kotlin's own libraries may leave out the
    /// version, like kotlin("test") in Gradle: they get the compiler's.
    pub fn read(args: &Arguments, name: &str, kotlin_version: &str) -> Result<Self, String> {
        let mut dependencies = Dependencies::default();

        for value in args.get::<Vec<Value>>(name)? {
            match value {
                Value::String(notation) => match DependencyNotation::parse(&notation)? {
                    DependencyNotation::Library(coordinate) => {
                        let coordinate = with_kotlin_version(coordinate, kotlin_version);
                        dependencies.libraries.push(coordinate)
                    }
                    DependencyNotation::Platform(coordinate) => {
                        dependencies.platforms.push(coordinate)
                    }
                },
                Value::Module(module) => {
                    let module = module.wait()
                        .map_err(|_| format!("{} it depends on failed", module.function))?;

                    match &module.output {
                        ModuleOutput::Jvm(_) => dependencies.modules.push(module),
                        ModuleOutput::Native(output) if output.shared_library.is_some() => dependencies.native.push(module),
                        ModuleOutput::Native(_) => {
                            return Err(format!("{} is no shared library: JVM modules load those of build_cpp with shared = true", module.name));
                        }
                    }
                }
                other => {
                    return Err(format!(
                        "internal error: dependency of type {}",
                        other.type_of()
                    ));
                }
            }
        }

        return Ok(dependencies);
    }

    pub fn is_empty(&self) -> bool {
        return self.libraries.is_empty() && self.modules.is_empty();
    }

    /// java.library.path: the directories of the shared libraries, also those of the modules
    pub fn native_library_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = self.modules.iter()
            .flat_map(|module| module.jvm().native_library_dirs.clone())
            .collect();

        for module in &self.native {
            if let ModuleOutput::Native(output) = &module.output {
                dirs.extend(output.shared_library.as_ref().and_then(|library| library.parent()).map(Path::to_path_buf));
            }
        }

        dirs.dedup();
        return dirs;
    }

    /// For annotation or symbol processors: the Maven artifacts, and the jars
    /// of the modules with what they need to run, kotlin-stdlib included
    pub fn resolve_processor_path(
        &self,
        resolver: &mut DependencyResolver,
        kotlin_version: &str,
    ) -> Result<Vec<PathBuf>, String> {
        let modules = BuiltModule::transitive(&self.modules);
        let mut roots = self.libraries.clone();
        let mut platforms = self.platforms.clone();

        for module in &modules {
            roots.extend(module.jvm().libraries.iter().cloned());
            platforms.extend(module.jvm().platforms.iter().cloned());
        }

        if !modules.is_empty() {
            roots.push(MavenCoordinate::new(
                "org.jetbrains.kotlin",
                "kotlin-stdlib",
                kotlin_version,
            ));
        }

        let mut path = resolver.resolve_with_platforms(&roots, &platforms)?;
        path.extend(
            modules.iter()
                .filter_map(|module| module.jvm().classes.clone()),
        );

        return Ok(path);
    }
}

impl KotlinCompilerPlugin {
    /// By its short name ("compose") or its Maven coordinates. Plugins of
    /// org.jetbrains.kotlin may leave out the version, they get the compiler's.
    pub fn find(name_or_coordinate: &str, kotlin_version: &str) -> Result<Self, String> {
        if name_or_coordinate.contains(':') {
            let coordinate =
                with_kotlin_version(MavenCoordinate::parse(name_or_coordinate)?, kotlin_version);

            if coordinate.version.is_empty() {
                return Err(format!(
                    "the compiler plugin {name_or_coordinate} needs a version"
                ));
            }

            return Ok(KotlinCompilerPlugin {
                coordinate,
                runtime: None,
                options: vec![],
            });
        }

        let Some((name, artifact, runtime, options)) = KOTLIN_COMPILER_PLUGINS.iter()
            .find(|(known, ..)| *known == name_or_coordinate)
        else {
            let known: Vec<&str> = KOTLIN_COMPILER_PLUGINS.iter()
                .map(|(known, ..)| *known)
                .collect();
            return Err(format!(
                "unknown compiler plugin '{name_or_coordinate}' (known: {}, or Maven coordinates)",
                known.join(", ")
            ));
        };

        // before 2.0 the Compose compiler was released separately by Google
        if *name == "compose" && maven::compare_versions(kotlin_version, "2.0.0").is_lt() {
            return Err(format!(
                "the compose plugin needs Kotlin 2.0 or newer, not {kotlin_version}"
            ));
        }

        return Ok(KotlinCompilerPlugin {
            coordinate: MavenCoordinate::new("org.jetbrains.kotlin", artifact, kotlin_version),
            runtime: runtime.map(|runtime| {
                MavenCoordinate::new("org.jetbrains.kotlin", runtime, kotlin_version)
            }),
            options: options.iter().map(|option| option.to_string()).collect(),
        });
    }
}

impl Compilation<'_> {
    /// Into `output`, without classes of earlier builds. Returns the
    /// directories with the Java resources the symbol processors wrote.
    pub fn run(&self) -> Result<Vec<PathBuf>, String> {
        let build_dir = system::build_dir(&self.inputs.directory);
        let tests = self.variant == "test";

        let module_name = if tests {
            format!("{}_test", self.inputs.name)
        } else {
            self.inputs.name.clone()
        };

        let jvm_target = match self.target {
            TargetPlatform::Jvm => self.inputs.jdk.jvm_target(),
            TargetPlatform::Android => ANDROID_JVM_TARGET.to_string(),
        };

        let (mut kotlin_sources, mut java_sources) = kotlin_and_java_sources(self.source_dirs);

        if let (Sources::Java, Some(kotlin)) = (self.inputs.sources, kotlin_sources.first()) {
            return Err(format!(
                "{} is Kotlin, build_java compiles Java only: build the module with build_kotlin",
                kotlin.display()
            ));
        }

        let mut resource_dirs = vec![];

        if let Some(symbol_processors) = self.symbol_processors {
            let generated = SymbolProcessing {
                inputs: self.inputs,
                resolved: symbol_processors,
                source_dirs: self.source_dirs,
                module_name: &module_name,
                libraries: self.classpath,
                friends: self.friends,
                jvm_target: &jvm_target,
                // java.* comes from android.jar among the libraries
                jdk_home: (self.target == TargetPlatform::Jvm)
                    .then_some(self.inputs.jdk.home.as_path()),
                output_dir: build_dir.join("generated/ksp").join(self.variant),
                caches_dir: build_dir.join("ksp-caches").join(self.variant),
            }
            .run()?;

            kotlin_sources.extend(system::files_with_extension(&[generated.kotlin], "kt"));
            java_sources.extend(system::files_with_extension(&[generated.java], "java"));
            resource_dirs.push(generated.resources);
        }

        system::recreate_dir(self.output)?;
        let files = if tests { "test file(s)" } else { "file(s)" };
        if !kotlin_sources.is_empty() {
            system::log(
                &self.inputs.name,
                &format!("Compiling {} Kotlin {files}", kotlin_sources.len()),
            );

            let mut command = self
                .inputs
                .jdk
                .java_command(&[], &self.tools.compiler, KOTLIN_COMPILER_MAIN_CLASS)?;

            command
                // the standard library is already on the classpath
                .args(["-no-stdlib", "-no-reflect"])
                .arg("-module-name")
                .arg(&module_name)
                .arg("-classpath")
                .arg(join_classpath(self.classpath)?)
                .arg("-d")
                .arg(self.output)
                .arg("-jvm-target")
                .arg(&jvm_target);

            if self.target == TargetPlatform::Android {
                command.arg("-no-jdk");
            }

            for plugin in &self.tools.plugins {
                command.arg(format!("-Xplugin={}", plugin.display()));
            }

            for option in &self.tools.plugin_options {
                command.arg("-P").arg(option);
            }

            for friend in self.friends {
                command.arg(format!("-Xfriend-paths={}", friend.display()));
            }

            command.args(&self.inputs.kotlin_args)
                .args(&kotlin_sources)
                .args(&java_sources);
            system::run_command(&mut command, "compilation")?;
        }

        if !java_sources.is_empty() {
            system::log(
                &self.inputs.name,
                &format!("Compiling {} Java {files}", java_sources.len()),
            );

            let generated_dir = build_dir.join("generated/sources/annotationProcessor/java")
                .join(self.variant);

            let processing = AnnotationProcessing {
                processor_path: self.annotation_processors,
                generated_dir: &generated_dir,
            };

            compile_java(
                &self.inputs.jdk,
                &java_sources,
                self.classpath,
                self.output,
                &jvm_target,
                Some(&processing),
            )?;
        }

        return Ok(resource_dirs);
    }
}

/// Kotlin's own libraries without a version get the compiler's
fn with_kotlin_version(coordinate: MavenCoordinate, kotlin_version: &str) -> MavenCoordinate {
    if coordinate.group == "org.jetbrains.kotlin" && coordinate.version.is_empty() {
        return MavenCoordinate {
            version: kotlin_version.to_string(),
            ..coordinate
        };
    }

    return coordinate;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiler_plugins_by_short_name_or_coordinates() {
        let serialization = KotlinCompilerPlugin::find("serialization", "2.4.21").unwrap();
        assert_eq!(
            serialization.coordinate.to_string(),
            "org.jetbrains.kotlin:kotlin-serialization-compiler-plugin-embeddable:2.4.21"
        );
        let parcelize = KotlinCompilerPlugin::find("parcelize", "1.9.25").unwrap();
        assert_eq!(
            parcelize
                .runtime
                .map(|runtime| runtime.to_string())
                .as_deref(),
            Some("org.jetbrains.kotlin:kotlin-parcelize-runtime:1.9.25")
        );
        assert_eq!(
            KotlinCompilerPlugin::find("spring", "2.4.21")
                .unwrap()
                .options,
            ["plugin:org.jetbrains.kotlin.allopen:preset=spring"]
        );

        // Kotlin's own plugins get the compiler's version, others need one
        let power_assert = KotlinCompilerPlugin::find(
            "org.jetbrains.kotlin:kotlin-power-assert-compiler-plugin-embeddable",
            "2.4.21",
        )
        .unwrap();

        assert_eq!(power_assert.coordinate.version, "2.4.21");
        assert_eq!(
            KotlinCompilerPlugin::find(
                "dev.drewhamilton.poko:poko-compiler-plugin:0.23.2",
                "2.4.21"
            )
            .unwrap()
            .coordinate
            .version,
            "0.23.2"
        );
        assert!(
            KotlinCompilerPlugin::find("dev.drewhamilton.poko:poko-compiler-plugin", "2.4.21")
                .unwrap_err()
                .contains("needs a version")
        );

        assert!(
            KotlinCompilerPlugin::find("compose", "1.9.25")
                .unwrap_err()
                .contains("Kotlin 2.0")
        );
        assert!(
            KotlinCompilerPlugin::find("kapt", "2.4.21")
                .unwrap_err()
                .contains("known: compose, serialization")
        );
    }
}
