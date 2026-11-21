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
use crate::language::runtime::{
    Arguments, BuildScriptContext, BuiltModule, FunctionParameters, ModuleOutput, ModuleTests, Placement, TestOutcome,
    TestRun, TestSuite, Type, Value, builtin,
};

/// stands for the directory of the command in its arguments: build/commands/<name>
const OUTPUT_PLACEHOLDER: &str = "{output}";

/// stands for the directory the tests write JUnit XML reports to, build-script counts them
const TEST_REPORTS_PLACEHOLDER: &str = "{test_reports}";

/// The files of `files` by the command and the target that write them: two writers would overwrite each other,
/// and run at the same time
static WRITERS: LazyLock<Mutex<HashMap<PathBuf, (String, String)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

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
            // view: what build install writes
            .with_default("resolve", Type::list(Type::String), Value::List(vec![]))
            .returns(Type::Module)
            // a program that writes a file once for every target, a compiler of Go once per target
            .placed(Placement::UnlessUnchanged)
            .running_on_host("command"),
        build_command,
    )];
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
    libraries: Vec<String>,
    direct_libraries: Vec<String>,
}

fn build_command(context: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
    let spec = CommandSpec::read(context, args)?;
    let always: bool = args.get("always")?;
    let target = context.target.map_or("this machine".to_string(), |target| target.name());
    claim_files(&spec, &target)?;

    let mut fingerprint = InputFingerprint::in_build_dir(&spec.build_dir, &format!("commands/{}", spec.name));
    fingerprint.add((&spec.command, &spec.outputs, &spec.environment, &spec.environment_inputs, &spec.path));
    fingerprint.add((&spec.files, &spec.libraries, &spec.direct_libraries));
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

    // the files it writes are next to the sources, where an editor may change what the command made of them
    let kept_outputs = spec.build_dir
        .join(up_to_date::FINGERPRINTS_DIR)
        .join(format!("commands/{}.outputs", spec.name));
    let outputs_changed = !spec.files.is_empty() && !outputs_as_left(&kept_outputs, &spec.outputs);

    if always || outputs_changed || !fingerprint.reports_up_to_date(&spec.name, &outputs)? {
        spec.run()?;

        if !spec.files.is_empty() {
            system::write_file(&kept_outputs, &format!("{:016x}", up_to_date::files_hash(&spec.outputs)?))?;
        }

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
        let Place { name, directory, build_dir, output_dir } = Place::of(context, args)?;

        let command = command_line(args.get("command")?, &output_dir)?;

        if command.is_empty() {
            return Err("the command is empty, its first element is the program".to_string());
        }

        let outputs = args.get::<Vec<String>>("outputs")?
            .iter()
            .map(|output| directory.join(with_output(output, &output_dir)))
            .collect();

        let environment = environment(args, &output_dir)?;

        let environment_inputs = args.get::<Vec<String>>("environment_inputs")?
            .into_iter()
            .map(|variable| {
                let value = std::env::var(&variable).ok();
                return (variable, value);
            })
            .collect();

        let path = path(args, &directory, &output_dir)?;

        let inputs = system::files_matching(&directory, &args.get::<Vec<String>>("inputs")?, &args.get::<Vec<String>>("exclude")?);

        let mut files = vec![];

        for file in args.get::<Vec<Vec<String>>>("files")? {
            let [path, content] = file.as_slice() else {
                return Err(format!("files takes file(path, content), not a list of {} strings", file.len()));
            };

            files.push((below(&directory, path)?, content.clone()));
        }

        let direct_libraries = command_line(args.get("direct_libraries")?, &output_dir)?;

        let mut module_values = args.get::<Vec<Value>>("command")?;
        module_values.extend(args.get::<Vec<Value>>("dependencies")?);

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
            module_files: module_files(&module_values)?,
            files,
            libraries: args.get("libraries")?,
            direct_libraries,
        });
    }

    /// The command with its directory and environment, `program` and `arguments` of it
    fn process(&self, program: &str, arguments: &[String]) -> Result<Command, String> {
        let mut process = Command::new(program);
        process.args(arguments).current_dir(&self.directory);
        set_environment(&mut process, &self.environment, &self.path)?;
        return Ok(process);
    }

    /// Into a fresh {output}: what a renamed source left there is gone. The files first, the libraries the
    /// command took once it ran.
    fn run(&self) -> Result<(), String> {
        let what = what(&self.command);
        system::recreate_dir(&self.output_dir)?;

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

/// NAME=value of `environment`, {output} replaced
fn environment(args: &Arguments, output_dir: &Path) -> Result<Vec<(String, String)>, String> {
    let mut environment = vec![];

    for entry in args.get::<Vec<String>>("environment")? {
        let Some((variable, value)) = entry.split_once('=') else {
            return Err(format!("environment takes NAME=value, not {entry}"));
        };

        environment.push((variable.to_string(), with_output(value, output_dir)));
    }

    return Ok(environment);
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
            None => unlisted.push(format!("{module} uses {library}, which its libraries do not list: build install {library}")),
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

/// What build install writes for `library` of the command `args` describe: what its `resolve` prints with
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
    set_environment(&mut process, &environment(args, &output_dir)?, &path(args, &directory, &output_dir)?)?;

    let printed = system::command_output(&mut process, &what(&line))?;

    return resolved_library(library_name, &printed)
        .map(Some)
        .map_err(|problem| format!("{} for {name} {problem}", what(&line)));
}

/// `build-script run` of a command: its `run` with the arguments after --, in its directory
pub fn launch(module: &BuiltModule, output: &CommandOutput, args: &[String], runner: &Runner) -> Result<i32, String> {
    let Some((program, arguments)) = output.run.split_first() else {
        return Err(format!("{} has nothing to run, build_command(run = ...) names what", module.name));
    };

    let mut process = runner.command(Path::new(program), &module.directory);
    process.args(arguments).args(args);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn line(arguments: &[&str]) -> Vec<String> {
        return arguments.iter().map(|argument| argument.to_string()).collect();
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
        assert_eq!(error, "m uses c@v4, which its libraries do not list: build install c@v4\n\
                           m uses d@v5, which its libraries do not list: build install d@v5");

        assert_eq!(checked_libraries("m", &[], "").unwrap(), Vec::<String>::new());
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
