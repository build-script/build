// build_command: a command with declared inputs and outputs as a module, what plugins build languages with.
// It runs again only when an input, a module it uses or its command line changed.
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use crate::common::system;
use crate::common::up_to_date::InputFingerprint;
use crate::jvm::JvmLaunch;
use crate::language::runtime::{
    Arguments, BuildScriptContext, BuiltModule, FunctionParameters, ModuleOutput, ModuleTests, TestOutcome, TestRun,
    TestSuite, Type, Value, builtin,
};

/// stands for the directory of the command in its arguments: build/commands/<name>
const OUTPUT_PLACEHOLDER: &str = "{output}";

/// stands for the directory the tests write JUnit XML reports to, build-script counts them
const TEST_REPORTS_PLACEHOLDER: &str = "{test_reports}";

/// What `build-script run` and the modules using it need of a command
#[derive(Debug, Hash)]
pub struct CommandOutput {
    /// {output}
    pub output_dir: PathBuf,
    pub outputs: Vec<PathBuf>,
    /// what `build-script run` starts, empty for nothing
    pub run: Vec<String>,
    /// for `run` as well
    pub environment: Vec<(String, String)>,
    pub path: Vec<PathBuf>,
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
            .strings("environment")
            .strings("environment_inputs")
            .strings("path")
            .with_default("directory", Type::String, ".")
            .with_default("run", command_line(), Value::List(vec![]))
            .with_default("test", command_line(), Value::List(vec![]))
            .with_default("test_timeout", Type::Int, Value::Int(0))
            .with_default("always", Type::Bool, false)
            .returns(Type::Module),
        build_command,
    )];
}

/// What the arguments say, with the placeholders replaced and the modules by their files
struct CommandSpec {
    name: String,
    directory: PathBuf,
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
}

fn build_command(context: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
    let spec = CommandSpec::read(context, args)?;
    let always: bool = args.get("always")?;

    let mut fingerprint = InputFingerprint::for_module(&spec.directory, &format!("commands/{}", spec.name));
    fingerprint.add((&spec.command, &spec.outputs, &spec.environment, &spec.environment_inputs, &spec.path));
    fingerprint.add_files(&spec.inputs)?;
    fingerprint.add_files(&spec.module_files)?;

    let artifact = spec.outputs
        .first()
        .cloned()
        .unwrap_or_else(|| spec.output_dir.clone());
    let outputs: Vec<&Path> = spec.outputs
        .iter()
        .map(PathBuf::as_path)
        .collect();

    if always || !fingerprint.reports_up_to_date(&spec.name, &outputs)? {
        spec.run()?;
        fingerprint.store_done(&spec.name, &artifact)?;
    }

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
            outputs: spec.outputs,
            run,
            environment: spec.environment,
            path: spec.path,
        }),
        tests,
    })));
}

impl CommandSpec {
    fn read(context: &BuildScriptContext, args: &Arguments) -> Result<Self, String> {
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

        let output_dir = system::build_dir(&directory).join("commands").join(&name);

        let command = command_line(args.get("command")?, &output_dir)?;

        if command.is_empty() {
            return Err("the command is empty, its first element is the program".to_string());
        }

        let outputs = args.get::<Vec<String>>("outputs")?
            .iter()
            .map(|output| directory.join(with_output(output, &output_dir)))
            .collect();

        let mut environment = vec![];

        for entry in args.get::<Vec<String>>("environment")? {
            let Some((variable, value)) = entry.split_once('=') else {
                return Err(format!("environment takes NAME=value, not {entry}"));
            };

            environment.push((variable.to_string(), with_output(value, &output_dir)));
        }

        let environment_inputs = args.get::<Vec<String>>("environment_inputs")?
            .into_iter()
            .map(|variable| {
                let value = std::env::var(&variable).ok();
                return (variable, value);
            })
            .collect();

        let path = args.get::<Vec<String>>("path")?
            .iter()
            .map(|directory_on_path| directory.join(with_output(directory_on_path, &output_dir)))
            .collect();

        let inputs = system::files_matching(&directory, &args.get::<Vec<String>>("inputs")?, &args.get::<Vec<String>>("exclude")?);

        let mut module_values = args.get::<Vec<Value>>("command")?;
        module_values.extend(args.get::<Vec<Value>>("dependencies")?);

        return Ok(CommandSpec {
            name,
            directory,
            output_dir,
            command,
            inputs,
            outputs,
            environment,
            environment_inputs,
            path,
            module_files: module_files(&module_values)?,
        });
    }

    /// The command with its directory and environment, `program` and `arguments` of it
    fn process(&self, program: &str, arguments: &[String]) -> Result<Command, String> {
        let mut process = Command::new(program);
        process.args(arguments).current_dir(&self.directory);
        set_environment(&mut process, &self.environment, &self.path)?;
        return Ok(process);
    }

    /// Into a fresh {output}: what a renamed source left there is gone
    fn run(&self) -> Result<(), String> {
        let what = what(&self.command);
        system::recreate_dir(&self.output_dir)?;
        system::log(&self.name, &format!("Running {what}"));

        let mut process = self.process(&self.command[0], &self.command[1..])?;
        system::run_command_in_one_piece(&mut process, &what)?;

        if let Some(missing) = self.outputs.iter().find(|output| !output.exists()) {
            return Err(format!("{what} did not write {}", system::relative_with_slashes(&self.directory, missing)));
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

        let mut fingerprint = InputFingerprint::for_module(&self.directory, &format!("commands/{}.tests", self.name));
        fingerprint.add((&test, &self.environment, &self.environment_inputs, &self.path));
        fingerprint.add_files(&self.inputs)?;
        fingerprint.add_files(&self.module_files)?;
        fingerprint.add_files(&self.outputs)?;

        if fingerprint.is_up_to_date(&[])? {
            system::log(&self.name, "Tests up to date");
            return Ok(ModuleTests::Known(TestOutcome::UpToDate));
        }

        let process = self.process(&test[0], &test[1..])?;
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

/// `build-script run` of a command: its `run` with the arguments after --, in its directory
pub fn launch(module: &BuiltModule, output: &CommandOutput, args: &[String]) -> Result<i32, String> {
    let Some((program, arguments)) = output.run.split_first() else {
        return Err(format!("{} has nothing to run, build_command(run = ...) names what", module.name));
    };

    let mut process = Command::new(program);
    process.args(arguments)
        .args(args)
        .current_dir(&module.directory);
    set_environment(&mut process, &output.environment, &output.path)?;

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

    return match command.get(1) {
        Some(first) if !first.starts_with('-') && !first.contains(['/', '\\', '.']) => format!("{program} {first}"),
        _ => program,
    };
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
        ModuleOutput::Command(_) => {}
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
