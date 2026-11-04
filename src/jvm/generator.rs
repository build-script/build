// generate_sources: code that tools from Maven write before a module is compiled,
// like the code generation plugins of Gradle (SQLDelight, Wire, jOOQ, OpenAPI)
use std::path::PathBuf;

use crate::common::system;
use crate::common::up_to_date::InputFingerprint;
use crate::language::runtime::{
    Arguments, BuildScriptContext, FunctionParameters, Type, Value, builtin,
};

use super::jdk::{self, Jdk};
use super::maven::{self, DependencyResolver, MavenCoordinate, TargetPlatform};
use super::module::{compile_java, module_directory};

/// stands for the output directory in the arguments of a tool
const OUTPUT_PLACEHOLDER: &str = "{output}";

pub fn builtins() -> Vec<(&'static str, Value)> {
    return vec![(
        "generate_sources",
        builtin(
            "generate_sources",
            FunctionParameters::new()
                .required("name", Type::String)
                .required("tool", Type::list(Type::String))
                .required("repositories", Type::list(Type::String))
                .required("main_class", Type::String)
                .with_default("args", Type::list(Type::String), Value::string_list(&[]))
                .with_default("inputs", Type::list(Type::String), Value::string_list(&[]))
                .with_default(
                    "tool_sources",
                    Type::list(Type::String),
                    Value::string_list(&[]),
                )
                .with_default("jdk", Type::String, jdk::DEFAULT_JAVA_VERSION)
                .with_default("directory", Type::String, ".")
                .returns(Type::String),
            generate_sources,
        ),
    )];
}

/// Runs a tool that writes sources into build/generated/sources/<name>, returns that
/// directory for generated_sources. The tool runs in `directory`, inputs and args are
/// relative to it, tool_sources to the build script.
fn generate_sources(context: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
    let name: String = args.get("name")?;
    let main_class: String = args.get("main_class")?;
    let directory = module_directory(context, args)?;
    let output = system::build_dir(&directory).join("generated/sources").join(&name);

    let inputs: Vec<PathBuf> = args.get::<Vec<String>>("inputs")?
        .iter()
        .map(|input| directory.join(input))
        .collect();

    // build logic of the project, like buildSrc in Gradle: next to the build script
    let tool_sources: Vec<PathBuf> = args.get::<Vec<String>>("tool_sources")?
        .iter()
        .map(|source| context.project_dir.join(source))
        .collect();

    let mut fingerprint = InputFingerprint::for_module(&directory, &format!("generator-{name}"));

    fingerprint.add(args);
    fingerprint.add_files(&inputs)?;
    fingerprint.add_files(&tool_sources)?;

    if fingerprint.reports_up_to_date(&name, &[&output])? {
        return Ok(output.display().to_string().into());
    }

    system::log(&name, &format!("Generating sources with {main_class}"));

    let jdk = Jdk::resolve(
        &args.get::<String>("jdk")?,
        jdk::DEFAULT_JDK_DISTRIBUTION,
        &context.project_dir,
    )?;

    let tool = args.get::<Vec<String>>("tool")?
        .iter()
        .map(|notation| MavenCoordinate::parse(notation))
        .collect::<Result<Vec<_>, _>>()?;

    let repositories = maven::repositories(
        &args.get::<Vec<String>>("repositories")?,
        &context.project_dir,
    )?;

    let mut classpath = DependencyResolver::new(repositories, TargetPlatform::Jvm)
        .resolve_with_platforms(&tool, &[])?;

    // a library without a command line comes with a small driver in the project
    if !tool_sources.is_empty() {
        let tool_classes = system::build_dir(&directory).join("generated/tools").join(&name);
        system::remove_dir(&tool_classes)?;
        compile_java(
            &jdk,
            &tool_sources,
            &classpath,
            &tool_classes,
            &jdk.jvm_target(),
            None,
        )?;
        classpath.insert(0, tool_classes);
    }

    system::recreate_dir(&output)?;
    let output_arg = output.display().to_string();
    let mut command = jdk.java_command(&[], &classpath, &main_class)?;
    command.args(
        args.get::<Vec<String>>("args")?
            .iter()
            .map(|arg| arg.replace(OUTPUT_PLACEHOLDER, &output_arg)),
    )
        // paths in the arguments are relative to the directory
        .current_dir(&directory);
    system::run_command(&mut command, &main_class)?;

    fingerprint.store_done(&name, &output)?;

    return Ok(output_arg.into());
}
