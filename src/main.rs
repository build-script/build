mod common;
mod jvm;
mod language;
mod native;
mod project;

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::settings::Settings;
use common::terminal;
use language::parser::Parser;
use language::runtime::{self, BuiltModule, Runtime, TestCounts, TestOutcome, TestSelection};

/// Every command and its arguments, in the order of the usage
const COMMANDS: &[(&str, &str)] = &[
    ("build", "[build] [<script>]"),
    ("idea", "idea [<script>]"),
    ("run", "run [--module <name>] [<script>] [-- <args>...]"),
    ("test", "test [--module <name>] [<script>]"),
    ("new", "new <kind> <path> [--package <package>]"),
    ("add", "add <kind> <name> [<script>] [--package <package>]"),
    ("install", "install <library> [<script>] [--module <name>] [<list>]"),
    ("clean", "clean [<script>]"),
    ("config", "config [<setting> [<value>]] | config --unset <setting>"),
];

/// What take_build_options takes before the commands get their arguments
const BUILD_OPTIONS: &[&str] = &["--jobs", "--refresh", "--release"];

const BUILD_OPTIONS_USAGE: &str = "build, run and test take --jobs <n>, the number of modules built at the same time;
they and install take --refresh, which asks the repositories again for the newest
versions and SNAPSHOTs; build and run take --release, which builds Android apps for release";

/// what commands without a script argument build
const DEFAULT_SCRIPT: &str = "project.build";

#[derive(Debug, PartialEq)]
enum Command {
    /// the usage of every command and what they do, or the usage of one
    Help {
        command: Option<&'static str>,
    },
    Build {
        script: String,
    },
    /// builds, sets up the project for IntelliJ, later builds keep it current
    Idea {
        script: String,
    },
    /// builds, then starts a module, the last startable one by default
    Run {
        script: String,
        module: Option<String>,
        args: Vec<String>,
    },
    /// builds and runs the tests of every module, or of one
    Test {
        script: String,
        module: Option<String>,
    },
    /// a new project directory, like cargo new, what is missing is asked in the terminal
    New {
        kind: Option<String>,
        path: Option<String>,
        package: Option<String>,
    },
    /// a new module next to the script, with its build call in the script
    Add {
        kind: Option<String>,
        name: Option<String>,
        script: String,
        package: Option<String>,
    },
    /// a Maven library into a list of a module's build call, `parameter` names the list
    Install {
        library: Option<String>,
        script: String,
        module: Option<String>,
        parameter: &'static str,
    },
    /// removes the build directories of the script's modules
    Clean {
        script: String,
    },
    /// shows the settings, one of them, sets or removes one
    Config {
        setting: Option<String>,
        value: Option<String>,
        unset: bool,
    },
}

/// Every command, then the options of those that build
fn usage() -> String {
    let lines: Vec<String> = COMMANDS.iter()
        .enumerate()
        .map(|(index, (_, arguments))| format!("{} build-script {arguments}", if index == 0 { "usage:" } else { "      " }))
        .collect();

    return format!("{}\n{BUILD_OPTIONS_USAGE}", lines.join("\n"));
}

/// The usage of one command, below it what its arguments may be
fn command_usage(command: &str) -> String {
    let arguments = COMMANDS.iter()
        .find(|(name, _)| *name == command)
        .map_or("", |(_, arguments)| arguments);
    let lists: Vec<&str> = project::DEPENDENCY_KINDS.iter()
        .map(|(flag, _)| *flag)
        .collect();
    let settings: Vec<&str> = project::SETTINGS.iter()
        .map(|(name, _)| *name)
        .collect();

    let choices = match command {
        "new" => vec![format!("<kind>: {}", project::project_kinds().join(", "))],
        "add" => vec![format!("<kind>: {}", project::module_kinds().join(", "))],
        "install" => vec![
            "<library>: group:artifact[:version] of Maven, or a vcpkg port for a C or C++ module".to_string(),
            format!("<list>: {}, without one dependencies", lists.join(", ")),
        ],
        "config" => vec![format!("<setting>: {}", settings.join(", "))],
        _ => vec![],
    };

    let mut lines = vec![format!("usage: build-script {arguments}")];
    lines.extend(choices.iter().map(|line| format!("       {line}")));

    return lines.join("\n");
}

/// The command, or what is wrong with the arguments and the usage of the command
fn parse_args(args: &[String]) -> Result<Command, String> {
    let Some((first, rest)) = args.split_first() else {
        return Ok(Command::Help { command: None });
    };

    let names: Vec<&'static str> = COMMANDS.iter()
        .map(|(name, _)| *name)
        .collect();

    let no_command = |word: &str| match word.starts_with('-') {
        true => format!("build-script has no option {word}{}\n{}", suggestion(word, BUILD_OPTIONS), usage()),
        false => format!("{word} is no command and no script{}\n{}", suggestion(word, &names), usage()),
    };

    if matches!(first.as_str(), "help" | "--help" | "-h") {
        return match rest {
            [] => Ok(Command::Help { command: None }),
            [name] => match names.iter().find(|known| *known == name) {
                Some(command) => Ok(Command::Help { command: Some(command) }),
                None => Err(no_command(name)),
            },
            _ => Err(format!("help takes one command at most\n{}", usage())),
        };
    }

    let Some(command) = names.iter().copied().find(|name| name == first) else {
        // a script alone is built, a word that is no file is most likely a misspelled command
        let is_script = !first.starts_with('-') && (first.contains(['.', '/']) || Path::new(first).exists());

        return match (is_script, rest) {
            (true, []) => Ok(Command::Build { script: first.clone() }),
            (true, _) => Err(format!("{first} is a script, the command goes before it\n{}", usage())),
            (false, _) => Err(no_command(first)),
        };
    };

    // the arguments of a program after -- are its own
    let own = &rest[..rest.iter().position(|arg| arg == "--").unwrap_or(rest.len())];

    if own.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Ok(Command::Help { command: Some(command) });
    }

    let wrong = |problem: String| format!("{problem}\n{}", command_usage(command));

    // an option the command does not have, most likely misspelled
    let without_unknown_options = |positional: Vec<String>, options: &[&str]| -> Result<Vec<String>, String> {
        let Some(option) = positional.iter().find(|arg| arg.starts_with('-')) else {
            return Ok(positional);
        };

        let known = [options, BUILD_OPTIONS].concat();
        return Err(wrong(format!("{command} has no option {option}{}", suggestion(option, &known))));
    };

    let script_or_default = |positional: &[String]| match positional {
        [] => Ok(DEFAULT_SCRIPT.to_string()),
        [script] => Ok(script.clone()),
        _ => Err(wrong(format!("{command} takes one script at most, not {}", positional.join(" ")))),
    };

    match command {
        "build" | "idea" | "clean" => {
            let script = script_or_default(&without_unknown_options(rest.to_vec(), &[])?)?;

            return Ok(match command {
                "build" => Command::Build { script },
                "idea" => Command::Idea { script },
                _ => Command::Clean { script },
            });
        }
        "run" => {
            let (module, positional) = take_option(own, "--module").map_err(wrong)?;
            let positional = without_unknown_options(positional, &["--module"])?;

            return Ok(Command::Run {
                script: script_or_default(&positional)?,
                module,
                args: rest.get(own.len() + 1..).unwrap_or_default().to_vec(),
            });
        }
        "test" => {
            let (module, positional) = take_option(rest, "--module").map_err(wrong)?;
            let positional = without_unknown_options(positional, &["--module"])?;

            return Ok(Command::Test {
                script: script_or_default(&positional)?,
                module,
            });
        }
        "new" => {
            let (package, positional) = take_option(rest, "--package").map_err(wrong)?;
            let positional = without_unknown_options(positional, &["--package"])?;

            if positional.len() > 2 {
                return Err(wrong(format!("new takes a kind and a path, not {}", positional.join(" "))));
            }

            return Ok(Command::New {
                kind: positional.first().cloned(),
                path: positional.get(1).cloned(),
                package,
            });
        }
        "add" => {
            let (package, positional) = take_option(rest, "--package").map_err(wrong)?;
            let positional = without_unknown_options(positional, &["--package"])?;

            return Ok(Command::Add {
                kind: positional.first().cloned(),
                name: positional.get(1).cloned(),
                script: script_or_default(positional.get(2..).unwrap_or_default())?,
                package,
            });
        }
        "install" => {
            let (module, args) = take_option(rest, "--module").map_err(wrong)?;
            let is_list = |arg: &String| project::DEPENDENCY_KINDS.iter().any(|(flag, _)| flag == arg);

            // a flag names the list the library goes to
            let (lists, positional): (Vec<String>, Vec<String>) = args.into_iter().partition(is_list);
            let flags: Vec<&str> = project::DEPENDENCY_KINDS.iter()
                .map(|(flag, _)| *flag)
                .collect();
            let positional = without_unknown_options(positional, &[&flags[..], &["--module"]].concat())?;

            let parameter = match lists.as_slice() {
                [] => project::DEPENDENCIES,
                [list] => project::DEPENDENCY_KINDS.iter()
                    .find(|(flag, _)| flag == list)
                    .map_or(project::DEPENDENCIES, |(_, parameter)| *parameter),
                _ => return Err(wrong(format!("install takes one list, not {}", lists.join(" ")))),
            };

            return Ok(Command::Install {
                library: positional.first().cloned(),
                script: script_or_default(positional.get(1..).unwrap_or_default())?,
                module,
                parameter,
            });
        }
        "config" => {
            let unset = rest.iter().any(|arg| arg == "--unset");
            let positional = rest.iter()
                .filter(|arg| *arg != "--unset")
                .cloned()
                .collect();
            let positional = without_unknown_options(positional, &["--unset"])?;

            // config --unset asks which setting
            return match (positional.as_slice(), unset) {
                ([], _) => Ok(Command::Config {
                    setting: None,
                    value: None,
                    unset,
                }),
                ([setting], _) => Ok(Command::Config {
                    setting: Some(setting.clone()),
                    value: None,
                    unset,
                }),
                ([setting, value], false) => Ok(Command::Config {
                    setting: Some(setting.clone()),
                    value: Some(value.clone()),
                    unset,
                }),
                _ => Err(wrong(format!("config takes a setting and its value at most, not {}", rest.join(" ")))),
            };
        }
        _ => return Err(no_command(command)),
    }
}

/// ", did you mean build?" for biuld: the known word the fewest edits away, none when all are too far
fn suggestion(word: &str, known: &[&str]) -> String {
    // Levenshtein, a row of the table at a time
    let distance = |other: &str| {
        let other: Vec<char> = other.chars().collect();
        let mut row: Vec<usize> = (0..=other.len()).collect();

        for (index, char) in word.chars().enumerate() {
            let mut diagonal = row[0];
            row[0] = index + 1;

            for (position, other_char) in other.iter().enumerate() {
                let replaced = diagonal + usize::from(char != *other_char);
                diagonal = row[position + 1];
                row[position + 1] = replaced.min(diagonal + 1).min(row[position] + 1);
            }
        }

        return row[other.len()];
    };

    // the dashes of options alike in all of them
    let most_edits = word.trim_start_matches('-').chars().count() / 2;

    return known.iter()
        .map(|candidate| (distance(candidate), candidate))
        .filter(|(distance, _)| *distance <= most_edits)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| format!(", did you mean {candidate}?"))
        .unwrap_or_default();
}

/// An argument the command was given, else chosen or typed in a terminal. Without one it exits with what
/// is missing and the usage of the command. With `choices` a given one is one of them.
fn argument(command: &str, given: Option<&str>, label: &str, choices: &[&str]) -> String {
    if let Some(value) = given.filter(|value| choices.is_empty() || choices.contains(value)) {
        return value.to_string();
    }

    let problem = match given {
        Some(value) => format!("{value} is no {label}{}", suggestion(value, choices)),
        None => format!("{command} needs the {label}"),
    };

    if !io::stdin().is_terminal() {
        eprintln!("{problem}\n{}", command_usage(command));
        std::process::exit(2);
    }

    if given.is_some() {
        println!("{problem}");
    }

    // "kind of project" asks "Kind of project: "
    let question = label[..1].to_uppercase() + &label[1..];

    return exit_on_error(match choices.is_empty() {
        true => terminal::ask(&question),
        false => terminal::choose(&question, choices, given.unwrap_or("")),
    });
}

/// `name value` anywhere in the arguments: the value, and the arguments without both
fn take_option(args: &[String], name: &str) -> Result<(Option<String>, Vec<String>), String> {
    let Some(index) = args.iter().position(|arg| arg == name) else {
        return Ok((None, args.to_vec()));
    };

    let value = args.get(index + 1)
        .filter(|value| !value.starts_with('-'))
        .ok_or_else(|| format!("{name} needs a value"))?;

    let rest = args.iter()
        .enumerate()
        .filter(|(position, _)| *position != index && *position != index + 1)
        .map(|(_, arg)| arg.clone())
        .collect();

    return Ok((Some(value.clone()), rest));
}

fn help() -> String {
    return format!(
        "
  build   builds every module of the script, modules that did not change are up to date
  idea    builds and sets the project up for IntelliJ: a module file per module in .idea,
          every build after keeps them current
  run     builds, then starts a module: the last one with a main_class or the last
          Android app, or the one --module names; arguments after -- go to the program
  test    builds, then compiles and runs the tests of every module, or of one; failing
          tests do not stop the others, a summary of all of them follows
  clean   removes the build directories build-script wrote below the script's
          directory, the downloads stay
  new     creates a project in a new directory: {}
  add     adds a module in a directory next to the script and its build call to the
          script: {}
  install adds a Maven library to the dependencies of the module, the newest version
          unless one is given, once it resolved for the module, or a vcpkg port to a C or
          C++ module; --module picks one in scripts with several, these pick another
          list than dependencies:
          {}
  config  shows or changes what new and add remember, like your domain:
          {}

The script is {DEFAULT_SCRIPT} in the current directory unless one is given, see README.md.",
        project::project_kinds().join(", "),
        project::module_kinds().join(", "),
        project::DEPENDENCY_KINDS
            .iter()
            .map(|(flag, _)| *flag)
            .collect::<Vec<_>>()
            .join(", "),
        project::SETTINGS
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(", "),
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let (options, args) = take_build_options(&args).unwrap_or_else(|error| {
        eprintln!("{error}");
        std::process::exit(2);
    });

    let command = parse_args(&args).unwrap_or_else(|usage| {
        eprintln!("{usage}");
        std::process::exit(2);
    });

    // tests run against debug builds, like with the Android Gradle plugin
    if options.release && !matches!(command, Command::Build { .. } | Command::Run { .. }) {
        eprintln!("--release only goes with build and run, tests run against debug builds");
        std::process::exit(2);
    }

    if options.refresh {
        jvm::maven::refresh();
    }

    let script_path = match &command {
        Command::Help { command: None } => {
            println!("{}\n{}", usage(), help());
            return;
        }
        Command::Help {
            command: Some(command),
        } => {
            println!("{}", command_usage(command));
            return;
        }
        Command::New {
            kind,
            path,
            package,
        } => {
            let kind = argument("new", kind.as_deref(), "kind of project", &project::project_kinds());
            let path = argument("new", path.as_deref(), "path of the project", &[]);

            exit_on_error(project::new_project(
                &kind,
                Path::new(&path),
                package.as_deref(),
                project::domain,
            ));

            return;
        }
        Command::Add {
            kind,
            name,
            script,
            package,
        } => {
            let kind = argument("add", kind.as_deref(), "kind of module", &project::module_kinds());
            let name = argument("add", name.as_deref(), "name of the module", &[]);

            exit_on_error(project::add_module(
                &kind,
                &name,
                Path::new(script),
                package.as_deref(),
                project::domain,
            ));

            return;
        }
        Command::Config {
            setting,
            value,
            unset,
        } => {
            let settings: Vec<&str> = project::SETTINGS.iter()
                .map(|(name, _)| *name)
                .collect();

            // config alone shows them all, config --unset asks which
            let setting = match (setting, unset) {
                (None, false) => None,
                _ => Some(argument("config", setting.as_deref(), "setting", &settings)),
            };

            exit_on_error(config(setting.as_deref(), value.as_deref(), *unset));

            return;
        }
        Command::Install {
            library,
            script,
            module,
            parameter,
        } => {
            let library = argument(
                "install",
                library.as_deref(),
                "library, group:artifact[:version] or a vcpkg port",
                &[],
            );

            exit_on_error(project::install(
                &library,
                Path::new(script),
                module.as_deref(),
                parameter,
            ));

            return;
        }
        Command::Clean { script } => {
            exit_on_error(project::clean(Path::new(script)));

            return;
        }
        Command::Build { script }
        | Command::Idea { script }
        | Command::Run { script, .. }
        | Command::Test { script, .. } => script.as_str(),
    };

    let tests = match &command {
        Command::Test {
            module: Some(module),
            ..
        } => TestSelection::Module(module.clone()),
        Command::Test { module: None, .. } => TestSelection::All,
        _ => TestSelection::None,
    };

    let jobs = options.jobs.unwrap_or_else(runtime::default_jobs);
    let set_up_idea = matches!(command, Command::Idea { .. });
    let modules = run_script(script_path, tests, jobs, options.release, set_up_idea);

    match &command {
        Command::Run { module, args, .. } => {
            std::process::exit(run_module(&modules, module.as_deref(), args))
        }
        Command::Test { module, .. } => std::process::exit(run_tests(&modules, module.as_deref())),
        _ => {}
    }
}

/// What the commands that build take besides their own arguments
#[derive(Debug, Default, PartialEq)]
struct BuildOptions {
    /// modules built at the same time
    jobs: Option<usize>,
    /// SNAPSHOTs are looked up again instead of once a day
    refresh: bool,
    /// Android apps are built for release
    release: bool,
}

/// `--jobs <n>`, `--refresh` and `--release`, before the arguments of a program after `--`
fn take_build_options(args: &[String]) -> Result<(BuildOptions, Vec<String>), String> {
    let program_args = args.iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let (jobs, own) = take_option(&args[..program_args], "--jobs")?;

    let refresh = own.iter().any(|arg| arg == "--refresh");
    let release = own.iter().any(|arg| arg == "--release");

    let mut own: Vec<String> = own.into_iter()
        .filter(|arg| arg != "--refresh" && arg != "--release")
        .collect();
    own.extend(args[program_args..].iter().cloned());

    let jobs = match jobs {
        Some(jobs) => Some(
            jobs.parse::<usize>()
                .ok()
                .filter(|jobs| *jobs > 0)
                .ok_or_else(|| format!("--jobs needs a number above 0, not {jobs}"))?,
        ),
        None => None,
    };

    return Ok((
        BuildOptions {
            jobs,
            refresh,
            release,
        },
        own,
    ));
}

/// The modules the script built, the builds of independent modules ran in parallel
fn run_script(
    script_path: &str,
    tests: TestSelection,
    jobs: usize,
    release: bool,
    set_up_idea: bool,
) -> Vec<Arc<BuiltModule>> {
    let source = std::fs::read_to_string(script_path).unwrap_or_else(|error| {
        match error.kind() {
            io::ErrorKind::NotFound if script_path == DEFAULT_SCRIPT => eprintln!(
                "there is no {DEFAULT_SCRIPT} in this directory: give the script, or create a project with build-script new"
            ),
            io::ErrorKind::NotFound => eprintln!("there is no script {script_path}"),
            _ => eprintln!("could not read {script_path}: {error}"),
        }
        std::process::exit(1);
    });

    let script = match Parser::new(&source).parse_script() {
        Ok(script) => script,
        Err(error) => {
            let (line, column) = line_column(&source, error.span.start);
            eprintln!("{script_path}:{line}:{column}: {}", error.variant);
            std::process::exit(1);
        }
    };

    // relative paths in the script are resolved against the script's directory
    let project_dir = Path::new(script_path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(|parent| parent.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    // once `build-script idea` set it up, every build keeps IntelliJ current, release builds leave it
    let marker = project_dir.join(jvm::idea::MARKER);

    if set_up_idea {
        exit_on_error(common::system::write_file(&marker, jvm::idea::MARKER_TEXT));
    }

    let idea = !release && marker.is_file();

    let mut runtime = Runtime::new(project_dir.clone())
        .with_tests(tests)
        .with_jobs(jobs)
        .with_release(release)
        .with_idea(idea);
    let evaluated = runtime.run(&script);

    // builds that started before an error finish, none of their tools keep running
    let built = runtime.finish();

    // a failed build keeps the module files of every module
    let worked = built.as_deref()
        .ok()
        .filter(|_| evaluated.is_ok());

    if idea && let Err(error) = jvm::idea::write_project(&project_dir, worked) {
        eprintln!("could not write the IntelliJ project: {error}");
    }

    if set_up_idea && worked.is_some() {
        println!(
            "Open {} in IntelliJ. A JDK it does not know yet, like temurin-21, is in {}: add it under Project Structure > SDKs with that name.",
            std::fs::canonicalize(&project_dir)
                .unwrap_or(project_dir)
                .display(),
            common::system::data_dir("jdks").display()
        );
    }

    let errors = match (evaluated, built) {
        (Ok(()), Ok(modules)) => return modules,
        (Err(error), _) => vec![error],
        (Ok(()), Err(errors)) => errors,
    };

    for error in errors {
        let (line, column) = line_column(&source, error.span.start);
        eprintln!("{script_path}:{line}:{column}: {error}");
    }

    std::process::exit(1);
}

/// The settings file and what it holds, or one setting shown, set or removed
fn config(setting: Option<&str>, value: Option<&str>, unset: bool) -> Result<(), String> {
    let mut settings = Settings::load()?;

    let Some(setting) = setting else {
        println!("{}", settings.file().display());
        for (name, meaning) in project::SETTINGS {
            match settings.get(name) {
                Some(value) => println!("  {name} = {value}"),
                None => println!("  {name} is not set ({meaning})"),
            }
        }

        return Ok(());
    };

    if !project::SETTINGS.iter().any(|(name, _)| *name == setting) {
        // the same message as for a value
        return project::checked_setting(setting, "").map(|_| ());
    }

    match (value, unset) {
        (Some(value), _) => {
            let value = project::checked_setting(setting, value)?;
            settings.set(setting, &value)?;
            println!("{setting} = {value}");
        }
        (None, true) if settings.remove(setting)? => println!("Removed {setting}"),
        (None, true) => println!("{setting} was not set"),
        (None, false) => match settings.get(setting) {
            Some(value) => println!("{value}"),
            None => println!("{setting} is not set"),
        },
    }

    return Ok(());
}

fn exit_on_error<T>(result: Result<T, String>) -> T {
    match result {
        Ok(value) => return value,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

/// Line and column of a byte offset in the script, both counted from 1
fn line_column(source: &str, offset: usize) -> (usize, usize) {
    let before = source.get(..offset).unwrap_or(source);
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;

    return (line, column);
}

/// Starts a module the script built and returns the exit code for the process
fn run_module(built: &[Arc<BuiltModule>], name: Option<&str>, args: &[String]) -> i32 {
    let module = match name {
        Some(name) => built.iter().rev().find(|module| module.name == name),
        None => built.iter().rev().find(|module| module.can_launch()),
    };

    let Some(module) = module else {
        match name {
            Some(name) => eprintln!("run: {}", no_module_named(built, name)),
            None => eprintln!(
                "run: the script built nothing to run (build_kotlin with a main_class, or build_android)"
            ),
        }
        return 1;
    };

    if module.can_launch() {
        println!("> Running {}", module.name);
    }

    match runtime::launch(module, args) {
        Ok(code) => return code,
        Err(error) => {
            eprintln!("run: {error}");
            return 1;
        }
    }
}

/// Runs the tests of every selected module once all are built, in build order.
/// Exit code 1 if the tests of one failed.
fn run_tests(built: &[Arc<BuiltModule>], name: Option<&str>) -> i32 {
    let selected: Vec<_> = built.iter()
        .filter(|module| name.is_none_or(|name| module.name == name))
        .collect();

    match name {
        Some(name) if selected.is_empty() => {
            eprintln!("test: {}", no_module_named(built, name));
            return 1;
        }
        None if selected.is_empty() => {
            println!("> No modules to test");
            return 0;
        }
        _ => {}
    }

    let mut outcomes: Vec<(String, Option<TestOutcome>)> = vec![];

    for module in selected {
        if module.tests.is_empty() {
            outcomes.push((module.name.clone(), None));
            continue;
        }

        let ran: Vec<(String, TestOutcome)> = module.tests.iter()
            .map(|suite| {
                (
                    suite_label(&module.name, suite.source_set),
                    suite.tests.run(&module.name),
                )
            })
            .collect();

        // source sets without tests only show when none has any
        if ran.iter()
            .all(|(_, outcome)| *outcome == TestOutcome::NoTests)
        {
            outcomes.push((module.name.clone(), Some(TestOutcome::NoTests)));
        } else {
            outcomes.extend(
                ran.into_iter()
                    .filter(|(_, outcome)| *outcome != TestOutcome::NoTests)
                    .map(|(label, outcome)| (label, Some(outcome))),
            );
        }
    }

    println!("{}", test_summary(&outcomes));
    let failed = outcomes.iter()
        .any(|(_, outcome)| matches!(outcome, Some(TestOutcome::Failed { .. })));

    return if failed { 1 } else { 0 };
}

/// The unit tests in src/test go by the module's name, others by their source set too: app (androidTest)
fn suite_label(module: &str, source_set: &str) -> String {
    if source_set == "test" {
        return module.to_string();
    }
    return format!("{module} ({source_set})");
}

fn no_module_named(built: &[Arc<BuiltModule>], name: &str) -> String {
    let built: Vec<&str> = built.iter()
        .map(|module| module.name.as_str())
        .collect();

    if built.is_empty() {
        return format!("the script built no module named {name}, it built none");
    }

    return format!(
        "the script built no module named {name}, only {}",
        built.join(", ")
    );
}

/// A line per module, then how many modules ended how
fn test_summary(outcomes: &[(String, Option<TestOutcome>)]) -> String {
    let width = outcomes.iter()
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(0);

    let mut lines = vec!["> Tests".to_string()];

    // in the order of the totals line
    let mut totals = [
        ("failed", 0),
        ("passed", 0),
        ("up to date", 0),
        ("not supported", 0),
        ("without tests", 0),
    ];

    for (name, outcome) in outcomes {
        let (total, description) = match outcome {
            Some(TestOutcome::Failed {
                counts: Some(counts),
                ..
            }) if counts.failed > 0 => (
                0,
                format!(
                    "FAILED, {} of {}",
                    counts.failed,
                    plural(counts.tests, "test")
                ),
            ),
            Some(TestOutcome::Failed { error, .. }) => (0, format!("FAILED: {error}")),
            Some(TestOutcome::Passed(Some(counts))) => {
                (1, format!("passed, {}", test_counts(counts)))
            }
            Some(TestOutcome::Passed(None)) => (1, "passed".to_string()),
            Some(TestOutcome::UpToDate) => (2, "up to date".to_string()),
            // a kind of module that knows nothing about tests
            None => (
                3,
                "not supported: tests of this kind of module do not run".to_string(),
            ),
            Some(TestOutcome::NoTests) => (4, "no tests".to_string()),
        };

        totals[total].1 += 1;
        lines.push(format!("  {name:width$}  {description}"));
    }

    let totals: Vec<String> = totals.iter()
        .filter(|(_, count)| *count > 0)
        .map(|(what, count)| format!("{count} {what}"))
        .collect();

    lines.push(totals.join(", "));

    return lines.join("\n");
}

fn test_counts(counts: &TestCounts) -> String {
    let tests = plural(counts.tests, "test");

    if counts.skipped > 0 {
        return format!("{tests}, {} skipped", counts.skipped);
    }

    return tests;
}

fn plural(count: u32, word: &str) -> String {
    return format!("{count} {word}{}", if count == 1 { "" } else { "s" });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, String> {
        let args: Vec<String> = args.iter()
            .map(|arg| arg.to_string())
            .collect();
        return parse_args(&args);
    }

    #[test]
    fn commands_are_parsed() {
        let script = "app/project.build".to_string();

        assert_eq!(parse(&[]), Ok(Command::Help { command: None }));
        assert_eq!(
            parse(&["config"]),
            Ok(Command::Config {
                setting: None,
                value: None,
                unset: false
            })
        );
        assert_eq!(
            parse(&["config", "domain", "mvmo.dev"]),
            Ok(Command::Config {
                setting: Some("domain".to_string()),
                value: Some("mvmo.dev".to_string()),
                unset: false
            })
        );
        assert_eq!(
            parse(&["config", "--unset", "domain"]),
            Ok(Command::Config {
                setting: Some("domain".to_string()),
                value: None,
                unset: true
            })
        );
        assert!(parse(&["config", "a", "b", "c"]).is_err());
        assert_eq!(
            parse(&["clean"]),
            Ok(Command::Clean {
                script: DEFAULT_SCRIPT.to_string()
            })
        );
        assert_eq!(
            parse(&["app/project.build"]),
            Ok(Command::Build {
                script: script.clone()
            })
        );
        assert_eq!(
            parse(&["build", "app/project.build"]),
            Ok(Command::Build {
                script: script.clone()
            })
        );
        assert_eq!(
            parse(&["run", "app/project.build"]),
            Ok(Command::Run {
                script: script.clone(),
                module: None,
                args: vec![]
            }),
        );
        assert_eq!(
            parse(&[
                "run",
                "--module",
                "core",
                "app/project.build",
                "--",
                "a",
                "--b"
            ]),
            Ok(Command::Run {
                script: script.clone(),
                module: Some("core".to_string()),
                args: vec!["a".to_string(), "--b".to_string()]
            }),
        );

        assert_eq!(
            parse(&["test", "app/project.build"]),
            Ok(Command::Test {
                script: script.clone(),
                module: None
            })
        );
        assert_eq!(
            parse(&["test", "--module", "core", "app/project.build"]),
            Ok(Command::Test {
                script,
                module: Some("core".to_string())
            }),
        );

        assert_eq!(parse(&["--help"]), Ok(Command::Help { command: None }));
        assert!(parse(&["--verbose"]).is_err());
        assert!(parse(&["build", "a", "b"]).is_err());
        assert!(parse(&["run", "--module"]).is_err());

        // without a script, project.build of the current directory
        let default = DEFAULT_SCRIPT.to_string();
        assert_eq!(
            parse(&["build"]),
            Ok(Command::Build {
                script: default.clone()
            })
        );
        assert_eq!(
            parse(&["run", "--", "x"]),
            Ok(Command::Run {
                script: default.clone(),
                module: None,
                args: vec!["x".to_string()]
            })
        );
        assert_eq!(
            parse(&["test", "--module", "core"]),
            Ok(Command::Test {
                script: default.clone(),
                module: Some("core".to_string())
            })
        );

        assert_eq!(
            parse(&["new", "multi", "apps/demo", "--package", "org.acme"]),
            Ok(Command::New {
                kind: Some("multi".to_string()),
                path: Some("apps/demo".to_string()),
                package: Some("org.acme".to_string())
            }),
        );
        assert_eq!(
            parse(&["add", "kotlin-library", "core"]),
            Ok(Command::Add {
                kind: Some("kotlin-library".to_string()),
                name: Some("core".to_string()),
                script: default,
                package: None
            }),
        );
        assert_eq!(
            parse(&[
                "add",
                "--package",
                "org.acme.ui",
                "android-library",
                "ui",
                "app/project.build"
            ]),
            Ok(Command::Add {
                kind: Some("android-library".to_string()),
                name: Some("ui".to_string()),
                script: "app/project.build".to_string(),
                package: Some("org.acme.ui".to_string())
            }),
        );

        assert_eq!(
            parse(&["install", "org.spigotmc:spigot-api", "--compile-only", "--module", "plugin"]),
            Ok(Command::Install {
                library: Some("org.spigotmc:spigot-api".to_string()),
                script: DEFAULT_SCRIPT.to_string(),
                module: Some("plugin".to_string()),
                parameter: "compile_only"
            }),
        );
        assert_eq!(
            parse(&["install", "com.google.code.gson:gson:2.14.0", "app/project.build"]),
            Ok(Command::Install {
                library: Some("com.google.code.gson:gson:2.14.0".to_string()),
                script: "app/project.build".to_string(),
                module: None,
                parameter: "dependencies"
            }),
        );
        assert!(parse(&["install", "a:b", "--test", "--ksp"]).is_err());
        assert!(parse(&["install", "a:b", "--shade"]).is_err());
    }

    #[test]
    fn missing_arguments_are_asked_for_later() {
        assert_eq!(
            parse(&["new"]),
            Ok(Command::New {
                kind: None,
                path: None,
                package: None
            })
        );
        assert_eq!(
            parse(&["new", "cpp"]),
            Ok(Command::New {
                kind: Some("cpp".to_string()),
                path: None,
                package: None
            })
        );
        assert_eq!(
            parse(&["config", "--unset"]),
            Ok(Command::Config {
                setting: None,
                value: None,
                unset: true
            })
        );
        assert!(matches!(parse(&["install"]), Ok(Command::Install { library: None, .. })));
    }

    #[test]
    fn errors_say_what_is_wrong_and_show_the_usage_of_the_command() {
        let error = |args: &[&str]| parse(args).unwrap_err();

        assert!(error(&["biuld"]).starts_with("biuld is no command and no script, did you mean build?\nusage: build-script [build]"));
        assert!(error(&["--verbose"]).starts_with("build-script has no option --verbose\nusage:"));
        assert!(error(&["app/project.build", "x"]).starts_with("app/project.build is a script, the command goes before it"));
        assert_eq!(
            error(&["build", "--relase"]),
            "build has no option --relase, did you mean --release?\nusage: build-script [build] [<script>]"
        );
        assert_eq!(
            error(&["run", "--module"]),
            "--module needs a value\nusage: build-script run [--module <name>] [<script>] [-- <args>...]"
        );
        assert!(error(&["new", "cpp", "a", "b"]).starts_with("new takes a kind and a path, not cpp a b\nusage: build-script new"));
        assert!(error(&["install", "a:b", "--test", "--ksp"]).starts_with("install takes one list, not --test --ksp\n"));
        assert!(error(&["install", "a:b", "--tset"]).starts_with("install has no option --tset, did you mean --test?"));

        assert_eq!(
            command_usage("new"),
            format!(
                "usage: build-script new <kind> <path> [--package <package>]\n       <kind>: {}",
                project::project_kinds().join(", ")
            )
        );
        assert_eq!(suggestion("kotln", &project::project_kinds()), ", did you mean kotlin?");
        assert_eq!(suggestion("x", &["c", "cpp"]), "");
    }

    #[test]
    fn help_shows_the_usage_of_one_command() {
        assert_eq!(parse(&["new", "--help"]), Ok(Command::Help { command: Some("new") }));
        assert_eq!(parse(&["help", "install"]), Ok(Command::Help { command: Some("install") }));
        assert!(parse(&["help", "biuld"]).is_err());

        // the program after -- gets its own --help
        assert_eq!(
            parse(&["run", "--", "--help"]),
            Ok(Command::Run {
                script: DEFAULT_SCRIPT.to_string(),
                module: None,
                args: vec!["--help".to_string()]
            })
        );
    }

    #[test]
    fn the_test_summary_lists_every_module() {
        let failed = TestOutcome::Failed {
            counts: Some(TestCounts {
                tests: 10,
                failed: 2,
                skipped: 0,
            }),
            error: "tests failed".to_string(),
        };

        let not_compiled = TestOutcome::Failed {
            counts: None,
            error: "kotlinc failed (exit status: 1)".to_string(),
        };

        let passed = TestOutcome::Passed(Some(TestCounts {
            tests: 12,
            failed: 0,
            skipped: 1,
        }));

        let outcomes = [
            ("core", Some(passed)),
            ("app", Some(failed)),
            ("app (androidTest)", Some(not_compiled)),
            ("ui", Some(TestOutcome::UpToDate)),
            ("tools", Some(TestOutcome::NoTests)),
            ("other", None),
        ];

        let summary = test_summary(&outcomes.map(|(label, outcome)| (label.to_string(), outcome)));
        assert_eq!(
            summary,
            "> Tests
  core               passed, 12 tests, 1 skipped
  app                FAILED, 2 of 10 tests
  app (androidTest)  FAILED: kotlinc failed (exit status: 1)
  ui                 up to date
  tools              no tests
  other              not supported: tests of this kind of module do not run
2 failed, 1 passed, 1 up to date, 1 not supported, 1 without tests"
        );
        let one = [(
            "core".to_string(),
            Some(TestOutcome::Passed(Some(TestCounts {
                tests: 1,
                failed: 0,
                skipped: 0,
            }))),
        )];

        assert_eq!(
            test_summary(&one),
            "> Tests\n  core  passed, 1 test\n1 passed"
        );
        assert_eq!(suite_label("app", "androidTest"), "app (androidTest)");
    }

    #[test]
    fn build_options_are_taken_before_the_arguments_of_the_program() {
        let args = |args: &[&str]| args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();

        assert_eq!(
            take_build_options(&args(&["build", "--jobs", "3", "--refresh", "--release"])),
            Ok((
                BuildOptions {
                    jobs: Some(3),
                    refresh: true,
                    release: true
                },
                args(&["build"])
            ))
        );
        assert_eq!(
            take_build_options(&args(&["run", "--", "--jobs", "1", "--refresh"])),
            Ok((
                BuildOptions::default(),
                args(&["run", "--", "--jobs", "1", "--refresh"])
            ))
        );
        assert!(take_build_options(&args(&["test", "--jobs", "0"])).is_err());
    }

    #[test]
    fn errors_point_at_line_and_column() {
        let source = "use kotlin\nx = list(\n    y)";
        assert_eq!(line_column(source, 0), (1, 1));
        assert_eq!(line_column(source, 11), (2, 1));
        assert_eq!(line_column(source, source.len() - 2), (3, 5));
    }
}
