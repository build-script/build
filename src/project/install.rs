// build install: a library into the dependencies of a module, once it resolved for the module. A module of a
// plugin takes it in its libraries, in the version its resolve names.
use std::path::Path;

use crate::common::{lock, system};
use crate::jvm::{self, install::InstallTarget};
use crate::language::parser::{Parser, Script, StatementType};
use crate::language::runtime::{self, FromValue, ParameterDefault, Requests, Runtime};
use crate::{command, native};

use super::script::{self, DependencyChange, ModuleCall};

/// The flags of install, and the arguments of the build functions they put the library in
pub const DEPENDENCY_KINDS: &[(&str, &str)] = &[
    ("--compile-only", "compile_only"),
    ("--test", "test_dependencies"),
    ("--android-test", "android_test_dependencies"),
    ("--processor", "annotation_processors"),
    ("--ksp", "ksp"),
];

/// Where the library goes without a flag
pub const DEPENDENCIES: &str = "dependencies";

/// Where the libraries of a module of a plugin go, without a flag and with --test
const LIBRARIES: &str = "libraries";
const TEST_LIBRARIES: &str = "test_libraries";

/// `notation` is group:artifact, with a version to take that one
pub fn install(
    notation: &str,
    script_file: &Path,
    module: Option<&str>,
    parameter: &str,
) -> Result<(), String> {
    let source = system::read_file(script_file)?;

    let script = Parser::new(&source)
        .parse_script()
        .map_err(|error| format!("{}: {}", script_file.display(), error.variant))?;

    // relative paths in the script are resolved against the script's directory
    let project_dir = script_file.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));

    // the plugins at the commit project.lock keeps, their build functions tell their modules
    let uses_plugins = script.statements.iter().any(|statement| match &statement.variant {
        StatementType::UsePlugin(_) => true,
        StatementType::Use(platform) => !runtime::platforms().any(|known| known.name == platform.name),
        StatementType::Expression(_) => false,
    });

    let plugin_functions = match uses_plugins {
        true => {
            lock::open(script_file, false)?;
            Runtime::new(project_dir)
                .with_script(script_file, &source)
                .plugin_build_functions(&script)?
        }
        false => vec![],
    };

    let calls = script::module_calls_with(&script, &plugin_functions);
    let call = chosen_module(&calls, module, script_file)?;

    let parameter = match (call.plugin, parameter) {
        (true, DEPENDENCIES) => LIBRARIES,
        (true, "test_dependencies") => TEST_LIBRARIES,
        _ => parameter,
    };

    if !call.has_parameter(parameter) {
        return Err(format!(
            "{} of {} takes no {parameter}",
            call.function, call.name
        ));
    }

    // C and C++ modules take vcpkg ports
    let library = match call.platform {
        _ if call.plugin => plugin_library(notation, &script, &source, script_file, project_dir, call)?,
        "c" | "cpp" => native::checked_port(notation)?,
        _ => {
            let repositories: Vec<String> = argument_value(&script, call, "repositories", project_dir)?;
            let jdk: String = argument_value(&script, call, "jdk", project_dir)?;

            let target = InstallTarget {
                module: &call.name,
                repositories: &repositories,
                project_dir,
                android: call.platform == "android",
                jdk: &jdk,
            };

            jvm::install::checked_library(notation, &target)?.to_string()
        }
    };

    let edit = script::with_dependency(&source, &script, call, parameter, &library)?;

    // what goes into the file has to be a script still
    if let Err(error) = Parser::new(&edit.script).parse_script() {
        return Err(format!(
            "install would break {}: {}",
            script_file.display(),
            error.variant
        ));
    }

    let what = match &edit.change {
        DependencyChange::Present => {
            println!("{library} is in the {parameter} of {} already", call.name);
            return Ok(());
        }
        DependencyChange::Added => format!("Added {library} to the {parameter} of {}", call.name),
        DependencyChange::Replaced(previous) => {
            format!(
                "Replaced {previous} by {library} in the {parameter} of {}",
                call.name
            )
        }
    };

    system::write_file(script_file, &edit.script)?;

    let sharing: Vec<&str> = calls.iter()
        .filter(|other| other.name != call.name)
        .filter(|other| {
            edit.variable.is_some() && other.variable(parameter) == edit.variable.as_deref()
        })
        .map(|other| other.name.as_str())
        .collect();

    match (&edit.variable, sharing.is_empty()) {
        (Some(variable), false) => {
            println!(
                "{what}: in {variable}, which {} use as well",
                sharing.join(", ")
            );
        }
        _ => println!("{what}"),
    }

    return Ok(());
}

/// What a module of a plugin takes for `notation`: what the resolve of its command prints, the newest version
/// without one. The script runs for it, but builds nothing.
fn plugin_library(
    notation: &str,
    script: &Script,
    source: &str,
    script_file: &Path,
    project_dir: &Path,
    call: &ModuleCall,
) -> Result<String, String> {
    let mut runtime = Runtime::new(project_dir)
        .with_script(script_file, source)
        .with_requests(Requests::Nothing);

    runtime.run(script).map_err(|error| runtime.describe(&error))?;

    // the command the plugin builds the module with, named like the module
    let module = runtime.module_named(&call.name);
    let arguments = module.as_ref()
        .filter(|module| module.function == "build_command")
        .and_then(|module| module.arguments.as_ref());

    let resolved = match arguments {
        Some(arguments) => command::resolve(runtime.context(), arguments, notation)?,
        None => None,
    };

    let name = command::library_name(notation);

    return match resolved {
        Some(resolved) => Ok(resolved),
        None if name.len() < notation.len() => Ok(notation.to_string()),
        None => Err(format!(
            "{} of {} cannot find the newest version of {name}, its plugin has no resolve: name one, like {name}@<version>",
            call.function, call.name
        )),
    };
}

/// The module `--module` names, or the only one
fn chosen_module<'a, 'b>(
    calls: &'b [ModuleCall<'a>],
    module: Option<&str>,
    script_file: &Path,
) -> Result<&'b ModuleCall<'a>, String> {
    let names = calls.iter()
        .map(|call| call.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    match (module, calls) {
        (Some(module), _) => {
            return calls.iter()
                .find(|call| call.name == module)
                .ok_or_else(|| {
                    format!(
                        "{} has no module {module}, its modules are {names}",
                        script_file.display()
                    )
                });
        }
        (None, [call]) => return Ok(call),
        (None, []) => return Err(format!("{} builds no module", script_file.display())),
        (None, _) => {
            return Err(format!(
                "{} has several modules, name one with --module: {names}",
                script_file.display()
            ));
        }
    }
}

/// What the call passes for `parameter`, without building, or the default of the function
fn argument_value<T: FromValue>(
    script: &Script,
    call: &ModuleCall,
    parameter: &str,
    project_dir: &Path,
) -> Result<T, String> {
    let default = || {
        call.signature.parameters.fixed.iter()
            .find(|param| param.name == parameter)
            .and_then(|param| match &param.default {
                ParameterDefault::Value(value) => Some(value.clone()),
                _ => None,
            })
    };

    let value = match call.argument(parameter) {
        Some(expression) => Runtime::new(project_dir)
            .evaluate_unbuilt(
                script,
                call.statement_start,
                jvm::maven::builtins(),
                expression,
            )
            .map_err(|error| {
                format!(
                    "install cannot read the {parameter} of {} without building: {error}",
                    call.name
                )
            })?,
        None => default().ok_or_else(|| format!("{} has no {parameter}", call.name))?,
    };

    return T::from_value(&value).ok_or_else(|| {
        format!(
            "the {parameter} of {} are no {}",
            call.name,
            std::any::type_name::<T>()
        )
    });
}

#[cfg(all(test, unix))]
mod tests {
    use crate::common::test_support::ScratchDir;

    use super::*;

    /// A plugin whose modules take libraries: resolve names version 9.<asked> of them, build writes ran.txt
    const PLUGIN: &str = r#"
build_thing = fn(name: str, libraries: list<str> = list(), dependencies: list<module> = list()) -> module {
    build_command(name, list("sh", "-c", "echo ran > ran.txt"), libraries = libraries, dependencies = dependencies,
        resolve = list("sh", "-c", "echo {name}@9.{version}"))
}

build_plain = fn(name: str, libraries: list<str> = list()) -> module {
    build_command(name, list("true"), libraries = libraries)
}

// resolve prints the version alone, like npm view
build_versions = fn(name: str, libraries: list<str> = list()) -> module {
    build_command(name, list("true"), libraries = libraries, resolve = list("sh", "-c", "echo 3.{version}"))
}
"#;

    #[test]
    fn modules_of_plugins_take_libraries_in_the_version_their_resolve_names() {
        let _lock = lock::TESTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let directory = ScratchDir::new("install-plugin");
        let project = system::canonical_path(&directory).unwrap();
        let script = project.join("project.build");
        system::write_file(&project.join("plug.build"), PLUGIN).unwrap();
        system::write_file(&script, "use \"./plug.build\"\n\nbuild_thing(\"m\")\n").unwrap();
        let read = || system::read_file(&script).unwrap();

        install("x", &script, None, DEPENDENCIES).unwrap();
        assert!(read().contains(r#"build_thing("m", libraries = list("x@9.latest"))"#), "{}", read());
        assert!(!project.join("ran.txt").exists(), "install builds nothing");

        install("y@2", &script, None, DEPENDENCIES).unwrap();
        install("x@1", &script, Some("m"), DEPENDENCIES).unwrap();
        assert!(read().contains(r#"libraries = list("x@9.1", "y@9.2")"#), "{}", read());

        let error = install("z", &script, None, "test_dependencies").unwrap_err();
        assert_eq!(error, "build_thing of m takes no test_libraries");

        // without resolve only a version the command line names
        system::write_file(&script, "use \"./plug.build\"\n\nbuild_plain(\"p\")\n").unwrap();
        let error = install("z", &script, None, DEPENDENCIES).unwrap_err();
        assert!(error.contains("cannot find the newest version of z, its plugin has no resolve"), "{error}");
        install("z@3", &script, None, DEPENDENCIES).unwrap();
        assert!(read().contains(r#"build_plain("p", libraries = list("z@3"))"#), "{}", read());

        system::write_file(&script, "use \"./plug.build\"\n\nbuild_versions(\"v\")\n").unwrap();
        install("@scope/w", &script, None, DEPENDENCIES).unwrap();
        assert!(read().contains(r#"build_versions("v", libraries = list("@scope/w@3.latest"))"#), "{}", read());

        // a script that asks for a module while install runs it: the module is not built
        system::write_file(&script, "use \"./plug.build\"\n\nm = build_thing(\"m\")\nprint(m)\n").unwrap();
        install("x", &script, None, DEPENDENCIES).unwrap();
        assert!(!project.join("ran.txt").exists(), "install builds nothing, also what the script prints");

        lock::close();
    }
}
