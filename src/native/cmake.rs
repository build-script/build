// Libraries outside vcpkg: build_cmake configures, builds and installs CMake projects with the CMake and
// Ninja of vcpkg, system_library takes an installed one, like the LLVM of Homebrew. The modules depending
// on them get the headers and the libraries, or what a tool like llvm-config names.
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use crate::common::system::{self, HostOperatingSystem};
use crate::common::up_to_date::InputFingerprint;
use crate::language::runtime::{
    Arguments, BuildScriptContext, BuiltModule, FunctionParameters, FunctionSignature, ModuleOutput, Type, Value,
};

use super::{NativeOutput, vcpkg};

pub fn build_cmake_signature() -> FunctionSignature {
    let strings = || Type::list(Type::String);
    let none = || Value::string_list(&[]);

    return FunctionParameters::new()
        .required("name", Type::String)
        // the directory with CMakeLists.txt, like git(...) + "/llvm"
        .required("source", Type::String)
        // -D of CMake: "LLVM_ENABLE_PROJECTS=clang;lld"
        .with_default("options", strings(), none())
        // what modules depending on it link, like "LLVMSupport", else every static library it installed
        .with_default("libraries", strings(), none())
        // a tool of the installation and its arguments, its output the flags: "bin/llvm-config", "--cxxflags"
        .with_default("compile_flags_from", strings(), none())
        .with_default("link_flags_from", strings(), none())
        .with_default("link_flags", strings(), none())
        .returns(Type::Module);
}

pub fn system_library_signature() -> FunctionSignature {
    let strings = || Type::list(Type::String);
    let none = || Value::string_list(&[]);

    return FunctionParameters::new()
        .required("name", Type::String)
        // a tool and its arguments, its output the flags: "llvm-config", "--cxxflags"
        .required("compile_flags_from", strings())
        .required("link_flags_from", strings())
        .with_default("link_flags", strings(), none())
        .returns(Type::Module);
}

/// A source of git() builds once on the machine, in the cache, named after its commit and options. Other
/// sources build into build/cmake/<name>/ of the project. The build goes to build/, the installation to install/.
pub fn build_cmake(context: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
    let name: String = args.get("name")?;
    let options: Vec<String> = args.get("options")?;
    let source = context.project_dir.join(args.get::<String>("source")?);

    if !source.join("CMakeLists.txt").is_file() {
        return Err(format!("{} has no CMakeLists.txt", source.display()));
    }

    let cmake = vcpkg::tool("cmake")?;
    let ninja = vcpkg::tool("ninja")?;

    // CMake reads the compilers and their flags from the environment
    let environment: Vec<(&str, Option<String>)> = ["CC", "CXX", "AR", "CFLAGS", "CXXFLAGS", "LDFLAGS"].into_iter()
        .map(|variable| (variable, std::env::var(variable).ok()))
        .collect();

    // a checkout of git() is the same on every machine: its commit is in its path
    let build_dir = match source.starts_with(system::cache_dir("git")) {
        true => {
            let mut hasher = DefaultHasher::new();
            (&source, &options, &environment, &cmake).hash(&mut hasher);
            system::cache_dir("cmake").join(format!("{name}-{:016x}", hasher.finish()))
        }
        false => system::build_dir(&context.project_dir).join("cmake").join(&name),
    };

    let (binary_dir, install_dir) = (build_dir.join("build"), build_dir.join("install"));
    // what modules depending on it see change: written when the build changed something
    let stamp = build_dir.join("stamp");

    let mut fingerprint = InputFingerprint::new(build_dir.join("configured"));
    fingerprint.add(&source);
    fingerprint.add(&options);
    fingerprint.add(&environment);
    fingerprint.add(&cmake);

    // release builds, a debug LLVM is huge
    if !fingerprint.is_up_to_date(&[&binary_dir.join("CMakeCache.txt")])? {
        system::log(&name, "Configuring with CMake");

        let mut configure = Command::new(&cmake);
        configure.arg("-G")
            .arg("Ninja")
            .arg("-S")
            .arg(&source)
            .arg("-B")
            .arg(&binary_dir);
        configure.arg(format!("-DCMAKE_MAKE_PROGRAM={}", ninja.display()));
        configure.arg("-DCMAKE_BUILD_TYPE=Release");
        configure.arg(format!("-DCMAKE_INSTALL_PREFIX={}", install_dir.display()));
        configure.args(options.iter().map(|option| format!("-D{option}")));
        system::run_command_quietly(&mut configure, "configuring with CMake")?;

        fingerprint.store()?;
    }

    let dry_run = Command::new(&ninja)
        .arg("-C")
        .arg(&binary_dir)
        .arg("-n")
        .output()
        .map_err(|error| format!("could not start ninja: {error}"))?;

    if String::from_utf8_lossy(&dry_run.stdout).contains("no work to do") && stamp.is_file() {
        system::log(&name, "Up to date");
    } else {
        system::log(&name, "Building with CMake and Ninja");
        system::run_command(Command::new(&cmake).arg("--build").arg(&binary_dir), "building with CMake")?;
        system::run_command_quietly(Command::new(&cmake).arg("--install").arg(&binary_dir), "installing with CMake")?;

        let built = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        system::write_file(&stamp, &format!("{}", built.as_nanos()))?;
        system::log(&name, &format!("Done: {}", install_dir.display()));
    }

    let lib_dir = install_dir.join("lib");
    let libraries: Vec<String> = args.get("libraries")?;
    let compile_command: Vec<String> = args.get("compile_flags_from")?;
    let link_command: Vec<String> = args.get("link_flags_from")?;

    let compile_flags = match compile_command.is_empty() {
        true => vec![format!("-I{}", install_dir.join("include").display())],
        false => flags_from(&compile_command, Some(&install_dir))?,
    };

    let mut link_flags: Vec<String> =
        libraries.iter()
            .map(|library| lib_dir.join(format!("lib{library}.a")).display().to_string())
            .collect();

    if !link_command.is_empty() {
        link_flags.extend(flags_from(&link_command, Some(&install_dir))?);
    }

    if link_flags.is_empty() {
        link_flags = system::files_with_extension(&[lib_dir.clone()], "a")
            .iter()
            .filter(|library| library.parent() == Some(lib_dir.as_path()))
            .map(|library| library.display().to_string())
            .collect();

        // GNU ld takes static libraries in order, these need each other
        if system::host_operating_system()? == HostOperatingSystem::Linux {
            link_flags.insert(0, "-Wl,--start-group".to_string());
            link_flags.push("-Wl,--end-group".to_string());
        }
    }

    link_flags.extend(args.get::<Vec<String>>("link_flags")?);

    return Ok(library_module(name, source, stamp, compile_flags, link_flags, install_dir.join("bin")));
}

/// A library installed on the system, like the LLVM of Homebrew or apt, through its llvm-config
pub fn system_library(_context: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
    let name: String = args.get("name")?;
    let compile_command: Vec<String> = args.get("compile_flags_from")?;
    let link_command: Vec<String> = args.get("link_flags_from")?;

    let tool = compile_command.first()
        .and_then(|program| system::find_program(program))
        .ok_or_else(|| format!("{name}: compile_flags_from names no program in the PATH"))?;

    let compile_flags = flags_from(&compile_command, None)?;
    let mut link_flags = flags_from(&link_command, None)?;
    link_flags.extend(args.get::<Vec<String>>("link_flags")?);

    // an update of the library is one of its tool
    let tools_dir = tool.parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    return Ok(library_module(name, tools_dir.clone(), tool, compile_flags, link_flags, tools_dir));
}

fn library_module(
    name: String,
    directory: PathBuf,
    artifact: PathBuf,
    compile_flags: Vec<String>,
    link_flags: Vec<String>,
    tools_dir: PathBuf,
) -> Value {
    return Value::from(Arc::new(BuiltModule {
        name,
        directory,
        artifact,
        modules: vec![],
        output: ModuleOutput::Native(NativeOutput {
            compile_flags,
            link_flags,
            program: None,
            tools_dir: Some(tools_dir),
            shared_library: None,
            android: None,
            // CMake projects, like LLVM, are of C++ mostly
            cpp: true,
        }),
        tests: vec![],
    }));
}

/// The output of the command, its program relative to `installation` when it has one. Without the
/// language standard: the module depending on it has its own.
fn flags_from(command: &[String], installation: Option<&Path>) -> Result<Vec<String>, String> {
    if command.is_empty() {
        return Ok(vec![]);
    }

    let program = match installation {
        Some(installation) => installation.join(&command[0]),
        None => PathBuf::from(&command[0]),
    };

    let output = Command::new(&program)
        .args(&command[1..])
        .output()
        .map_err(|error| format!("could not start {}: {error}", program.display()))?;

    if !output.status.success() {
        return Err(format!("{} failed: {}", command.join(" "), String::from_utf8_lossy(&output.stderr).trim()));
    }

    let flags = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter(|flag| !flag.starts_with("-std="))
        .map(str::to_string)
        .collect();

    return Ok(flags);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_come_from_the_tool_without_the_language_standard() {
        let command = ["echo", "-I/llvm/include -std=c++17 -fno-rtti"].map(String::from);

        assert_eq!(flags_from(&command, None).unwrap(), ["-I/llvm/include", "-fno-rtti"]);
        assert!(system::find_program("echo").is_some());
        assert!(flags_from(&[], None).unwrap().is_empty());
    }
}
