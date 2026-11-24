// Libraries outside vcpkg: build_cmake configures, builds and installs CMake projects with the CMake and
// Ninja of vcpkg, system_library takes an installed one, like the LLVM of Homebrew, prebuilt_library the files
// of an SDK a plugin installed. The modules depending on them get the headers and the libraries, or what a tool
// like llvm-config names.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use crate::common::system;
use crate::common::target::OperatingSystem;
use crate::common::up_to_date::{self, InputFingerprint};
use crate::language::runtime::{
    Arguments, BuildScriptContext, BuiltModule, FunctionParameters, FunctionSignature, ModuleOutput, Type, Value,
};

use super::toolchain::{self, Toolchain};
use super::{NativeOutput, vcpkg};

pub fn build_cmake_signature() -> FunctionSignature {
    return FunctionParameters::new()
        .required("name", Type::String)
        // the directory with CMakeLists.txt, like git(...) + "/llvm"
        .required("source", Type::String)
        // -D of CMake: "LLVM_ENABLE_PROJECTS=clang;lld"
        .strings("options")
        // what modules depending on it link, like "LLVMSupport", else every static library it installed
        .strings("libraries")
        // a tool of the installation and its arguments, its output the flags: "bin/llvm-config", "--cxxflags"
        .strings("compile_flags_from")
        .strings("link_flags_from")
        .strings("link_flags")
        // the targets it is built for besides those of the modules depending on it, each with zig
        .with_default("targets", Type::list(Type::target()), Value::List(vec![]))
        .returns(Type::Module);
}

pub fn prebuilt_library_signature() -> FunctionSignature {
    return FunctionParameters::new()
        .required("name", Type::String)
        // relative to the module or absolute, like those of an SDK tool() installed
        .strings("include_dirs")
        // "NAME" or "NAME=value"
        .strings("defines")
        // libraries by their path, -l and -L, frameworks
        .strings("link_flags")
        // what programs linking it load when they start: copied next to them
        .strings("shared_libraries")
        // NAME=value for the programs linking it when they run, like VK_ADD_LAYER_PATH
        .strings("environment")
        // C++ is in it: what links it links with the C++ compiler
        .with_default("cpp", Type::Bool, false)
        .with_default("directory", Type::String, ".")
        .returns(Type::Module);
}

/// A library of files that exist already, like those of an SDK: nothing to build, what depends on it gets its
/// headers, links its libraries and runs with its environment. Its stamp changes with what it names.
pub fn prebuilt_library(context: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
    let name: String = args.get("name")?;
    let directory = context.project_dir.join(args.get::<String>("directory")?);
    let output = prebuilt_output(
        &name,
        &directory,
        &args.get::<Vec<String>>("include_dirs")?,
        &args.get::<Vec<String>>("defines")?,
        args.get("link_flags")?,
        &args.get::<Vec<String>>("shared_libraries")?,
        &args.get::<Vec<String>>("environment")?,
        args.get("cpp")?,
    )?;

    let stamp = context.build_dir(&directory).join("prebuilt").join(&name);
    system::write_file_if_changed(&stamp, &format!("{output:?}"))?;

    return Ok(Value::from(Arc::new(BuiltModule {
        name,
        directory,
        artifact: stamp,
        modules: vec![],
        output: ModuleOutput::Native(output),
        tests: vec![],
    })));
}

/// What a prebuilt library gives the modules depending on it, its directories and libraries checked
fn prebuilt_output(
    name: &str,
    directory: &Path,
    include_dirs: &[String],
    defines: &[String],
    link_flags: Vec<String>,
    shared_libraries: &[String],
    environment: &[String],
    cpp: bool,
) -> Result<NativeOutput, String> {
    let existing = |path: &String| -> Result<PathBuf, String> {
        let path = directory.join(path);
        return match path.exists() {
            true => Ok(path),
            false => Err(format!("{name}: {} does not exist", path.display())),
        };
    };

    let include_dirs = include_dirs.iter().map(existing).collect::<Result<Vec<_>, _>>()?;
    let mut compile_flags: Vec<String> = include_dirs.iter().map(|dir| format!("-I{}", dir.display())).collect();
    compile_flags.extend(defines.iter().map(|define| format!("-D{define}")));

    let environment = environment.iter()
        .map(|entry| match entry.split_once('=') {
            Some((variable, value)) if !variable.is_empty() => Ok((variable.to_string(), value.to_string())),
            _ => Err(format!("{name}: environment takes NAME=value, not {entry}")),
        })
        .collect::<Result<Vec<_>, _>>()?;

    return Ok(NativeOutput {
        compile_flags,
        link_flags,
        program: None,
        tools_dir: None,
        shared_library: None,
        cpp,
        c_compiler: None,
        // the headers of an SDK change with its version, which its paths name
        header_dirs: vec![],
        abi: None,
        runtime_libraries: shared_libraries.iter().map(existing).collect::<Result<Vec<_>, _>>()?,
        environment,
    });
}

pub fn system_library_signature() -> FunctionSignature {
    return FunctionParameters::new()
        .required("name", Type::String)
        // a tool and its arguments, its output the flags: "llvm-config", "--cxxflags"
        .required("compile_flags_from", Type::list(Type::String))
        .required("link_flags_from", Type::list(Type::String))
        .strings("link_flags")
        .returns(Type::Module);
}

/// A source of git() builds once on the machine, in the cache, named after its commit and options. Other
/// sources build into build/cmake/<name>/ of the project. The build goes to build/, the installation to install/.
pub fn build_cmake(context: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
    let name: String = args.get("name")?;
    let options: Vec<String> = args.get("options")?;
    let source = context.project_dir.join(args.get::<String>("source")?);
    let compile_command: Vec<String> = args.get("compile_flags_from")?;
    let link_command: Vec<String> = args.get("link_flags_from")?;

    // another target than the machine: zig builds it, CMake gets its toolchain file
    let toolchain_file = match context.target {
        Some(target) if !compile_command.is_empty() || !link_command.is_empty() => {
            return Err(format!(
                "{name}: compile_flags_from and link_flags_from run a program of the installation, one for {} runs nowhere here: libraries = names what to link",
                target.name()
            ));
        }
        Some(target) => Some(Toolchain::for_target("auto", Some(target), context.android.as_ref())?.cmake_toolchain_file()?),
        None => None,
    };
    // the libraries of Visual Studio on Windows, those of every other system and of MinGW are lib<name>.a
    let msvc = cfg!(windows) && toolchain_file.is_none();

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
            let key = up_to_date::hash_hex((&source, &options, &environment, &cmake, context.target.map(|target| target.name())));
            system::cache_dir("cmake").join(format!("{name}-{key}"))
        }
        false => context.build_dir(&context.project_dir).join("cmake").join(&name),
    };

    let (binary_dir, install_dir) = (build_dir.join("build"), build_dir.join("install"));
    // what modules depending on it see change: written when the build changed something
    let stamp = build_dir.join("stamp");

    let mut fingerprint = InputFingerprint::new(build_dir.join("configured"));
    fingerprint.add(&source);
    fingerprint.add(&options);
    fingerprint.add(&environment);
    fingerprint.add(&cmake);
    // its contents: a new build-script may write another one
    fingerprint.add(toolchain_file.as_ref().map(|file| std::fs::read_to_string(file).unwrap_or_default()));

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

        // CMake finds its compilers once per build directory: for another one it starts afresh
        if let Some(toolchain_file) = &toolchain_file {
            configure.arg("--fresh").arg(format!("-DCMAKE_TOOLCHAIN_FILE={}", toolchain_file.display()));
        }

        // on Windows clang like for build_cpp, CMake would find no compiler without the environment of
        // Visual Studio. With the static C library of release builds, like the ports of vcpkg.
        if msvc {
            let [c_compiler, cpp_compiler, _] = toolchain::system_compilers()?;
            let resource_compiler = Path::new(&c_compiler).with_file_name("llvm-rc.exe");

            configure.env("CC", std::env::var("CC").unwrap_or(c_compiler))
                .env("CXX", std::env::var("CXX").unwrap_or(cpp_compiler))
                .arg("-DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded")
                .arg("-DCMAKE_POLICY_DEFAULT_CMP0091=NEW");

            // CMake reads a \ in a path as an escape
            if resource_compiler.is_file() {
                configure.arg(format!("-DCMAKE_RC_COMPILER={}", resource_compiler.display().to_string().replace('\\', "/")));
            }
        }

        configure.args(options.iter().map(|option| format!("-D{option}")));
        system::run_command_quietly(&mut configure, "configuring with CMake")?;

        fingerprint.store()?;
    }

    // a ninja that cannot tell has work to do
    let dry_run = system::command_output(Command::new(&ninja).arg("-C").arg(&binary_dir).arg("-n"), "ninja -n").unwrap_or_default();

    if dry_run.contains("no work to do") && stamp.is_file() {
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

    let compile_flags = match compile_command.is_empty() {
        true => vec![format!("-I{}", install_dir.join("include").display())],
        false => flags_from(&compile_command, Some(&install_dir))?,
    };

    // Visual Studio names them <library>.lib
    let (prefix, extension) = if msvc { ("", "lib") } else { ("lib", "a") };
    let mut link_flags: Vec<String> =
        libraries.iter()
            .map(|library| lib_dir.join(format!("{prefix}{library}.{extension}")).display().to_string())
            .collect();

    if !link_command.is_empty() {
        link_flags.extend(flags_from(&link_command, Some(&install_dir))?);
    }

    if link_flags.is_empty() {
        link_flags = system::files_with_extension(&[lib_dir.clone()], extension)
            .iter()
            .filter(|library| library.parent() == Some(lib_dir.as_path()))
            .map(|library| library.display().to_string())
            .collect();

        // GNU ld takes static libraries in order, these need each other
        if context.target().os == OperatingSystem::Linux {
            link_flags.insert(0, "-Wl,--start-group".to_string());
            link_flags.push("-Wl,--end-group".to_string());
        }
    }

    link_flags.extend(args.get::<Vec<String>>("link_flags")?);

    // what CMake built it with, always for release: the compilers of the system for the machine, zig for another
    // target
    let toolchain = Toolchain::for_target("auto", context.target, context.android.as_ref()).ok();

    return Ok(library_module(name, source, stamp, compile_flags, link_flags, install_dir.join("bin"), toolchain));
}

/// A library installed on the system, like the LLVM of Homebrew or apt, through its llvm-config
pub fn system_library(context: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
    super::only_for_the_machine(context)?;
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
    // like the LLVM of Homebrew with the compilers of the system
    return Ok(library_module(name, tools_dir.clone(), tool, compile_flags, link_flags, tools_dir, Toolchain::named("system").ok()));
}

fn library_module(
    name: String,
    directory: PathBuf,
    artifact: PathBuf,
    compile_flags: Vec<String>,
    link_flags: Vec<String>,
    tools_dir: PathBuf,
    // what built it
    toolchain: Option<Toolchain>,
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
            // CMake projects, like LLVM, are of C++ mostly
            cpp: true,
            c_compiler: toolchain.as_ref().map(|toolchain| toolchain.c_compiler_for_tools(true)),
            // installed with it, like the many of LLVM: what depends on it builds again with its artifact
            header_dirs: vec![],
            abi: toolchain.as_ref().map(Toolchain::abi),
            runtime_libraries: vec![],
            environment: vec![],
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

    let output = system::command_output(Command::new(&program).args(&command[1..]), &command.join(" "))?;
    let flags = output.split_whitespace()
        .filter(|flag| !flag.starts_with("-std="))
        .map(str::to_string)
        .collect();

    return Ok(flags);
}

// echo of the system, Windows has none
#[cfg(all(test, unix))]
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

// on every system, those above need echo
#[cfg(test)]
mod prebuilt_tests {
    use crate::common::system;
    use crate::common::test_support::ScratchDir;

    use super::prebuilt_output;

    #[test]
    fn a_prebuilt_library_names_files_that_exist_and_its_environment() {
        let directory = ScratchDir::new("prebuilt-library");
        system::write_file(&directory.join("sdk/include/vulkan/vulkan.h"), "").unwrap();
        system::write_file(&directory.join("sdk/lib/libVkLayer_khronos_validation.dylib"), "").unwrap();
        let strings = |items: &[&str]| -> Vec<String> { items.iter().map(|item| item.to_string()).collect() };

        let output = prebuilt_output(
            "vulkan-runtime",
            &directory,
            &strings(&["sdk/include"]),
            &strings(&["VK_NO_PROTOTYPES"]),
            strings(&["-lvulkan"]),
            &strings(&["sdk/lib/libVkLayer_khronos_validation.dylib"]),
            &strings(&["VK_ADD_LAYER_PATH=/sdk/share/vulkan/explicit_layer.d"]),
            false,
        )
        .unwrap();

        assert_eq!(output.compile_flags, [format!("-I{}", directory.join("sdk/include").display()), "-DVK_NO_PROTOTYPES".to_string()]);
        assert_eq!(output.link_flags, ["-lvulkan"]);
        assert_eq!(output.runtime_libraries, [directory.join("sdk/lib/libVkLayer_khronos_validation.dylib")]);
        assert_eq!(output.environment, [("VK_ADD_LAYER_PATH".to_string(), "/sdk/share/vulkan/explicit_layer.d".to_string())]);

        // a path of another version of the SDK, an environment without a value
        let error = prebuilt_output("vulkan", &directory, &strings(&["sdk/1.4/include"]), &[], vec![], &[], &[], false).unwrap_err();
        assert!(error.starts_with("vulkan: ") && error.ends_with("does not exist"), "{error}");
        let error = prebuilt_output("vulkan", &directory, &[], &[], vec![], &[], &strings(&["VK_ADD_LAYER_PATH"]), false).unwrap_err();
        assert_eq!(error, "vulkan: environment takes NAME=value, not VK_ADD_LAYER_PATH");
    }
}
