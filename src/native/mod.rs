// C++: build_cpp compiles a module into a static library or a program, its libraries come from vcpkg,
// its compiler is the system's or zig. Every source compiles on its own, only when it changed.
mod compile;
mod toolchain;
mod vcpkg;

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::common::system;
use crate::common::up_to_date::InputFingerprint;
use crate::language::runtime::{
    Arguments, BuildScriptContext, BuiltModule, FunctionParameters, FunctionSignature, ModuleOutput, Platform,
    Type, Value, builtin,
};

use compile::Compilation;
use toolchain::Toolchain;

/// `use cpp`
pub const PLATFORMS: &[Platform] = &[Platform {
    name: "cpp",
    functions: cpp_builtins,
}];

/// The sources of a module that names none
const DEFAULT_SOURCES: &str = "src/**";

/// What modules depending on a C or C++ module need from it, and what `run` starts
#[derive(Debug, Hash)]
pub struct NativeOutput {
    /// -I and -D of its headers and of their libraries
    pub compile_flags: Vec<String>,
    /// its library and what that needs, in link order
    pub link_flags: Vec<String>,
    /// None for libraries
    pub program: Option<PathBuf>,
}

/// A dependency list: vcpkg ports and modules of the script
#[derive(Default)]
struct Dependencies {
    ports: Vec<String>,
    modules: Vec<Arc<BuiltModule>>,
}

fn cpp_builtins() -> Vec<(&'static str, Value)> {
    return vec![("build_cpp", builtin("build_cpp", signature(), build_module))];
}

fn signature() -> FunctionSignature {
    let strings = || Type::list(Type::String);
    let none = || Value::string_list(&[]);
    let dependencies = || Type::list(Type::Union(vec![Type::String, Type::Module]));

    return FunctionParameters::new()
        .required("name", Type::String)
        // vcpkg ports, like "fmt", and modules of the script
        .required("dependencies", dependencies())
        // a program `run` starts, else a static library
        .with_default("program", Type::Bool, false)
        // "system": cc and c++ of the system, "zig": downloaded once
        .with_default("toolchain", Type::String, "system")
        .with_default("c_standard", Type::String, "c17")
        .with_default("cpp_standard", Type::String, "c++20")
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
        .returns(Type::Module);
}

/// build/lib/lib<name>.a, with program = true build/bin/<name>
fn build_module(context: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
    let name: String = args.get("name")?;
    let directory = context.project_dir.join(args.get::<String>("directory")?);
    let build_dir = directory.join("build");
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
    let installed = vcpkg::install(&name, &dependencies.ports, &toolchain, &build_dir)?;
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
    let cpp_standard: String = args.get("cpp_standard")?;
    let compilation = Compilation {
        module_name: &name,
        toolchain: &toolchain,
        module_dir: &directory,
        source_patterns: &source_patterns,
        exclude_patterns: &exclude_patterns,
        flags,
        c_standard: &c_standard,
        cpp_standard: &cpp_standard,
        objects_dir,
    };

    let extensions = compile::EXTENSIONS.join(", .");

    // the default may match nothing: a library of only headers
    if source_patterns != [DEFAULT_SOURCES] && let Some(pattern) = compilation.unmatched_pattern() {
        return Err(format!("no .{extensions} file matches \"{pattern}\" of sources"));
    }

    // a library of only headers links nothing
    let header_only = compilation.sources().is_empty();

    if program && header_only {
        return Err(format!("{name} has no .{extensions} files, a program needs a main()"));
    }

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
    fingerprint.add(&link_flags);
    fingerprint.add_files(&[objects.clone(), module_artifacts].concat())?;

    // a library of only headers builds no file of its own
    let outputs: Vec<&Path> = match header_only {
        true => vec![&compile_commands],
        false => vec![&artifact, &compile_commands],
    };

    if !compiled && fingerprint.is_up_to_date(&outputs)? {
        system::log(&name, "Up to date");
    } else {
        match (program, header_only) {
            (true, _) => compile::link(&toolchain, &objects, &link_flags, &artifact)?,
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
        commands.extend(compilation.compile_commands());

        let commands = serde_json::to_string_pretty(&commands).map_err(|error| error.to_string())?;
        system::write_file(&compile_commands, &commands)?;

        fingerprint.store()?;

        match header_only {
            true => system::log(&name, "Done, only headers"),
            false => system::log(&name, &format!("Done: {}", artifact.display())),
        }
    }

    // modules depending on a library get its headers and link it before what it needs
    if !program && !header_only {
        link_flags.insert(0, artifact.display().to_string());
    }

    return Ok(Value::from(Arc::new(BuiltModule {
        name,
        directory,
        artifact: artifact.clone(),
        modules: dependencies.modules,
        output: ModuleOutput::Native(NativeOutput {
            compile_flags: [public_flags, compile_flags].concat(),
            link_flags,
            program: program.then_some(artifact.clone()),
        }),
        tests: vec![],
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

/// The flags, those that stand alone like -I and -D once: the 80 packages of abseil repeat them
fn without_repeated_flags(mut flags: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();

    flags.retain(|flag| {
        let stands_alone = flag.len() > 2 && ["-I", "-D", "-W"].iter().any(|prefix| flag.starts_with(prefix));
        return !stands_alone || seen.insert(flag.clone());
    });

    return flags;
}
