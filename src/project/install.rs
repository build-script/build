// build install: a library into the dependencies of a module, once it resolved for the module
use std::path::Path;

use crate::common::system;
use crate::jvm::{self, install::InstallTarget};
use crate::language::parser::{Parser, Script};
use crate::language::runtime::{FromValue, ParameterDefault, Runtime};
use crate::native;

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

    let calls = script::module_calls(&script);
    let call = chosen_module(&calls, module, script_file)?;

    if !call.has_parameter(parameter) {
        return Err(format!(
            "{} of {} takes no {parameter}",
            call.function, call.name
        ));
    }

    // relative paths in the script are resolved against the script's directory
    let project_dir = script_file.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));

    // C and C++ modules take vcpkg ports
    let library = match call.platform {
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
