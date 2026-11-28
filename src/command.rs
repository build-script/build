// build_command: a command with declared inputs and outputs as a module, what plugins build languages with.
// It runs again only when an input, a module it uses or its command line changed.
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use crate::common::runner::Runner;
use crate::common::system;
use crate::common::up_to_date::{self, InputFingerprint};
use crate::jvm::JvmLaunch;
use crate::native::CCompiler;
use crate::language::runtime::{
    Arguments, BuildScriptContext, BuiltModule, FunctionParameters, ModuleOutput, ModuleTests, Placement, TestOutcome,
    TestRun, TestSuite, Type, Value, builtin,
};

/// stands for the directory of the command in its arguments: build/commands/<name>
const OUTPUT_PLACEHOLDER: &str = "{output}";

/// stands for the directory the tests write JUnit XML reports to, xkit counts them
const TEST_REPORTS_PLACEHOLDER: &str = "{test_reports}";

/// In `environment`, for tools that compile and link against the C and C++ libraries among the dependencies
/// with a compiler of their own, like cgo of Go: -I and -D of their headers, their libraries in link order and
/// the C compiler that built them. Their words are quoted the way Go and sh split them.
const COMPILE_FLAGS_PLACEHOLDER: &str = "{compile_flags}";
const LINK_FLAGS_PLACEHOLDER: &str = "{link_flags}";
const C_COMPILER_PLACEHOLDER: &str = "{c_compiler}";

/// The headers of C and C++ libraries whose hash those placeholders carry
const HEADER_EXTENSIONS: &[&str] = &["h", "hh", "hpp", "hxx", "h++", "inc", "inl", "ipp", "tpp", "tcc"];

/// The files of `files` by the command and the target that write them: two writers would overwrite each other,
/// and run at the same time
static WRITERS: LazyLock<Mutex<HashMap<PathBuf, (String, String)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// What `xkit run` and the modules using it need of a command
#[derive(Debug, Hash)]
pub struct CommandOutput {
    /// {output}
    pub output_dir: PathBuf,
    pub outputs: Vec<PathBuf>,
    /// what `xkit run` starts, empty for nothing
    pub run: Vec<String>,
    /// for `run` as well
    pub environment: Vec<(String, String)>,
    pub path: Vec<PathBuf>,
    /// what `run` and the tests need of the C and C++ libraries among the dependencies
    pub runtime: Runtime,
}

/// What the C and C++ libraries among the dependencies of a command need when what it built runs, like a program of
/// cgo: their environment, like the validation layers of Vulkan, and the directories of their shared libraries, like
/// the Vulkan loader of vcpkg, which programs of other languages do not have next to them
#[derive(Debug, Default, Hash)]
pub struct Runtime {
    pub environment: Vec<(String, String)>,
    pub library_dirs: Vec<PathBuf>,
}

/// where the system looks for shared libraries besides its own directories
const LIBRARY_PATH: &str = if cfg!(target_os = "macos") {
    "DYLD_LIBRARY_PATH"
} else if cfg!(windows) {
    "PATH"
} else {
    "LD_LIBRARY_PATH"
};

impl Runtime {
    /// Of the modules among the dependencies and those below them, bundles unpacked
    fn of(dependencies: &[Value]) -> Result<Self, String> {
        let mut modules = vec![];

        for value in dependencies {
            let Value::Module(module) = value else { continue };
            let built = module.wait().map_err(|_| format!("{} it uses failed", module.function))?;

            match crate::bundle::unpacked(&built) {
                Some(unpacked) => modules.extend(unpacked.modules),
                None => modules.push(built),
            }
        }

        let natives = BuiltModule::transitive(&modules);
        let mut library_dirs: Vec<PathBuf> = vec![];

        for module in &natives {
            let ModuleOutput::Native(output) = &module.output else { continue };

            for library in output.runtime_libraries.iter().chain(&output.shared_library) {
                if let Some(dir) = library.parent().filter(|dir| !library_dirs.iter().any(|known| known == dir)) {
                    library_dirs.push(dir.to_path_buf());
                }
            }
        }

        return Ok(Runtime {
            environment: crate::native::joined_environment(&natives),
            library_dirs,
        });
    }

    /// After the environment of the command, which wins: the library directories go in front of those the system
    /// looks in already
    fn apply(&self, process: &mut Command) -> Result<(), String> {
        let set = |process: &Command, variable: &str| {
            return process.get_envs().find(|(name, _)| *name == variable).map(|(_, value)| value.map(OsString::from));
        };

        for (variable, value) in &self.environment {
            if set(process, variable).is_none() {
                process.env(variable, value);
            }
        }

        if self.library_dirs.is_empty() {
            return Ok(());
        }

        let current = set(process, LIBRARY_PATH)
            .unwrap_or_else(|| std::env::var_os(LIBRARY_PATH))
            .unwrap_or_default();
        let directories = self.library_dirs.iter().cloned().chain(std::env::split_paths(&current));
        let joined = std::env::join_paths(directories).map_err(|error| format!("{LIBRARY_PATH}: {error}"))?;
        process.env(LIBRARY_PATH, joined);

        return Ok(());
    }
}

pub fn builtins() -> Vec<(&'static str, Value)> {
    let command_line = || Type::list(Type::Union(vec![Type::String, Type::Module]));

    return vec![builtin(
        "build_command",
        FunctionParameters::new()
            .required("name", Type::String)
            .required("command", command_line())
            .strings("inputs")
            .strings("exclude")
            .strings("outputs")
            .with_default("dependencies", Type::list(Type::Module), Value::List(vec![]))
            // NAME=value, with {compile_flags}, {link_flags} and {c_compiler} of the C and C++ libraries among the
            // dependencies: an entry they leave empty is not set, the tool takes its default
            .strings("environment")
            .strings("environment_inputs")
            .strings("path")
            .with_default("directory", Type::String, ".")
            .with_default("run", command_line(), Value::List(vec![]))
            .with_default("test", command_line(), Value::List(vec![]))
            .with_default("test_timeout", Type::Int, Value::Int(0))
            .with_default("always", Type::Bool, false)
            // once for every file of inputs, which {input} and {input_name} name in the command and the outputs,
            // like a shader to SPIR-V: only those that changed run again
            .with_default("per_input", Type::Bool, false)
            // where the command writes the files it read besides its inputs, like -MD -MF of glslc for the files a
            // shader includes, with {input_name} once for every input: they count like inputs from then on
            .with_default("depfile", Type::String, "")
            .with_default("targets", Type::list(Type::target()), Value::List(vec![]))
            // what file(path, content) gives: written below the directory before the command, like the go.mod
            // of the libraries the script lists
            .with_default("files", Type::list(Type::list(Type::String)), Value::List(vec![]))
            // the libraries the script lists, name@version; direct_libraries prints those the command took once it
            // ran, like go mod tidy completing the go.mod of `files`: one a line, name@version, the name and the
            // version like serde v1.0.215 of cargo tree, or the name alone
            .strings("libraries")
            .with_default("direct_libraries", command_line(), Value::List(vec![]))
            // prints {name} in {version}, latest without one, like direct_libraries or the version alone like npm
            // view: what xkit install writes
            .with_default("resolve", Type::list(Type::String), Value::List(vec![]))
            .returns(Type::Module)
            // a program that writes a file once for every target, a compiler of Go once per target
            .placed(Placement::UnlessUnchanged)
            .running_on_host("command"),
        build_command,
    ), builtin(
        "embed_files",
        FunctionParameters::new()
            .required("name", Type::String)
            // globs relative to the directory, or modules of commands whose outputs it embeds, like SPIR-V
            .required("files", command_line())
            // as words of 32 bits, like SPIR-V: arrays of uint32_t, else of bytes
            .with_default("words", Type::Bool, false)
            .with_default("directory", Type::String, ".")
            .returns(Type::Module)
            .placed(Placement::UnlessUnchanged)
            .running_on_host("files"),
        embed_files,
    )];
}

/// Every file as a header of its own in {output}, <file>.h with an array named after the file and its size: a program
/// includes it and reads no file when it runs. A header is written when it changed, those of files that went go too.
fn embed_files(context: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
    let Place { name, directory, output_dir, .. } = Place::of(context, args)?;
    let words: bool = args.get("words")?;
    let mut files = vec![];
    let mut patterns = vec![];

    for value in args.get::<Vec<Value>>("files")? {
        match value {
            Value::String(pattern) => patterns.push(pattern),
            Value::Module(module) => {
                let module = module.wait()
                    .map_err(|_| format!("{} it embeds failed", module.function))?;

                match &module.output {
                    ModuleOutput::Command(output) => files.extend(output.outputs.iter().cloned()),
                    _ => files.push(module.artifact.clone()),
                }
            }
            other => return Err(format!("internal error: embed_files of {}", other.type_of())),
        }
    }

    files.extend(system::files_matching(&directory, &patterns, &[]));
    system::create_dir(&output_dir)?;
    let mut headers: Vec<PathBuf> = vec![];

    for file in &files {
        let file_name = file.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let header = output_dir.join(format!("{file_name}.h"));

        if headers.contains(&header) {
            return Err(format!("{name} embeds two files named {file_name}, their headers would be one"));
        }

        let bytes = std::fs::read(file).map_err(|error| format!("{name}: could not read {}: {error}", file.display()))?;
        system::write_file_if_changed(&header, &embedded_header(&file_name, &bytes, words).map_err(|error| format!("{name}: {error}"))?)?;
        headers.push(header);
    }

    for file in system::files_below(std::slice::from_ref(&output_dir)) {
        if !headers.contains(&file) {
            let _ = std::fs::remove_file(&file);
        }
    }

    return Ok(Value::from(Arc::new(BuiltModule {
        name,
        directory,
        artifact: output_dir.clone(),
        modules: vec![],
        output: ModuleOutput::Command(CommandOutput {
            output_dir,
            outputs: headers,
            run: vec![],
            environment: vec![],
            path: vec![],
            runtime: Runtime::default(),
        }),
        tests: vec![],
    })));
}

/// The C and C++ header of a file: its bytes, or its words of 32 bits in the order of the little-endian machines that
/// write and read them, like SPIR-V. double.comp.spv is double_comp_spv and double_comp_spv_size, in bytes.
fn embedded_header(file_name: &str, bytes: &[u8], words: bool) -> Result<String, String> {
    let mut identifier: String = file_name.chars()
        .map(|character| if character.is_ascii_alphanumeric() { character } else { '_' })
        .collect();

    if identifier.starts_with(|character: char| character.is_ascii_digit()) {
        identifier.insert(0, '_');
    }

    if words && bytes.len() % 4 != 0 {
        return Err(format!("{file_name} has {} bytes, which are no words of 4", bytes.len()));
    }

    let (kind, values, per_line): (&str, Vec<String>, usize) = match words {
        true => ("uint32_t", bytes.chunks(4).map(|word| format!("0x{:08x}", u32::from_le_bytes([word[0], word[1], word[2], word[3]]))).collect(), 8),
        false => ("unsigned char", bytes.iter().map(|byte| format!("0x{byte:02x}")).collect(), 12),
    };

    // an empty array is no C
    let lines = match values.is_empty() {
        true => "    0,".to_string(),
        false => values.chunks(per_line)
            .map(|line| format!("    {},", line.join(", ")))
            .collect::<Vec<_>>()
            .join("\n"),
    };

    return Ok(format!(
        "// Embedded by xkit from {file_name}, changes here are lost\n#pragma once\n\n#include <stddef.h>\n#include <stdint.h>\n\n\
         static const {kind} {identifier}[] = {{\n{lines}\n}};\nstatic const size_t {identifier}_size = {};\n",
        bytes.len()
    ));
}

/// What the arguments say, with the placeholders replaced and the modules by their files
struct CommandSpec {
    name: String,
    directory: PathBuf,
    /// build/ of its directory, build/<target>/ for another target than the machine
    build_dir: PathBuf,
    output_dir: PathBuf,
    command: Vec<String>,
    inputs: Vec<PathBuf>,
    outputs: Vec<PathBuf>,
    environment: Vec<(String, String)>,
    /// inherited variables that count as inputs, with their values
    environment_inputs: Vec<(String, Option<String>)>,
    path: Vec<PathBuf>,
    /// what the modules of the command and its dependencies produced
    module_files: Vec<PathBuf>,
    /// written before the command: below the directory, with their content
    files: Vec<(PathBuf, String)>,
    /// {output} replaced, {input_name} and {input} not yet: empty without one
    depfile: String,
    libraries: Vec<String>,
    direct_libraries: Vec<String>,
    /// for the tests and `run`
    runtime: Runtime,
}

fn build_command(context: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
    let spec = CommandSpec::read(context, args)?;
    let always: bool = args.get("always")?;
    let target = context.target.map_or("this machine".to_string(), |target| target.name());
    claim_files(&spec, &target)?;

    let (artifact, outputs) = match args.get("per_input")? {
        true => (spec.output_dir.clone(), spec.run_per_input(always)?),
        false => (spec.outputs.first().cloned().unwrap_or_else(|| spec.output_dir.clone()), spec.run_once(always)?),
    };

    let run = command_line(args.get("run")?, &spec.output_dir)?;
    let test = command_line(args.get("test")?, &spec.output_dir)?;

    let tests = match !test.is_empty() && context.tests.includes(&spec.name) {
        true => vec![TestSuite {
            source_set: "test",
            tests: spec.tests(context, test, args.get("test_timeout")?)?,
        }],
        false => vec![],
    };

    return Ok(Value::from(Arc::new(BuiltModule {
        name: spec.name,
        directory: spec.directory,
        artifact,
        // the modules of a command are no dependencies of the modules using it
        modules: vec![],
        output: ModuleOutput::Command(CommandOutput {
            output_dir: spec.output_dir,
            outputs,
            run,
            environment: spec.environment,
            path: spec.path,
            runtime: spec.runtime,
        }),
        tests,
    })));
}

impl CommandSpec {
    fn read(context: &BuildScriptContext, args: &Arguments) -> Result<Self, String> {
        let Place { name, directory, build_dir, output_dir } = Place::of(context, args)?;

        let command = command_line(args.get("command")?, &output_dir)?;

        if command.is_empty() {
            return Err("the command is empty, its first element is the program".to_string());
        }

        let outputs = args.get::<Vec<String>>("outputs")?
            .iter()
            .map(|output| directory.join(with_output(output, &output_dir)))
            .collect();

        let mut module_values = args.get::<Vec<Value>>("command")?;
        let dependencies = args.get::<Vec<Value>>("dependencies")?;
        let c_libraries = CLibraries::of(&dependencies)?;
        let environment = environment(args, &output_dir, &c_libraries)?;
        let runtime = Runtime::of(&dependencies)?;

        let environment_inputs = args.get::<Vec<String>>("environment_inputs")?
            .into_iter()
            .map(|variable| {
                let value = std::env::var(&variable).ok();
                return (variable, value);
            })
            .collect();

        let path = path(args, &directory, &output_dir)?;

        // once they are built: what the modules it depends on write may be among its inputs, like the SPIR-V a
        // validation takes
        module_values.extend(dependencies);
        let mut module_files = module_files(&module_values)?;
        module_files.extend(c_libraries.headers);

        let inputs = system::files_matching(&directory, &args.get::<Vec<String>>("inputs")?, &args.get::<Vec<String>>("exclude")?);

        let mut files = vec![];

        for file in args.get::<Vec<Vec<String>>>("files")? {
            let [path, content] = file.as_slice() else {
                return Err(format!("files takes file(path, content), not a list of {} strings", file.len()));
            };

            files.push((below(&directory, path)?, content.clone()));
        }

        let direct_libraries = command_line(args.get("direct_libraries")?, &output_dir)?;
        let depfile = with_output(&args.get::<String>("depfile")?, &output_dir);

        return Ok(CommandSpec {
            name,
            directory,
            build_dir,
            output_dir,
            command,
            inputs,
            outputs,
            environment,
            environment_inputs,
            path,
            module_files,
            files,
            depfile,
            libraries: args.get("libraries")?,
            direct_libraries,
            runtime,
        });
    }

    /// The command with its directory and environment, `program` and `arguments` of it
    fn process(&self, program: &str, arguments: &[String]) -> Result<Command, String> {
        let mut process = Command::new(program);
        process.args(arguments).current_dir(&self.directory);
        set_environment(&mut process, &self.environment, &self.path)?;
        return Ok(process);
    }

    /// Unless nothing it depends on changed since it ran. Its outputs.
    fn run_once(&self, always: bool) -> Result<Vec<PathBuf>, String> {
        // with what its depfile names: before the run what the last run read, after it what this one read
        let fingerprint = || -> Result<InputFingerprint, String> {
            let mut fingerprint = InputFingerprint::in_build_dir(&self.build_dir, &format!("commands/{}", self.name));
            fingerprint.add((&self.command, &self.outputs, &self.environment, &self.environment_inputs, &self.path));
            fingerprint.add((&self.files, &self.libraries, &self.direct_libraries));
            fingerprint.add_files(&self.inputs)?;
            fingerprint.add_files(&self.module_files)?;
            fingerprint.add_files(&self.included(&self.depfile))?;
            return Ok(fingerprint);
        };

        let artifact = self.outputs
            .first()
            .cloned()
            .unwrap_or_else(|| self.output_dir.clone());
        let outputs: Vec<&Path> = self.outputs
            .iter()
            .map(PathBuf::as_path)
            .collect();

        // the files it writes are next to the sources, where an editor may change what the command made of them
        let kept_outputs = self.build_dir
            .join(up_to_date::FINGERPRINTS_DIR)
            .join(format!("commands/{}.outputs", self.name));
        let outputs_changed = !self.files.is_empty() && !outputs_as_left(&kept_outputs, &self.outputs);

        if always || outputs_changed || !fingerprint()?.reports_up_to_date(&self.name, &outputs)? {
            self.run()?;

            if !self.files.is_empty() {
                system::write_file(&kept_outputs, &format!("{:016x}", up_to_date::files_hash(&self.outputs)?))?;
            }

            fingerprint()?.store_done(&self.name, &artifact)?;
        }

        return Ok(self.outputs.clone());
    }

    /// Once for every input with {input} and {input_name}, each only when it or what the command depends on
    /// changed. What a removed input left in {output} goes. Its outputs, those of every input.
    fn run_per_input(&self, always: bool) -> Result<Vec<PathBuf>, String> {
        if !self.files.is_empty() || !self.direct_libraries.is_empty() {
            return Err(format!("{} runs once for every input, files and direct_libraries are for a command that runs once", self.name));
        }

        system::create_dir(&self.output_dir)?;
        let mut all_outputs = vec![];
        // with the depfiles, which the next build reads
        let mut kept = vec![];
        let mut ran = 0;

        for input in &self.inputs {
            let input_name = input.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let (command, outputs) = for_input(&self.command, &self.outputs, input);

            // of its own, by where it is below the directory, with what its depfile names: before the run what the
            // last run read, after it what this one read
            let relative = system::relative_with_slashes(&self.directory, input).replace(['/', ':'], "_");
            let depfile = for_input(&[self.depfile.clone()], &[], input).0.remove(0);
            let fingerprint = || -> Result<InputFingerprint, String> {
                let mut fingerprint = InputFingerprint::in_build_dir(&self.build_dir, &format!("commands/{}/{relative}", self.name));
                fingerprint.add((&command, &outputs, &self.environment, &self.environment_inputs, &self.path));
                fingerprint.add_files(std::slice::from_ref(input))?;
                fingerprint.add_files(&self.module_files)?;
                fingerprint.add_files(&self.included(&depfile))?;
                return Ok(fingerprint);
            };

            let expected: Vec<&Path> = outputs.iter().map(PathBuf::as_path).collect();

            if always || !fingerprint()?.is_up_to_date(&expected)? {
                create_output_dirs(&outputs)?;
                let what = what(&command);
                system::log(&self.name, &format!("Running {what} for {input_name}"));
                let mut process = self.process(&command[0], &command[1..])?;
                system::run_command_in_one_piece(&mut process, &what)?;

                if let Some(missing) = outputs.iter().find(|output| !output.exists()) {
                    return Err(format!("{what} did not write {} for {input_name}", system::relative_with_slashes(&self.directory, missing)));
                }

                fingerprint()?.store()?;
                ran += 1;
            }

            kept.extend(outputs.iter().cloned());
            if !depfile.is_empty() {
                kept.push(self.directory.join(&depfile));
            }
            all_outputs.extend(outputs);
        }

        // only when it names what it writes: else nothing in {output} is known to be left over
        if !self.outputs.is_empty() {
            for file in system::files_below(std::slice::from_ref(&self.output_dir)) {
                if !kept.contains(&file) {
                    let _ = std::fs::remove_file(&file);
                }
            }
        }

        match ran {
            0 => system::log(&self.name, "Up to date"),
            _ => system::log(&self.name, &format!("Done: {ran} of {} input(s)", self.inputs.len())),
        }

        return Ok(all_outputs);
    }

    /// What the depfile of the last run names that still exists, relative to the directory: nothing before the
    /// first run, which has to happen anyway
    fn included(&self, depfile: &str) -> Vec<PathBuf> {
        if depfile.is_empty() {
            return vec![];
        }

        let text = std::fs::read_to_string(self.directory.join(depfile)).unwrap_or_default();
        return crate::native::depfile_inputs(&text)
            .into_iter()
            .map(|file| self.directory.join(file))
            .filter(|file| file.is_file())
            .collect();
    }

    /// Into a fresh {output}: what a renamed source left there is gone. The files first, the libraries the
    /// command took once it ran.
    fn run(&self) -> Result<(), String> {
        let what = what(&self.command);
        system::recreate_dir(&self.output_dir)?;
        create_output_dirs(&self.outputs)?;

        for (path, content) in &self.files {
            system::write_file(path, content)?;
        }

        system::log(&self.name, &format!("Running {what}"));

        let mut process = self.process(&self.command[0], &self.command[1..])?;
        system::run_command_in_one_piece(&mut process, &what)?;

        if let Some(missing) = self.outputs.iter().find(|output| !output.exists()) {
            return Err(format!("{what} did not write {}", system::relative_with_slashes(&self.directory, missing)));
        }

        if !self.direct_libraries.is_empty() {
            let listing = crate::command::what(&self.direct_libraries);
            let mut process = self.process(&self.direct_libraries[0], &self.direct_libraries[1..])?;
            let taken = system::command_output(&mut process, &listing)?;

            for note in checked_libraries(&self.name, &self.libraries, &taken)? {
                system::log(&self.name, &note);
            }
        }

        return Ok(());
    }

    /// Not again while neither the tests nor what they test changed since they passed
    fn tests(&self, context: &BuildScriptContext, test: Vec<String>, timeout: i64) -> Result<ModuleTests, String> {
        let reports_dir = self.output_dir.with_file_name(format!("{}.test-reports", self.name));
        let reports = reports_dir.display().to_string();
        let mut test: Vec<String> = test.iter()
            .map(|argument| argument.replace(TEST_REPORTS_PLACEHOLDER, &reports))
            .collect();
        let writes_reports = test.iter().any(|argument| argument.contains(&reports));
        test.extend(context.test_arguments.iter().cloned());

        let mut fingerprint = InputFingerprint::in_build_dir(&self.build_dir, &format!("commands/{}.tests", self.name));
        fingerprint.add((&test, &self.environment, &self.environment_inputs, &self.path, &self.runtime));
        fingerprint.add_files(&self.inputs)?;
        fingerprint.add_files(&self.module_files)?;
        fingerprint.add_files(&self.outputs)?;

        if fingerprint.is_up_to_date(&[])? {
            system::log(&self.name, "Tests up to date");
            return Ok(ModuleTests::Known(TestOutcome::UpToDate));
        }

        let mut process = self.process(&test[0], &test[1..])?;
        self.runtime.apply(&mut process)?;
        let mut run = TestRun::new(what(&test), &process, reports_dir, fingerprint);

        if writes_reports {
            run = run.with_report_prefix("");
        }

        if timeout > 0 {
            run = run.with_timeout(Duration::from_secs(timeout as u64));
        }

        return Ok(ModuleTests::Run(Box::new(run)));
    }
}

/// Where the outputs go, before the command: compilers like glslc write a file, but no directory for it
fn create_output_dirs(outputs: &[PathBuf]) -> Result<(), String> {
    for output in outputs {
        if let Some(parent) = output.parent() {
            system::create_dir(parent)?;
        }
    }

    return Ok(());
}

/// The command and the outputs for one input: {input} its path, {input_name} its name like triangle.vert
fn for_input(command: &[String], outputs: &[PathBuf], input: &Path) -> (Vec<String>, Vec<PathBuf>) {
    let path = input.display().to_string();
    let name = input.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let fill = |text: &str| text.replace("{input_name}", &name).replace("{input}", &path);

    return (
        command.iter().map(|argument| fill(argument)).collect(),
        outputs.iter().map(|output| PathBuf::from(fill(&output.display().to_string()))).collect(),
    );
}

/// Where a command runs and writes: its name, directory, build/ and {output}
struct Place {
    name: String,
    directory: PathBuf,
    /// build/ of its directory, build/<target>/ for another target than the machine
    build_dir: PathBuf,
    output_dir: PathBuf,
}

impl Place {
    fn of(context: &BuildScriptContext, args: &Arguments) -> Result<Self, String> {
        let name: String = args.get("name")?;

        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
            return Err(format!("\"{name}\" cannot name a command, its files go to build/commands/<name>"));
        }

        // without the . of directory = "." in every path of the log
        let directory: PathBuf = context.project_dir
            .join(args.get::<String>("directory")?)
            .components()
            .filter(|component| *component != Component::CurDir)
            .collect();

        if !directory.is_dir() {
            return Err(format!("directory {} does not exist", directory.display()));
        }

        let build_dir = context.build_dir(&directory);
        let output_dir = build_dir.join("commands").join(&name);

        return Ok(Place { name, directory, build_dir, output_dir });
    }
}

/// NAME=value of `environment`, {output} and those of the C libraries replaced. What the C compiler needs in
/// its environment comes first when it is named, so the entries of the script win.
fn environment(args: &Arguments, output_dir: &Path, c_libraries: &CLibraries) -> Result<Vec<(String, String)>, String> {
    let mut environment = vec![];
    let mut compiler_environment = vec![];

    for entry in args.get::<Vec<String>>("environment")? {
        let Some((variable, value)) = entry.split_once('=') else {
            return Err(format!("environment takes NAME=value, not {entry}"));
        };

        let names_c = [COMPILE_FLAGS_PLACEHOLDER, LINK_FLAGS_PLACEHOLDER, C_COMPILER_PLACEHOLDER]
            .iter()
            .any(|placeholder| value.contains(placeholder));

        if value.contains(C_COMPILER_PLACEHOLDER) && let Some(compiler) = &c_libraries.compiler {
            compiler_environment = compiler.environment
                .iter()
                .map(|(variable, value)| (variable.clone(), value.display().to_string()))
                .collect();
        }

        let value = with_output(value, output_dir)
            .replace(COMPILE_FLAGS_PLACEHOLDER, &quoted_words(&c_libraries.compile_flags)?)
            .replace(LINK_FLAGS_PLACEHOLDER, &quoted_words(&c_libraries.link_flags)?)
            .replace(C_COMPILER_PLACEHOLDER, &quoted_words(c_libraries.compiler.as_ref().map_or(&[][..], |compiler| &compiler.command))?);

        // CC= without a C library: the tool takes its own compiler, not none
        if names_c && value.trim().is_empty() {
            continue;
        }

        environment.push((variable.to_string(), value));
    }

    return Ok([compiler_environment, environment].concat());
}

/// What the C and C++ libraries among the dependencies of a command give the placeholders of `environment`
#[derive(Default)]
struct CLibraries {
    compile_flags: Vec<String>,
    link_flags: Vec<String>,
    /// that of the first library that names one
    compiler: Option<CCompiler>,
    /// the headers of the libraries of the script, also of those they depend on: a change builds the command
    /// again, and the compile flags define their hash for tools with a cache of their own, like Go, which knows
    /// no headers outside its packages
    headers: Vec<PathBuf>,
}

impl CLibraries {
    /// Like a module of C depending on them takes them: the libraries of a later one link before those of an
    /// earlier one, a library of C++ links the runtime of C++, then what the compilers of the libraries need
    fn of(dependencies: &[Value]) -> Result<Self, String> {
        let mut libraries = CLibraries::default();
        let mut modules = vec![];
        let mut cpp_runtime = None;
        let mut compiler_link_flags = vec![];

        let mut expanded = vec![];

        for value in dependencies {
            let Value::Module(module) = value else { continue };
            let module = module.wait()
                .map_err(|_| format!("{} it uses failed", module.function))?;

            // of a bundle the modules it holds: its ports a function of a plugin gives to a build_c with bundled()
            match crate::bundle::unpacked(&module) {
                Some(unpacked) => expanded.extend(unpacked.modules),
                None => expanded.push(module),
            }
        }

        for module in expanded {
            let ModuleOutput::Native(output) = &module.output else { continue };

            libraries.compile_flags.extend(output.compile_flags.iter().cloned());
            libraries.link_flags.splice(0..0, output.link_flags.iter().cloned());

            if libraries.compiler.is_none() {
                libraries.compiler = output.c_compiler.clone();
            }

            if output.cpp && cpp_runtime.is_none() {
                cpp_runtime = Some(output.c_compiler.as_ref().map(|compiler| compiler.cpp_runtime.clone()).unwrap_or_default());
            }

            for flag in output.c_compiler.iter().flat_map(|compiler| &compiler.link_flags) {
                if !compiler_link_flags.contains(flag) {
                    compiler_link_flags.push(flag.clone());
                }
            }

            modules.push(module);
        }

        let mut headers: Vec<PathBuf> = BuiltModule::transitive(&modules)
            .iter()
            .filter_map(|module| match &module.output {
                ModuleOutput::Native(output) => Some(&output.header_dirs),
                _ => None,
            })
            .flatten()
            .flat_map(|dir| system::files_matching(dir, &["**".to_string()], &["build/**".to_string()]))
            // an include directory may be the module itself, with its sources and build/
            .filter(|file| file.extension().is_some_and(|extension| HEADER_EXTENSIONS.iter().any(|header| extension == *header)))
            .collect();
        headers.sort();
        headers.dedup();

        if !headers.is_empty() {
            libraries.compile_flags.push(format!("-DXKIT_HEADERS={:016x}", up_to_date::files_hash(&headers)?));
        }

        libraries.compile_flags = crate::native::without_repeated_flags(libraries.compile_flags);
        libraries.link_flags.extend(cpp_runtime.unwrap_or_default());
        libraries.link_flags.extend(compiler_link_flags);
        libraries.headers = headers;

        return Ok(libraries);
    }
}

/// Words as one value the way Go reads CC and CGO_CFLAGS, and sh too: a word with a space or a quote in quotes
/// of the other kind
fn quoted_words(words: &[String]) -> Result<String, String> {
    let mut quoted = vec![];

    for word in words {
        let plain = !word.is_empty() && !word.contains(|character: char| character.is_whitespace() || character == '\'' || character == '"');

        quoted.push(match (plain, word.contains('\'')) {
            (true, _) => word.clone(),
            (false, false) => format!("'{word}'"),
            (false, true) if !word.contains('"') => format!("\"{word}\""),
            (false, true) => return Err(format!("{word} has quotes of both kinds, a tool could not split it off the others")),
        });
    }

    return Ok(quoted.join(" "));
}

/// The directories of `path`, relative to the directory of the command
fn path(args: &Arguments, directory: &Path, output_dir: &Path) -> Result<Vec<PathBuf>, String> {
    return Ok(args.get::<Vec<String>>("path")?
        .iter()
        .map(|directory_on_path| directory.join(with_output(directory_on_path, output_dir)))
        .collect());
}

/// A path of `files` below the directory of the command: neither absolute nor up with ..
fn below(directory: &Path, path: &str) -> Result<PathBuf, String> {
    let relative = Path::new(path);
    let leaves = relative.components()
        .any(|component| matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_)));

    if path.is_empty() || leaves {
        return Err(format!("files go below the directory of the command, \"{path}\" does not"));
    }

    return Ok(directory.join(relative));
}

/// The files of the command are its own: no other command writes them, and no other target of it
fn claim_files(spec: &CommandSpec, target: &str) -> Result<(), String> {
    let mut writers = WRITERS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    for (path, _) in &spec.files {
        let file = system::relative_with_slashes(&spec.directory, path);

        match writers.get(path) {
            Some((name, _)) if *name != spec.name => {
                return Err(format!(
                    "{} writes {file} like {name}: commands that need the same file depend on one command that writes it",
                    spec.name
                ));
            }
            Some((_, other)) if other != target => {
                return Err(format!(
                    "{} writes {file} for {target} and for {other}: the files of a command are the same for every target, \
                     a command of its own writes them once",
                    spec.name
                ));
            }
            _ => {
                writers.insert(path.clone(), (spec.name.clone(), target.to_string()));
            }
        }
    }

    return Ok(());
}

/// Whether the outputs are as the last run left them, which `kept` holds the hash of
fn outputs_as_left(kept: &Path, outputs: &[PathBuf]) -> bool {
    let Ok(stored) = std::fs::read_to_string(kept) else {
        return false;
    };

    return up_to_date::files_hash(outputs).is_ok_and(|hash| stored.trim() == format!("{hash:016x}"));
}

/// The name of name@version, all of it without a version: github.com/spf13/cobra, @types/node of npm
pub fn library_name(notation: &str) -> &str {
    return split_library(notation).0;
}

/// The name and version of name@version, "" without a version
fn split_library(notation: &str) -> (&str, &str) {
    match notation.rfind('@') {
        Some(at) if at > 0 => return (&notation[..at], &notation[at + 1..]),
        _ => return (notation, ""),
    }
}

/// A library as a tool prints it, written name@version: name@version itself, the name and the version like
/// serde v1.0.215 of cargo tree, serde = "1.0.229" of cargo search, what follows them left out, or the name
/// alone. None when what stands for the version is none, like the = of serde@=.
fn printed_library(line: &str) -> Option<String> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let first = *fields.first()?;

    // @types/node 22.0.0 of npm: its @ starts the name
    let (name, version) = match split_library(first) {
        (name, version) if !version.is_empty() => (name, Some(version)),
        _ => match fields.get(1) {
            Some(&"=") => (first, Some(fields.get(2).map_or("", |version| version.trim_matches(['"', '\''])))),
            version => (first, version.copied()),
        },
    };

    match version {
        None => return Some(name.to_string()),
        Some(version) if version.starts_with(|character: char| character.is_ascii_alphanumeric()) => {
            return Some(format!("{name}@{version}"));
        }
        Some(_) => return None,
    }
}

/// What resolve printed for the library `name`, in its first line: a library like printed_library reads it, or
/// its version alone like npm view prints it. An error without a version, or for another library, like the
/// nearest one a search found.
fn resolved_library(name: &str, printed: &str) -> Result<String, String> {
    let line = printed.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or_else(|| format!("printed no version of {name}"))?;

    let resolved = match !line.contains(char::is_whitespace) && split_library(line).1.is_empty() && line != name {
        true => printed_library(&format!("{name}@{line}")),
        false => printed_library(line),
    };

    let Some(resolved) = resolved.filter(|resolved| !split_library(resolved).1.is_empty()) else {
        return Err(format!("printed no version of {name}: {line}"));
    };

    // crates.io and PyPI take serde_json for serde-json, PyPI also any case
    let normalized = |name: &str| name.to_lowercase().replace(['_', '.'], "-");

    if normalized(library_name(&resolved)) != normalized(name) {
        return Err(format!("printed {resolved}, which is no version of {name}"));
    }

    return Ok(resolved);
}

/// v1.0.215 of cargo tree is 1.0.215 of a script, the v of a Go version is in both
fn same_version(first: &str, second: &str) -> bool {
    fn without_v(version: &str) -> &str {
        match version.strip_prefix('v') {
            Some(rest) if rest.starts_with(|character: char| character.is_ascii_digit()) => return rest,
            _ => return version,
        }
    }

    return without_v(first) == without_v(second);
}

/// What the command took, one library a line, against what the script lists: an error for one the script does
/// not list, notes for listed ones it does not take and for other versions than a listed one names
fn checked_libraries(module: &str, listed: &[String], taken: &str) -> Result<Vec<String>, String> {
    let taken: Vec<String> = taken.lines()
        .filter_map(printed_library)
        .collect();
    let listing = |library: &str| listed.iter().find(|listed| library_name(listed) == library_name(library));

    let mut unlisted = vec![];
    let mut notes = vec![];

    for library in &taken {
        match listing(library) {
            None => unlisted.push(format!("{module} uses {library}, which its libraries do not list: xkit install {library}")),
            Some(listed) => {
                let (wanted, version) = (split_library(listed).1, split_library(library).1);

                if !wanted.is_empty() && !same_version(wanted, version) {
                    notes.push(format!("uses {library} for {listed} of its libraries"));
                }
            }
        }
    }

    for library in listed {
        if !taken.iter().any(|taken| library_name(taken) == library_name(library)) {
            notes.push(format!("lists {library}, which it does not use"));
        }
    }

    if !unlisted.is_empty() {
        return Err(unlisted.join("\n"));
    }

    return Ok(notes);
}

/// What xkit install writes for `library` of the command `args` describe: what its `resolve` prints with
/// {name} and {version}, latest without one. None when it has no resolve.
pub fn resolve(context: &BuildScriptContext, args: &Arguments, library: &str) -> Result<Option<String>, String> {
    let resolve: Vec<String> = args.get("resolve")?;

    if resolve.is_empty() {
        return Ok(None);
    }

    let Place { name, directory, output_dir, .. } = Place::of(context, args)?;
    let (library_name, version) = match split_library(library) {
        (library_name, "") => (library_name, "latest"),
        named => named,
    };
    let line: Vec<String> = resolve.iter()
        .map(|argument| argument.replace("{name}", library_name).replace("{version}", version))
        .collect();

    let mut process = Command::new(&line[0]);
    process.args(&line[1..]).current_dir(&directory);
    // xkit install builds no C library: entries of theirs are left out
    let environment = environment(args, &output_dir, &CLibraries::default())?;
    set_environment(&mut process, &environment, &path(args, &directory, &output_dir)?)?;

    let printed = system::command_output(&mut process, &what(&line))?;

    return resolved_library(library_name, &printed)
        .map(Some)
        .map_err(|problem| format!("{} for {name} {problem}", what(&line)));
}

/// `xkit run` of a command: its `run` with the arguments after --, in its directory
pub fn launch(module: &BuiltModule, output: &CommandOutput, args: &[String], runner: &Runner) -> Result<i32, String> {
    let Some((program, arguments)) = output.run.split_first() else {
        return Err(format!("{} has nothing to run, build_command(run = ...) names what", module.name));
    };

    let mut process = runner.command(Path::new(program), &module.directory);
    process.args(arguments).args(args);
    set_environment(&mut process, &output.environment, &output.path)?;
    output.runtime.apply(&mut process)?;

    return system::run_interactive(&mut process);
}

/// A later variable wins over an earlier one, `path` goes in front of PATH
fn set_environment(process: &mut Command, environment: &[(String, String)], path: &[PathBuf]) -> Result<(), String> {
    for (variable, value) in environment {
        process.env(variable, value);
    }

    if path.is_empty() {
        return Ok(());
    }

    let current: OsString = environment.iter()
        .rev()
        .find(|(variable, _)| variable == "PATH")
        .map(|(_, value)| OsString::from(value))
        .or_else(|| std::env::var_os("PATH"))
        .unwrap_or_default();
    let directories = path.iter()
        .cloned()
        .chain(std::env::split_paths(&current));
    let joined = std::env::join_paths(directories).map_err(|error| format!("path: {error}"))?;
    process.env("PATH", joined);

    return Ok(());
}

/// "go build" of go with its arguments, for the log and errors: the first argument when it is a subcommand,
/// not an option or a path
fn what(command: &[String]) -> String {
    let program = Path::new(&command[0])
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    // go -C src mod tidy: the subcommand after the directory to change to
    let start = match command.get(1).map(String::as_str) {
        Some("-C") => 3,
        _ => 1,
    };

    // up to two words, like mod tidy of go and run build of npm, not options or paths
    let words: Vec<&str> = command.iter()
        .skip(start)
        .take(2)
        .map(String::as_str)
        .take_while(|word| !word.is_empty() && !word.starts_with('-') && !word.contains(['/', '\\', '.', ' ', '=']))
        .collect();

    return [program.as_str()].into_iter().chain(words).collect::<Vec<_>>().join(" ");
}

fn with_output(text: &str, output_dir: &Path) -> String {
    return text.replace(OUTPUT_PLACEHOLDER, &output_dir.display().to_string());
}

/// The strings of a command, a module by how it starts: a JVM program with java -jar, a command by its
/// run or first output, others by their artifact
fn command_line(values: Vec<Value>, output_dir: &Path) -> Result<Vec<String>, String> {
    let mut line = vec![];

    for value in values {
        match value {
            Value::String(text) => line.push(with_output(&text, output_dir)),
            Value::Module(module) => {
                let module = module.wait()
                    .map_err(|_| format!("{} it runs failed", module.function))?;
                line.extend(module_command(&module));
            }
            other => return Err(format!("internal error: a command with {}", other.type_of())),
        }
    }

    return Ok(line);
}

fn module_command(module: &BuiltModule) -> Vec<String> {
    let artifact = module.artifact.display().to_string();

    match &module.output {
        ModuleOutput::Jvm(output) => {
            if let Some(JvmLaunch::Jar { java }) = &output.launch {
                return vec![java.display().to_string(), "-jar".to_string(), artifact];
            }
        }
        ModuleOutput::Native(output) => {
            if let Some(program) = &output.program {
                return vec![program.display().to_string()];
            }
        }
        ModuleOutput::Command(output) if !output.run.is_empty() => return output.run.clone(),
        ModuleOutput::Command(_) | ModuleOutput::Bundle(_) => {}
    }

    return vec![artifact];
}

/// What the modules produced: a tool built again keeps its path, its content counts. All outputs of a
/// command, a generator may write several files.
fn module_files(values: &[Value]) -> Result<Vec<PathBuf>, String> {
    let mut files = vec![];

    for value in values {
        let Value::Module(module) = value else { continue };
        let module = module.wait()
            .map_err(|_| format!("{} it uses failed", module.function))?;

        match &module.output {
            ModuleOutput::Command(output) => files.extend(output.outputs.iter().cloned()),
            _ => files.push(module.artifact.clone()),
        }
    }

    return Ok(files);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(arguments: &[&str]) -> Vec<String> {
        return arguments.iter().map(|argument| argument.to_string()).collect();
    }

    #[test]
    fn files_are_embedded_as_bytes_or_as_the_words_of_spir_v() {
        let words = embedded_header("double.comp.spv", &[0x03, 0x02, 0x23, 0x07, 0x00, 0x06, 0x01, 0x00], true).unwrap();
        assert!(words.contains("static const uint32_t double_comp_spv[] = {\n    0x07230203, 0x00010600,\n};"), "{words}");
        assert!(words.contains("static const size_t double_comp_spv_size = 8;"), "{words}");

        let bytes = embedded_header("1font.ttf", b"ab", false).unwrap();
        assert!(bytes.contains("static const unsigned char _1font_ttf[] = {\n    0x61, 0x62,\n};"), "{bytes}");

        // an empty file is an array of one zero, its size 0
        let empty = embedded_header("empty", b"", false).unwrap();
        assert!(empty.contains("empty[] = {\n    0,\n};") && empty.contains("empty_size = 0;"), "{empty}");

        assert_eq!(embedded_header("odd.spv", b"abc", true).unwrap_err(), "odd.spv has 3 bytes, which are no words of 4");
    }

    #[test]
    fn libraries_are_named_before_their_last_at() {
        assert_eq!(library_name("github.com/spf13/cobra@v1.9.1"), "github.com/spf13/cobra");
        assert_eq!(library_name("@types/node@22.0.0"), "@types/node");
        assert_eq!(library_name("@types/node"), "@types/node");
        assert_eq!(library_name("serde"), "serde");
        assert_eq!(split_library("golang.org/x/text@v0.24.0"), ("golang.org/x/text", "v0.24.0"));
    }

    #[test]
    fn libraries_are_read_in_the_forms_tools_print_them() {
        let printed = |line: &str| printed_library(line);
        assert_eq!(printed("github.com/spf13/cobra@v1.9.1").as_deref(), Some("github.com/spf13/cobra@v1.9.1"));
        assert_eq!(printed("serde v1.0.215 (proc-macro)").as_deref(), Some("serde@v1.0.215"));
        assert_eq!(printed("@types/node 22.0.0").as_deref(), Some("@types/node@22.0.0"));
        assert_eq!(printed("  rich  ").as_deref(), Some("rich"));
        assert_eq!(printed("   "), None);

        // like a line of Cargo.toml, a version that is none is no library
        assert_eq!(printed("serde = \"1.0.229\"    # A generic serialization framework").as_deref(), Some("serde@1.0.229"));
        assert_eq!(printed("serde@="), None);
        assert_eq!(printed("serde = "), None);
        assert_eq!(printed("hello (*)"), None);

        // the version alone, like npm view prints it, is one of the library asked for
        let resolved = |name: &str, printed: &str| resolved_library(name, printed);
        assert_eq!(resolved("lodash", "4.17.21\n").as_deref(), Ok("lodash@4.17.21"));
        assert_eq!(resolved("@types/node", "\n22.0.0").as_deref(), Ok("@types/node@22.0.0"));
        assert_eq!(resolved("serde", "serde 1.0.215\n").as_deref(), Ok("serde@1.0.215"));
        assert_eq!(resolved("github.com/google/uuid", "github.com/google/uuid@v1.6.0").as_deref(), Ok("github.com/google/uuid@v1.6.0"));
        assert_eq!(resolved("serde", "serde = \"1.0.229\"    # A generic\n... and 26400 crates more").as_deref(), Ok("serde@1.0.229"));
        assert_eq!(resolved("serde-json", "serde_json = \"1.0.140\"").as_deref(), Ok("serde_json@1.0.140"));

        assert_eq!(resolved("x", "\n \n"), Err("printed no version of x".to_string()));
        assert_eq!(resolved("serde", "serde"), Err("printed no version of serde: serde".to_string()));
        assert_eq!(resolved("serde", "serde = \"\""), Err("printed no version of serde: serde = \"\"".to_string()));
        assert_eq!(resolved("serdee", "serde = \"1.0.229\""), Err("printed serde@1.0.229, which is no version of serdee".to_string()));

        assert!(same_version("v1.0.215", "1.0.215"));
        assert!(same_version("v1.9.1", "v1.9.1"));
        assert!(!same_version("v1.9.1", "v1.9.2"));
        assert!(!same_version("version", "ersion"));
    }

    #[test]
    fn what_a_command_took_is_checked_against_the_libraries_of_the_script() {
        let listed = line(&["a@v1", "b@v1", "unused@v3", "serde@1.0.215", "tokio"]);

        // a listed one without a version takes any, the v of cargo tree is no other version
        let notes = checked_libraries("m", &listed, "a@v1\n\nb@v2\nserde v1.0.215 (proc-macro)\ntokio v1.41.0\n").unwrap();
        assert_eq!(notes, [
            "uses b@v2 for b@v1 of its libraries",
            "lists unused@v3, which it does not use",
        ]);

        let error = checked_libraries("m", &listed, "a@v1\nc@v4\nd v5").unwrap_err();
        assert_eq!(error, "m uses c@v4, which its libraries do not list: xkit install c@v4\n\
                           m uses d@v5, which its libraries do not list: xkit install d@v5");

        assert_eq!(checked_libraries("m", &[], "").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn words_are_quoted_the_way_go_splits_them() {
        assert_eq!(quoted_words(&line(&["zig", "cc", "-target", "aarch64-linux-gnu"])).unwrap(), "zig cc -target aarch64-linux-gnu");
        assert_eq!(quoted_words(&line(&["-I/Users/a b/include", "-DNAME=\"x\""])).unwrap(), "'-I/Users/a b/include' '-DNAME=\"x\"'");
        assert_eq!(quoted_words(&line(&["-DQUOTE='q'", ""])).unwrap(), "\"-DQUOTE='q'\" ''");
        assert_eq!(quoted_words(&[]).unwrap(), "");
        assert!(quoted_words(&line(&["-D'\""])).unwrap_err().contains("quotes of both kinds"));
    }

    #[test]
    fn commands_are_named_by_their_subcommand_also_after_a_directory() {
        assert_eq!(what(&line(&["/go/bin/go", "-C", "src", "mod", "tidy"])), "go mod tidy");
        assert_eq!(what(&line(&["/go/bin/go", "build", "-o", "x"])), "go build");
        assert_eq!(what(&line(&["/go/bin/go", "test", "./..."])), "go test");
        assert_eq!(what(&line(&["npm", "run", "build", "--", "x"])), "npm run build");
        assert_eq!(what(&line(&["sh", "-c", "true"])), "sh");
    }
}
