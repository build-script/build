// Unit tests of JVM modules, of build_kotlin and of Android modules: compiled against the module, run on the
// JUnit Platform, which runs JUnit 4 and 5, kotlin.test, Kotest, TestNG and every other framework with an engine
use std::path::PathBuf;

use crate::common::{system, terminal};
use crate::language::runtime::{
    Arguments, BuiltModule, FunctionParameters, ModuleTests, TestOutcome, TestRun, TestSuite, Type,
    Value,
};

use super::jdk::join_classpath;
use super::ksp::{ResolvedSymbolProcessors, SymbolProcessors};
use super::maven::{DependencyResolver, MavenCoordinate, TargetPlatform};
use super::module::{
    Compilation, Dependencies, ModuleBuildInputs, Sources, classes_kind, dependencies_type,
    kotlin_and_java_sources, source_kinds, source_set_dirs,
};

/// JUnit when the tests do not decide: the last JUnit 5 runs on every JDK from
/// 8 on, and its vintage engine for JUnit 4 is not deprecated yet

/// The resources of the tests, next to their sources
pub const TEST_RESOURCES_DIR: &str = "src/test/resources";
const DEFAULT_JUNIT_VERSION: &str = "5.14.4";
const CONSOLE_LAUNCHER_MAIN_CLASS: &str = "org.junit.platform.console.ConsoleLauncher";

/// Engines added when the tests use their framework without them. Kotest, jqwik and
/// Cucumber are engines themselves, and any engine in test_dependencies runs as well.
const ENGINES: &[Engine] = &[
    Engine {
        // kotlin-test-junit5 brings the Jupiter API, but only once it is resolved
        frameworks: &[
            ("org.junit.jupiter", "junit-jupiter"),
            ("org.jetbrains.kotlin", "kotlin-test-junit5"),
        ],
        engine: ("org.junit.jupiter", "junit-jupiter-engine"),
        version: EngineVersion::Junit,
        platform: None,
    },
    Engine {
        frameworks: &[("junit", "junit")],
        engine: ("org.junit.vintage", "junit-vintage-engine"),
        version: EngineVersion::Junit,
        platform: None,
    },
    Engine {
        frameworks: &[("org.testng", "testng")],
        engine: ("org.junit.support", "testng-engine"),
        version: EngineVersion::Fixed("1.1.0"),
        platform: None,
    },
    // Spek publishes its engines apart from its API, in the same version
    Engine {
        frameworks: &[("org.jetbrains.spek", "spek-api")],
        engine: ("org.jetbrains.spek", "spek-junit-platform-engine"),
        version: EngineVersion::Framework,
        platform: Some("1.0.0"),
    },
    Engine {
        frameworks: &[("org.spekframework.spek2", "spek-dsl-jvm")],
        engine: ("org.spekframework.spek2", "spek-runner-junit5"),
        version: EngineVersion::Framework,
        platform: None,
    },
];

/// What the unit tests of a module are compiled and run against
pub struct UnitTests<'a> {
    pub inputs: &'a ModuleBuildInputs,
    pub args: &'a Arguments,
    /// class directories and jars of the module, the tests see their internal declarations
    pub classes: Vec<PathBuf>,
    /// whose variants of the libraries the tests get
    pub platform: TargetPlatform,
    /// of Android modules: compiled against, at runtime after the libraries like with
    /// Gradle. Its methods throw "Stub!", tests that call them need a fake.
    pub android_jar: Option<PathBuf>,
}

/// Like testImplementation and its kin in Gradle
pub fn unit_test_parameters(
    parameters: FunctionParameters,
    sources: Sources,
) -> FunctionParameters {
    let mut parameters = parameters.with_default(
        "test_dependencies",
        dependencies_type(),
        Value::string_list(&[]),
    )
        .with_default(
            "test_source_dirs",
            Type::list(Type::String),
            source_set_dirs(&["test"], source_kinds(sources)),
        )
        // like testAnnotationProcessor and kspTest in Gradle, ksp_options and ksp_version apply to both
        .with_default(
            "test_annotation_processors",
            dependencies_type(),
            Value::string_list(&[]),
        );

    if sources == Sources::KotlinAndJava {
        parameters =
            parameters.with_default("test_ksp", dependencies_type(), Value::string_list(&[]));
    }

    // for frameworks without a JUnit Platform engine: started with the test classpath, its exit code decides
    return parameters.optional("test_main_class", Type::String);
}

impl UnitTests<'_> {
    /// Tests that cannot be resolved or compiled have failed already, the others run once the script is built
    pub fn prepare(&self) -> TestSuite {
        let tests = self.compile().unwrap_or_else(|error| {
            ModuleTests::Known(TestOutcome::Failed {
                counts: None,
                error,
            })
        });
        return TestSuite {
            source_set: "test",
            tests,
        };
    }

    /// Not compiled when neither the tests nor what they test changed since they last passed
    fn compile(&self) -> Result<ModuleTests, String> {
        let (inputs, args) = (self.inputs, self.args);

        let test_source_dirs: Vec<PathBuf> = args.get::<Vec<String>>("test_source_dirs")?
            .iter()
            .map(|dir| inputs.directory.join(dir))
            .collect();

        let test_resources_dir = inputs.directory.join(TEST_RESOURCES_DIR);
        let (kotlin_tests, java_tests) = kotlin_and_java_sources(&test_source_dirs);

        if kotlin_tests.is_empty() && java_tests.is_empty() {
            system::log(&inputs.name, "No tests");
            return Ok(ModuleTests::Known(TestOutcome::NoTests));
        }

        let test_dependencies =
            Dependencies::read(args, "test_dependencies", &inputs.kotlin_version)?;

        let test_symbol_processors = match inputs.sources {
            Sources::KotlinAndJava => {
                SymbolProcessors::read(args, "test_ksp", &inputs.kotlin_version)?
            }
            Sources::Java => SymbolProcessors::none(),
        };

        let test_annotation_processors =
            Dependencies::read(args, "test_annotation_processors", &inputs.kotlin_version)?;

        let test_main_class = args.get::<Option<String>>("test_main_class")?;

        let mut test_inputs = test_source_dirs.clone();
        test_inputs.push(test_resources_dir.clone());

        test_inputs.extend(self.classes.iter().cloned());
        let fingerprint =
            inputs.input_fingerprint(&format!("{}-tests", inputs.name), args, &test_inputs)?;

        if fingerprint.is_up_to_date(&[])? {
            system::log(&inputs.name, "Tests up to date");
            return Ok(ModuleTests::Known(TestOutcome::UpToDate));
        }

        // the compiler and the plugins of the module, its libraries come with the classes
        let tools = inputs.resolve_tools(&mut DependencyResolver::new(inputs.repositories.clone(), TargetPlatform::Jvm))?;
        let on_junit_platform = test_main_class.is_none();

        let test_libraries = resolve_test_libraries(
            inputs,
            &test_dependencies,
            &test_symbol_processors,
            &test_annotation_processors,
            on_junit_platform,
            self.platform,
        )?;

        // the module, the modules it and its tests depend on, then the libraries
        let mut classpath = self.classes.clone();
        let mut modules = BuiltModule::transitive(&inputs.dependencies.modules);
        modules.extend(BuiltModule::transitive(&test_dependencies.modules));
        classpath.extend(
            modules.iter()
                .filter_map(|module| module.jvm().classes.clone()),
        );
        classpath.extend(test_libraries.files);
        let mut compile_classpath: Vec<PathBuf> = self.android_jar.iter()
            .cloned()
            .collect();
        compile_classpath.extend(classpath.iter().cloned());

        let build_dir = system::build_dir(&inputs.directory);

        let test_classes = build_dir.join("classes")
            .join(classes_kind(inputs.sources))
            .join("test");

        let generated_resources = Compilation {
            inputs,
            tools: &tools,
            variant: "test",
            source_dirs: &test_source_dirs,
            classpath: &compile_classpath,
            // tests see the internal declarations of the module, like with the Gradle plugin
            friends: &self.classes,
            symbol_processors: test_libraries.symbol_processors.as_ref(),
            annotation_processors: &test_libraries.annotation_processor_path,
            target: self.platform,
            output: &test_classes,
        }
        .run()?;

        let mut runtime_classpath = vec![test_classes.clone()];
        runtime_classpath.extend(
            [test_resources_dir].into_iter()
                .chain(generated_resources)
                .filter(|dir| dir.is_dir()),
        );
        runtime_classpath.extend(classpath);
        runtime_classpath.extend(self.android_jar.iter().cloned());

        let reports_dir = build_dir.join("test-results");

        // the shared C and C++ libraries of the module and of the tests
        let library_dirs = [inputs.dependencies.native_library_dirs(), test_dependencies.native_library_dirs()].concat();
        let java_options = match library_dirs.is_empty() {
            true => vec![],
            false => vec![format!("-Djava.library.path={}", join_classpath(&library_dirs)?.to_string_lossy())],
        };

        let (mut command, runner) = match test_main_class {
            Some(main_class) => (
                inputs.jdk.java_command(&java_options, &runtime_classpath, &main_class)?,
                main_class,
            ),
            None => {
                let version = test_libraries
                    .console_launcher_version
                    .ok_or("the JUnit Platform console launcher was not resolved")?;

                let mut command = inputs
                    .jdk
                    .java_command(&java_options, &runtime_classpath, CONSOLE_LAUNCHER_MAIN_CLASS)?;

                command.args(console_launcher_options(&version))
                    .arg(format!("--scan-classpath={}", test_classes.display()))
                    .arg(format!("--reports-dir={}", reports_dir.display()));

                // its tree has colors only when the log has
                if !terminal::colors() {
                    command.arg("--disable-ansi-colors");
                }

                (command, format!("JUnit Platform {version}"))
            }
        };

        // like Gradle, tests run in the module directory
        command.current_dir(&inputs.directory);

        return Ok(ModuleTests::Run(Box::new(TestRun::new(
            runner,
            &command,
            reports_dir,
            fingerprint,
        ))));
    }
}

/// A test framework and the engine that runs its tests on the JUnit Platform
struct Engine {
    /// group and artifact prefix of what the tests use
    frameworks: &'static [(&'static str, &'static str)],
    engine: (&'static str, &'static str),
    version: EngineVersion,
    /// the only platform version the engine runs on
    platform: Option<&'static str>,
}

enum EngineVersion {
    /// the JUnit version of the platform the tests run on
    Junit,
    /// the version of the framework
    Framework,
    Fixed(&'static str),
}

pub struct TestLibraries {
    pub files: Vec<PathBuf>,
    /// None when the tests do not run on the JUnit Platform
    console_launcher_version: Option<String>,
    /// jars of the javac processors for the tests and what they need
    annotation_processor_path: Vec<PathBuf>,
    symbol_processors: Option<ResolvedSymbolProcessors>,
}

/// The libraries of the module and its tests in one resolution, so versions agree, plus
/// what the JUnit Platform and the test processors need. Of aars only their jars count.
pub fn resolve_test_libraries(
    inputs: &ModuleBuildInputs,
    test_dependencies: &Dependencies,
    symbol_processors: &SymbolProcessors,
    annotation_processors: &Dependencies,
    on_junit_platform: bool,
    platform: TargetPlatform,
) -> Result<TestLibraries, String> {
    let (mut roots, mut platforms) = inputs.runtime_dependencies();
    roots.extend(test_dependencies.libraries.iter().cloned());
    platforms.extend(test_dependencies.platforms.iter().cloned());

    let mut resolver = DependencyResolver::new(inputs.repositories.clone(), platform);

    if on_junit_platform {
        let found = resolver.resolve_graph(&roots, &platforms)?;
        roots.extend(junit_platform_libraries(&found, &inputs.kotlin_version));
    }

    let coordinates = resolver.resolve_graph(&roots, &platforms)?;

    let console_launcher_version = coordinates.iter()
        .find(|coordinate| {
            coordinate.group == "org.junit.platform"
                && coordinate.artifact == "junit-platform-console"
        })
        .map(|coordinate| coordinate.version.clone());

    return Ok(TestLibraries {
        files: jars_of_aars(resolver.artifacts(&coordinates)?)?,
        console_launcher_version,
        annotation_processor_path: annotation_processors
            .resolve_processor_path(&mut resolver, &inputs.kotlin_version)?,
        symbol_processors: symbol_processors.resolve(&mut resolver, &inputs.kotlin_version)?,
    });
}

/// The console launcher in the platform version the tests need, and the engines
/// of the frameworks the tests use without them
fn junit_platform_libraries(
    resolved: &[MavenCoordinate],
    kotlin_version: &str,
) -> Vec<MavenCoordinate> {
    let binding = kotlin_test_binding(resolved, kotlin_version);
    let used: Vec<&MavenCoordinate> = resolved.iter()
        .chain(&binding)
        .collect();

    let find = |group: &str, artifact_prefix: &str| {
        used.iter()
            .copied()
            .find(|coordinate| {
                coordinate.group == group && coordinate.artifact.starts_with(artifact_prefix)
            })
    };

    // each engine with the framework that needs it
    let engines: Vec<(&Engine, &MavenCoordinate)> = ENGINES.iter()
        .filter_map(|engine| {
            engine.frameworks.iter()
                .find_map(|(group, artifact)| find(group, artifact))
                .map(|framework| (engine, framework))
        })
        .collect();

    // the platform an engine needs, else the JUnit the tests use, else the platform the frameworks were built for
    let jupiter_version =
        find("org.junit.jupiter", "junit-jupiter").map(|coordinate| coordinate.version.clone());

    let platform_version = engines.iter()
        .find_map(|(engine, _)| engine.platform)
        .map(str::to_string)
        .or_else(|| jupiter_version.as_deref().map(platform_version_of_junit))
        .or_else(|| {
            find("org.junit.platform", "junit-platform-")
                .map(|coordinate| coordinate.version.clone())
        })
        .unwrap_or_else(|| platform_version_of_junit(DEFAULT_JUNIT_VERSION));

    let junit_version =
        jupiter_version.unwrap_or_else(|| junit_version_of_platform(&platform_version));

    let mut libraries = vec![MavenCoordinate::new(
        "org.junit.platform",
        "junit-platform-console",
        &platform_version,
    )];

    libraries.extend(binding.clone());

    for (engine, framework) in engines {
        let (group, artifact) = engine.engine;

        if resolved.iter()
            .any(|coordinate| coordinate.group == group && coordinate.artifact == artifact)
        {
            continue;
        }

        let version = match engine.version {
            EngineVersion::Junit => junit_version.clone(),
            EngineVersion::Framework => framework.version.clone(),
            EngineVersion::Fixed(version) => version.to_string(),
        };

        libraries.push(MavenCoordinate::new(group, artifact, &version));
    }

    return libraries;
}

/// kotlin("test") annotates and asserts with the framework it is bound to, the
/// Kotlin Gradle plugin picks one like this. Next to Kotest or Spek it only asserts.
fn kotlin_test_binding(
    resolved: &[MavenCoordinate],
    kotlin_version: &str,
) -> Option<MavenCoordinate> {
    let has = |group: &str, artifact: &str| {
        resolved.iter()
            .any(|coordinate| coordinate.group == group && coordinate.artifact == artifact)
    };

    let has_group = |group: &str| resolved.iter().any(|coordinate| coordinate.group == group);

    let bound = [
        "kotlin-test-junit",
        "kotlin-test-junit5",
        "kotlin-test-testng",
    ]
    .iter()
    .any(|artifact| has("org.jetbrains.kotlin", artifact));

    if !has("org.jetbrains.kotlin", "kotlin-test") || bound {
        return None;
    }

    let binding = if has_group("org.junit.jupiter") {
        "kotlin-test-junit5"
    } else if has("junit", "junit") {
        "kotlin-test-junit"
    } else if has("org.testng", "testng") {
        "kotlin-test-testng"
    } else if has_group("io.kotest")
        || has_group("org.jetbrains.spek")
        || has_group("org.spekframework.spek2")
    {
        return None;
    } else {
        "kotlin-test-junit5"
    };

    return Some(MavenCoordinate::new(
        "org.jetbrains.kotlin",
        binding,
        kotlin_version,
    ));
}

/// The aars among the files become their classes.jar and libs/*.jar, extracted once into the cache
fn jars_of_aars(files: Vec<PathBuf>) -> Result<Vec<PathBuf>, String> {
    let mut jars = vec![];

    for file in files {
        if file.extension().is_none_or(|extension| extension != "aar") {
            jars.push(file);
            continue;
        }

        let target =
            system::kept_in_cache("extracted", &file, |target| system::extract(&file, target))?;
        jars.extend(Some(target.join("classes.jar")).filter(|classes| classes.is_file()));
        jars.extend(system::files_with_extension(&[target.join("libs")], "jar"));
    }

    return Ok(jars);
}

/// JUnit 5 numbers the platform 1.x for Jupiter 5.x, JUnit 6 numbers both alike
fn junit_version_of_platform(platform_version: &str) -> String {
    match platform_version.strip_prefix("1.") {
        Some(rest) => return format!("5.{rest}"),
        None => return platform_version.to_string(),
    }
}

fn platform_version_of_junit(junit_version: &str) -> String {
    match junit_version.strip_prefix("5.") {
        Some(rest) => return format!("1.{rest}"),
        None => return junit_version.to_string(),
    }
}

/// As far as the version knows them
fn console_launcher_options(console_launcher_version: &str) -> Vec<&'static str> {
    let mut parts = console_launcher_version.split('.')
        .map(|part| part.parse::<u32>().unwrap_or(0));

    let version = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));

    let mut options = vec![];

    if version >= (1, 10) {
        options.push("execute");
    }

    if version >= (1, 4) {
        options.push("--disable-banner");
    }

    options.push("--details=tree");

    return options;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coordinates(notations: &[&str]) -> Vec<MavenCoordinate> {
        return notations.iter()
            .map(|notation| MavenCoordinate::parse(notation).unwrap())
            .collect();
    }

    fn added_libraries(resolved: &[&str]) -> Vec<String> {
        return junit_platform_libraries(&coordinates(resolved), "2.4.21")
            .iter()
            .map(|root| root.to_string())
            .collect();
    }

    #[test]
    fn engines_are_added_for_the_test_frameworks() {
        // JUnit 4 runs on the vintage engine
        assert_eq!(
            added_libraries(&["junit:junit:4.13.2"]),
            [
                "org.junit.platform:junit-platform-console:1.14.4",
                "org.junit.vintage:junit-vintage-engine:5.14.4"
            ]
        );

        // JUnit 5 gets its engine in the version of its API, also JUnit 6
        assert_eq!(
            added_libraries(&["org.junit.jupiter:junit-jupiter-api:5.10.2"]),
            [
                "org.junit.platform:junit-platform-console:1.10.2",
                "org.junit.jupiter:junit-jupiter-engine:5.10.2"
            ]
        );
        assert_eq!(
            added_libraries(&["org.junit.jupiter:junit-jupiter-api:6.1.3"]),
            [
                "org.junit.platform:junit-platform-console:6.1.3",
                "org.junit.jupiter:junit-jupiter-engine:6.1.3"
            ]
        );

        // kotlin("test") alone is bound to JUnit 5, next to JUnit 4 to that, next to Kotest it only asserts
        assert_eq!(
            added_libraries(&["org.jetbrains.kotlin:kotlin-test:2.4.21"])[1..],
            [
                "org.jetbrains.kotlin:kotlin-test-junit5:2.4.21",
                "org.junit.jupiter:junit-jupiter-engine:5.14.4"
            ]
        );
        assert_eq!(
            added_libraries(&[
                "org.jetbrains.kotlin:kotlin-test:2.4.21",
                "junit:junit:4.13.2"
            ])[1..],
            [
                "org.jetbrains.kotlin:kotlin-test-junit:2.4.21",
                "org.junit.vintage:junit-vintage-engine:5.14.4"
            ]
        );
        assert_eq!(
            added_libraries(&[
                "org.jetbrains.kotlin:kotlin-test:2.4.21",
                "io.kotest:kotest-runner-junit5-jvm:6.2.6"
            ]),
            ["org.junit.platform:junit-platform-console:1.14.4"]
        );

        // Kotest is an engine itself and runs on the platform it brings
        assert_eq!(
            added_libraries(&[
                "io.kotest:kotest-runner-junit5-jvm:6.2.6",
                "org.junit.platform:junit-platform-engine:1.13.4"
            ]),
            ["org.junit.platform:junit-platform-console:1.13.4"]
        );

        assert_eq!(
            added_libraries(&["org.testng:testng:7.12.0"])[1..],
            ["org.junit.support:testng-engine:1.1.0"]
        );

        // Spek publishes its engines apart from the API, Spek 1 only runs on platform 1.0
        assert_eq!(
            added_libraries(&["org.jetbrains.spek:spek-api:1.1.5"]),
            [
                "org.junit.platform:junit-platform-console:1.0.0",
                "org.jetbrains.spek:spek-junit-platform-engine:1.1.5"
            ]
        );
        assert_eq!(
            added_libraries(&["org.spekframework.spek2:spek-dsl-jvm:2.0.19"])[1..],
            ["org.spekframework.spek2:spek-runner-junit5:2.0.19"]
        );

        // engines the tests bring are not added again
        assert_eq!(
            added_libraries(&[
                "org.junit.jupiter:junit-jupiter-api:5.10.2",
                "org.junit.jupiter:junit-jupiter-engine:5.10.2"
            ]),
            ["org.junit.platform:junit-platform-console:1.10.2"]
        );
        assert_eq!(
            added_libraries(&[
                "junit:junit:4.13.2",
                "org.junit.vintage:junit-vintage-engine:5.9.3"
            ]),
            ["org.junit.platform:junit-platform-console:1.14.4"]
        );
    }

    #[test]
    fn the_console_launcher_gets_the_options_it_knows() {
        assert_eq!(console_launcher_options("1.0.0"), ["--details=tree"]);
        assert_eq!(
            console_launcher_options("1.6.2"),
            ["--disable-banner", "--details=tree"]
        );
        assert_eq!(
            console_launcher_options("1.14.4"),
            ["execute", "--disable-banner", "--details=tree"]
        );
        assert_eq!(
            console_launcher_options("6.1.3"),
            ["execute", "--disable-banner", "--details=tree"]
        );
        assert_eq!(junit_version_of_platform("1.14.3"), "5.14.3");
        assert_eq!(platform_version_of_junit("6.1.3"), "6.1.3");
    }
}
