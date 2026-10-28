// C and C++: build_c and build_cpp compile a module into a static library or a program, its libraries
// come from vcpkg, its compiler is the system's or zig. Every source compiles on its own, only when it
// changed.
mod compile;
pub mod templates;
mod toolchain;
mod vcpkg;

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use crate::common::system;
use crate::common::up_to_date::InputFingerprint;
use crate::language::runtime::{
    Arguments, BuildScriptContext, BuiltModule, FunctionParameters, FunctionSignature, ModuleOutput,
    ModuleTests, Platform, TestCounts, TestOutcome, TestRun, TestRunner, TestSuite, Type, Value, builtin,
};

use compile::{Compilation, Language};
use toolchain::Toolchain;

pub use vcpkg::checked_port;

/// `use c` and `use cpp`
pub const PLATFORMS: &[Platform] = &[
    Platform {
        name: "c",
        functions: c_builtins,
    },
    Platform {
        name: "cpp",
        functions: cpp_builtins,
    },
];

/// The sources and tests of a module that names no other
const DEFAULT_SOURCES: &str = "src/**";
const DEFAULT_TEST_SOURCES: &str = "test/**";

/// vcpkg port, name in the log, the package of its main(), arguments for a JUnit report at {report}
type TestFramework = (&'static str, &'static str, Option<&'static str>, &'static [&'static str]);

const TEST_FRAMEWORKS: &[TestFramework] = &[
    ("gtest", "GoogleTest", Some("gtest_main"), &["--gtest_output=xml:{report}"]),
    // the console for people, the report for the summary
    ("catch2", "Catch2", Some("catch2-with-main"), &["--reporter", "console", "--reporter", "junit::out={report}"]),
    ("doctest", "doctest", None, &["--reporters=junit", "--out={report}"]),
];

/// What modules depending on a C or C++ module need from it, and what `run` starts
#[derive(Debug, Hash)]
pub struct NativeOutput {
    /// -I and -D of its headers and of their libraries
    pub compile_flags: Vec<String>,
    /// its library and what that needs, in link order
    pub link_flags: Vec<String>,
    /// None for libraries
    pub program: Option<PathBuf>,
    /// C++ is in it: what links it links with the C++ compiler
    pub cpp: bool,
}

/// How the tests of a module are linked and run
struct TestSetup<'a> {
    framework: Option<&'a TestFramework>,
    /// the framework's main(), for tests without one
    main_flags: Vec<String>,
    /// patterns of test sources that are a program each
    programs: Vec<String>,
    /// after those of the framework
    arguments: Vec<String>,
}

/// A dependency list: vcpkg ports and modules of the script
#[derive(Default)]
struct Dependencies {
    ports: Vec<String>,
    modules: Vec<Arc<BuiltModule>>,
}

fn c_builtins() -> Vec<(&'static str, Value)> {
    let build_c = builtin("build_c", signature(Language::C), |context, args| build_module(context, args, Language::C));
    return vec![("build_c", build_c)];
}

fn cpp_builtins() -> Vec<(&'static str, Value)> {
    let build_cpp = builtin("build_cpp", signature(Language::Cpp), |context, args| build_module(context, args, Language::Cpp));
    return vec![("build_cpp", build_cpp)];
}

/// Those of build_c and build_cpp, which has a cpp_standard
fn signature(language: Language) -> FunctionSignature {
    let strings = || Type::list(Type::String);
    let none = || Value::string_list(&[]);
    let dependencies = || Type::list(Type::Union(vec![Type::String, Type::Module]));

    let mut parameters = FunctionParameters::new()
        .required("name", Type::String)
        // vcpkg ports, like "fmt", and modules of the script
        .required("dependencies", dependencies())
        // a program `run` starts, else a static library
        .with_default("program", Type::Bool, false)
        // "system": cc and c++ of the system, "zig": downloaded once
        .with_default("toolchain", Type::String, "system")
        .with_default("c_standard", Type::String, "c17")
        // "NAME" or "NAME=value"
        .with_default("defines", strings(), none())
        // also for the modules depending on it, like SPDLOG_COMPILED_LIB
        .with_default("public_defines", strings(), none())
        .with_default("flags", strings(), none())
        // like "-framework", "Cocoa", modules depending on the library link them too
        .with_default("link_flags", strings(), none())
        .with_default("directory", Type::String, ".")
        // globs relative to the module, like "*.c" or "examples/simple.cpp"
        .with_default("sources", strings(), Value::string_list(&[DEFAULT_SOURCES]))
        // globs relative to the module, like "testes/**"
        .with_default("exclude", strings(), none())
        // the headers of modules depending on it, private headers stay among the sources
        .with_default("include_dirs", strings(), Value::string_list(&["include"]))
        .with_default("test_dependencies", dependencies(), none())
        .with_default("test_sources", strings(), Value::string_list(&[DEFAULT_TEST_SOURCES]))
        .with_default("test_exclude", strings(), none())
        // test sources that are a program each, with the other test sources: "test/*-test.cc"
        .with_default("test_programs", strings(), none())
        // like "--order", "decl" for Catch2
        .with_default("test_arguments", strings(), none());

    if language == Language::Cpp {
        parameters = parameters.with_default("cpp_standard", Type::String, "c++20");
    }

    return parameters.returns(Type::Module);
}

/// build/lib/lib<name>.a, with program = true build/bin/<name>
fn build_module(context: &BuildScriptContext, args: &Arguments, language: Language) -> Result<Value, String> {
    let name: String = args.get("name")?;
    let directory = context.project_dir.join(args.get::<String>("directory")?);
    let build_dir = system::build_dir(&directory);
    let program: bool = args.get("program")?;
    let toolchain = Toolchain::named(&args.get::<String>("toolchain")?)?;
    let release = context.release;

    let module_paths = |parameter: &str| -> Result<Vec<PathBuf>, String> {
        let paths = args.get::<Vec<String>>(parameter)?;
        return Ok(paths.iter().map(|path| directory.join(path)).collect());
    };

    let include_dirs = module_paths("include_dirs")?;
    let source_patterns: Vec<String> = args.get("sources")?;
    let exclude_patterns: Vec<String> = args.get("exclude")?;
    // with the sources the private headers
    let source_roots = compile::source_roots(&directory, &source_patterns);

    // what the modules depending on it see too
    let mut public_flags: Vec<String> = include_dirs.iter()
        .map(|dir| format!("-I{}", dir.display()))
        .collect();
    public_flags.extend(args.get::<Vec<String>>("public_defines")?.iter().map(|define| format!("-D{define}")));

    let dependencies = Dependencies::read(args, "dependencies")?;
    let test_dependencies = Dependencies::read(args, "test_dependencies")?;

    // one install for the module and its tests
    let ports = [dependencies.ports.clone(), test_dependencies.ports.clone()].concat();
    let installed = vcpkg::install(&name, &ports, &toolchain, &build_dir)?;
    let (compile_flags, mut link_flags) = dependencies.flags(installed.as_ref(), release);
    link_flags.extend(args.get::<Vec<String>>("link_flags")?);

    let mut flags = public_flags.clone();
    flags.extend(source_roots.iter().map(|dir| format!("-I{}", dir.display())));
    flags.extend(args.get::<Vec<String>>("defines")?.iter().map(|define| format!("-D{define}")));

    match release {
        true => flags.extend(["-O2".to_string(), "-DNDEBUG".to_string()]),
        false => flags.extend(["-O0".to_string(), "-g".to_string()]),
    }

    flags.extend(args.get::<Vec<String>>("flags")?);
    flags.extend(compile_flags.iter().cloned());
    let flags = without_repeated_flags(flags);

    // of their own: modules may share a directory
    let objects_dir = build_dir.join("obj")
        .join(&name)
        .join(if release { "release" } else { "debug" });
    let c_standard: String = args.get("c_standard")?;
    let cpp_standard: String = match language {
        Language::C => String::new(),
        Language::Cpp => args.get("cpp_standard")?,
    };
    let compilation = Compilation {
        module_name: &name,
        language,
        toolchain: &toolchain,
        module_dir: &directory,
        source_patterns: &source_patterns,
        exclude_patterns: &exclude_patterns,
        flags,
        c_standard: &c_standard,
        cpp_standard: &cpp_standard,
        objects_dir: objects_dir.clone(),
        tests: false,
    };

    // a module that names its sources names its tests too, modules may share a directory
    let test_sources: Vec<String> = args.get("test_sources")?;
    let test_source_patterns = match source_patterns != [DEFAULT_SOURCES] && test_sources == [DEFAULT_TEST_SOURCES] {
        true => vec![],
        false => test_sources,
    };
    let test_exclude_patterns: Vec<String> = args.get("test_exclude")?;

    // the tests see the headers next to them
    let (test_compile_flags, test_link_flags) = test_dependencies.flags(installed.as_ref(), release);
    let mut test_flags = compilation.flags.clone();
    test_flags.extend(compile::source_roots(&directory, &test_source_patterns).iter().map(|dir| format!("-I{}", dir.display())));
    test_flags.extend(test_compile_flags);
    let test_flags = without_repeated_flags(test_flags);

    let test_compilation = Compilation {
        flags: test_flags,
        source_patterns: &test_source_patterns,
        exclude_patterns: &test_exclude_patterns,
        objects_dir: objects_dir.join("test"),
        tests: true,
        ..compilation
    };

    let extensions = language.extensions().join(", .");

    // the defaults may match nothing: a library of only headers, a module without tests
    let checked = [("sources", &compilation, DEFAULT_SOURCES), ("test_sources", &test_compilation, DEFAULT_TEST_SOURCES)];

    for (parameter, compilation, default) in checked {
        if compilation.source_patterns == [default] {
            continue;
        }

        if let Some(pattern) = compilation.unmatched_pattern() {
            return Err(format!("no .{extensions} file matches \"{pattern}\" of {parameter}"));
        }
    }

    // a library of only headers links nothing
    let header_only = compilation.sources().is_empty();

    if program && header_only {
        return Err(format!("{name} has no .{extensions} files, a program needs a main()"));
    }

    // C modules link with the C compiler, unless C++ is among what they link
    let links_cpp = |modules: &[Arc<BuiltModule>]| {
        return BuiltModule::transitive(modules)
            .iter()
            .any(|module| matches!(&module.output, ModuleOutput::Native(output) if output.cpp));
    };
    let cpp = language == Language::Cpp || links_cpp(&dependencies.modules);
    let test_cpp = cpp || links_cpp(&test_dependencies.modules);

    let artifact = match (program, header_only) {
        (true, _) => build_dir.join("bin").join(&name),
        (false, true) => include_dirs.first().cloned().unwrap_or(directory.clone()),
        (false, false) => build_dir.join("lib").join(format!("lib{name}.a")),
    };

    // every build checks every source, a source compiles again when it or one of its headers changed
    let (objects, compiled) = compilation.run()?;
    let compile_commands = build_dir.join("compile_commands.json");
    let module_artifacts: Vec<PathBuf> =
        dependencies.modules.iter()
            .map(|module| module.artifact.clone())
            .collect();

    let mut fingerprint = InputFingerprint::new(build_dir.join("fingerprints").join(&name));
    fingerprint.add(args);
    fingerprint.add(release);
    fingerprint.add(&test_compilation.flags);
    fingerprint.add(&link_flags);
    fingerprint.add_files(&[objects.clone(), module_artifacts].concat())?;

    // the headers need not be there: a module of only tests has none
    let outputs: Vec<&Path> = match header_only {
        true => vec![&compile_commands],
        false => vec![&artifact, &compile_commands],
    };

    if !compiled && fingerprint.is_up_to_date(&outputs)? {
        system::log(&name, "Up to date");
    } else {
        match (program, header_only) {
            (true, _) => compile::link(&toolchain, cpp, &objects, &link_flags, &artifact)?,
            (false, true) => {}
            (false, false) => compile::archive(&toolchain, &objects, &artifact)?,
        }

        // those of other modules in the directory stay
        let own_objects = build_dir.join("obj").join(&name);
        let mut commands: Vec<serde_json::Value> = fs::read_to_string(&compile_commands)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        commands.retain(|command| {
            return !command["output"].as_str().is_some_and(|output| Path::new(output).starts_with(&own_objects));
        });
        commands.extend([compilation.compile_commands(), test_compilation.compile_commands()].concat());

        let commands = serde_json::to_string_pretty(&commands).map_err(|error| error.to_string())?;
        system::write_file(&compile_commands, &commands)?;

        fingerprint.store()?;

        match header_only {
            true => system::log(&name, "Done, only headers"),
            false => system::log(&name, &format!("Done: {}", artifact.display())),
        }
    }

    let mut tests = vec![];

    if context.tests.includes(&name) {
        let library = (!program && !header_only).then_some(artifact.as_path());
        let link_flags = [link_flags.clone(), test_link_flags].concat();
        let ports = &test_dependencies.ports;
        let framework = TEST_FRAMEWORKS.iter().find(|(port, ..)| ports.iter().any(|known| vcpkg::port_and_features(known).0 == *port));

        let setup = TestSetup {
            framework,
            main_flags: match (framework, installed.as_ref()) {
                (Some((_, _, Some(main), _)), Some(installed)) => installed.pkg_config(&[main.to_string()], release).1,
                _ => vec![],
            },
            programs: args.get("test_programs")?,
            arguments: args.get("test_arguments")?,
        };

        let outcome = unit_tests(&test_compilation, &compilation, library, test_cpp, link_flags, &setup)
            .unwrap_or_else(|error| ModuleTests::Known(TestOutcome::Failed { counts: None, error }));

        tests.push(TestSuite {
            source_set: "test",
            tests: outcome,
        });
    }

    // modules depending on a library get its headers and link it before what it needs
    if !program && !header_only {
        link_flags.insert(0, artifact.display().to_string());
    }

    return Ok(Value::from(Arc::new(BuiltModule {
        name,
        directory,
        artifact: artifact.clone(),
        modules: [dependencies.modules, test_dependencies.modules].concat(),
        output: ModuleOutput::Native(NativeOutput {
            compile_flags: [public_flags, compile_flags].concat(),
            link_flags,
            program: program.then_some(artifact.clone()),
            cpp,
        }),
        tests,
    })));
}

impl Dependencies {
    fn read(args: &Arguments, name: &str) -> Result<Self, String> {
        let mut dependencies = Dependencies::default();

        for value in args.get::<Vec<Value>>(name)? {
            match value {
                Value::String(port) => dependencies.ports.push(port),
                Value::Module(module) => {
                    let module = module.wait()
                        .map_err(|_| format!("{} it depends on failed", module.function))?;

                    if !matches!(module.output, ModuleOutput::Native(_)) {
                        let name = &module.name;
                        return Err(format!("{name} is no C or C++ module, build_cpp depends on those"));
                    }

                    dependencies.modules.push(module);
                }
                other => return Err(format!("internal error: dependency of type {}", other.type_of())),
            }
        }

        return Ok(dependencies);
    }

    /// -I and -D, and what to link: of the modules, then of the ports
    fn flags(&self, installed: Option<&vcpkg::Installed>, release: bool) -> (Vec<String>, Vec<String>) {
        let (mut compile_flags, mut link_flags) = match installed {
            Some(installed) => installed.flags(&self.ports, release),
            None => (vec![], vec![]),
        };

        for module in &self.modules {
            if let ModuleOutput::Native(output) = &module.output {
                compile_flags.extend(output.compile_flags.iter().cloned());
                link_flags.splice(0..0, output.link_flags.iter().cloned());
            }
        }

        return (compile_flags, link_flags);
    }
}

/// The tests in a program of their own, with the library, or with the objects of a program but the
/// one with main(). Every test source `test_programs` matches is a program, with the other test sources.
fn unit_tests(
    tests: &Compilation,
    module: &Compilation,
    library: Option<&Path>,
    cpp: bool,
    link_flags: Vec<String>,
    setup: &TestSetup,
) -> Result<ModuleTests, String> {
    let sources = tests.sources();

    if sources.is_empty() {
        return Ok(ModuleTests::Known(TestOutcome::NoTests));
    }

    tests.run()?;

    let tested: Vec<PathBuf> = match library {
        Some(library) => vec![library.to_path_buf()],
        None => {
            let sources = module.sources();
            sources.iter()
                .filter(|source| !defines_main(source))
                .map(|source| module.object(source))
                .collect()
        }
    };

    let (separate, shared): (Vec<&PathBuf>, Vec<&PathBuf>) =
        sources.iter().partition(|source| tests.matches_any(&setup.programs, source));

    let name = module.module_name;
    let programs: Vec<(String, Vec<&PathBuf>)> = match separate.is_empty() {
        true => vec![(format!("{name}-test"), shared.clone())],
        false => separate.iter()
            .map(|source| {
                let stem = source.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy();
                return (format!("{name}-{stem}"), [vec![*source], shared.clone()].concat());
            })
            .collect(),
    };

    let mut runs = vec![];

    for (program, sources) in programs {
        let mut objects: Vec<PathBuf> = sources.iter()
            .map(|source| tests.object(source))
            .collect();
        objects.extend(tested.iter().cloned());

        let mut link_flags = link_flags.clone();

        if !sources.iter().any(|source| defines_main(source)) {
            link_flags.splice(0..0, setup.main_flags.iter().cloned());
        }

        let run = test_program(tests, module, &program, cpp, &objects, &link_flags, setup)?;
        runs.push((program, run));
    }

    if runs.len() == 1 {
        return Ok(runs.remove(0).1);
    }

    return Ok(ModuleTests::Run(Box::new(TestPrograms(runs))));
}

/// Linked into build/bin/<program>, not again while neither it nor what it tests changed since it passed
fn test_program(
    tests: &Compilation,
    module: &Compilation,
    program: &str,
    cpp: bool,
    objects: &[PathBuf],
    link_flags: &[String],
    setup: &TestSetup,
) -> Result<ModuleTests, String> {
    let build_dir = system::build_dir(module.module_dir);
    let executable = build_dir.join("bin").join(program);

    let mut fingerprint = InputFingerprint::new(build_dir.join("fingerprints").join("tests").join(program));
    fingerprint.add(link_flags);
    fingerprint.add(&setup.arguments);
    fingerprint.add_files(objects)?;

    if fingerprint.is_up_to_date(&[&executable])? {
        system::log(module.module_name, &format!("Tests of {program} up to date"));
        return Ok(ModuleTests::Known(TestOutcome::UpToDate));
    }

    compile::link(tests.toolchain, cpp, objects, link_flags, &executable)?;

    let reports_dir = build_dir.join("test-results").join(program);
    let mut command = Command::new(&executable);
    command.current_dir(module.module_dir);

    let runner = match setup.framework {
        Some((port, runner, _, arguments)) => {
            let report = reports_dir.join(format!("TEST-{port}.xml"))
                .display()
                .to_string();
            command.args(arguments.iter().map(|argument| argument.replace("{report}", &report)));
            format!("{runner}: {program}")
        }
        None => program.to_string(),
    };

    command.args(&setup.arguments);

    let run = TestRun::new(runner, &command, reports_dir, fingerprint);
    return Ok(ModuleTests::Run(Box::new(run)));
}

/// The test programs of a module, one after another, summed up
#[derive(Debug)]
struct TestPrograms(Vec<(String, ModuleTests)>);

impl TestRunner for TestPrograms {
    fn run(&self, module: &str) -> TestOutcome {
        let mut counts: Option<TestCounts> = None;
        let mut errors = vec![];
        let mut ran = false;

        for (program, tests) in &self.0 {
            let (program_counts, error) = match tests.run(module) {
                TestOutcome::Passed(program_counts) => (program_counts, None),
                TestOutcome::Failed { counts, error } => (counts, Some(format!("{program}: {error}"))),
                TestOutcome::NoTests | TestOutcome::UpToDate => continue,
            };

            ran = true;
            errors.extend(error);

            if let Some(program_counts) = program_counts {
                let total = counts.get_or_insert_with(TestCounts::default);
                total.tests += program_counts.tests;
                total.failed += program_counts.failed;
                total.skipped += program_counts.skipped;
            }
        }

        if !errors.is_empty() {
            return TestOutcome::Failed { counts, error: errors.join(", ") };
        }

        return match ran {
            true => TestOutcome::Passed(counts),
            false => TestOutcome::UpToDate,
        };
    }
}

/// The flags, those that stand alone like -I and -D once: the 80 packages of abseil repeat them
fn without_repeated_flags(mut flags: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();

    flags.retain(|flag| {
        let stands_alone = flag.len() > 2 && ["-I", "-D", "-W"].iter().any(|prefix| flag.starts_with(prefix));
        return !stands_alone || seen.insert(flag.clone());
    });

    return flags;
}

/// A source with `int main(`, which a test program brings itself
fn defines_main(source: &Path) -> bool {
    let text = fs::read_to_string(source).unwrap_or_default();
    return text.contains("int main(") || text.contains("int main (");
}
