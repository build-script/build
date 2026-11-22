// What every script has without `use`: print, type, env, git, the target and the machine, files, patterns,
// joining and replacing text, the C libraries among modules, the time, list, build_command and tool
use std::path::Path;

use super::*;

pub fn core_functions() -> Vec<(&'static str, Value)> {
    let mut functions = vec![
        builtin(
            "print",
            FunctionParameters::new()
                .variadic("values", Type::Any)
                .returns(Type::Unit),
            |context, args| {
                let parts: Vec<String> = args
                    .rest::<Value>()?
                    .iter()
                    .map(|value| value.to_string())
                    .collect();

                // the script runs once per target, a line says which one besides the machine
                match context.target {
                    Some(target) => println!("[{}] {}", target.name(), parts.join(" ")),
                    None => println!("{}", parts.join(" ")),
                }
                Ok(Value::Unit)
            },
        ),
        builtin(
            "type",
            FunctionParameters::new()
                .required("value", Type::Any)
                .returns(Type::String),
            |_, args| Ok(args.get::<Value>("value")?.type_of().to_string().into()),
        ),
        builtin(
            "env",
            FunctionParameters::new()
                .required("name", Type::String)
                .optional("default", Type::String)
                .returns(Type::String),
            |_, args| {
                let name: String = args.get("name")?;

                if let Ok(value) = std::env::var(&name) {
                    return Ok(value.into());
                }

                let default: Option<String> = args.get("default")?;

                return default.map(Value::from).ok_or_else(|| {
                    format!(
                        "the environment variable {name} is not set, a default = ... would be taken then"
                    )
                });
            },
        ),
        builtin(
            "has_env",
            FunctionParameters::new()
                .required("name", Type::String)
                .returns(Type::Bool),
            |_, args| {
                let name: String = args.get("name")?;
                Ok(Value::Bool(std::env::var_os(name).is_some()))
            },
        ),
        builtin(
            "git",
            FunctionParameters::new()
                .required("url", Type::String)
                // a commit, better its whole hash, or a tag
                .required("commit", Type::String)
                .returns(Type::String),
            |context, args| {
                let url: String = args.get("url")?;
                let commit: String = args.get("commit")?;
                let checkout = crate::common::git::checkout(&url, &commit, &context.project_dir)?;
                return Ok(checkout.display().to_string().into());
            },
        ),
        // of the target the script is built for: the machine, or another system
        builtin(
            "target",
            FunctionParameters::new().returns(Type::String),
            // like "linux-aarch64", "macos-aarch64" or "windows-x86_64"
            |context, _| Ok(context.target().name().into()),
        ),
        builtin(
            "operating_system",
            FunctionParameters::new().returns(Type::String),
            // "macos", "linux", "windows" or "android"
            |context, _| match context.target {
                Some(target) => Ok(target.os.name().into()),
                None => Ok(host_operating_system()),
            },
        ),
        builtin(
            "architecture",
            FunctionParameters::new().returns(Type::String),
            // "aarch64", "x86_64", "armv7" or "x86"
            |context, _| match context.target {
                Some(target) => Ok(target.arch.name().into()),
                None => host_architecture(),
            },
        ),
        builtin(
            "executable",
            FunctionParameters::new()
                .required("name", Type::String)
                .returns(Type::String),
            // with .exe for Windows
            |context, args| match context.target {
                Some(target) => Ok(target.os.executable(&args.get::<String>("name")?).into()),
                None => host_executable(args),
            },
        ),
        // of the machine build-script runs on, for the tools a build downloads and runs
        builtin(
            "host_target",
            FunctionParameters::new().returns(Type::String),
            |_, _| Ok(Target::host().name().into()),
        ),
        builtin(
            "host_operating_system",
            FunctionParameters::new().returns(Type::String),
            |_, _| Ok(host_operating_system()),
        ),
        builtin(
            "host_architecture",
            FunctionParameters::new().returns(Type::String),
            |_, _| host_architecture(),
        ),
        builtin(
            "host_executable",
            FunctionParameters::new()
                .required("name", Type::String)
                .returns(Type::String),
            |_, args| host_executable(args),
        ),
        builtin(
            "cache_directory",
            FunctionParameters::new()
                .required("name", Type::String)
                .returns(Type::String),
            // for the caches of tools, like the modules of Go, apart from those of build-script
            |_, args| {
                let name: String = args.get("name")?;

                if name.is_empty() || name.contains("..") || Path::new(&name).is_absolute() {
                    return Err(format!("\"{name}\" cannot name a directory of the cache"));
                }

                let directory = system::cache_dir("plugins").join(name);
                system::create_dir(&directory)?;
                return Ok(directory.display().to_string().into());
            },
        ),
        builtin(
            "fail",
            FunctionParameters::new()
                .required("message", Type::String)
                .returns(Type::Unit),
            |_, args| Err(args.get::<String>("message")?),
        ),
        builtin(
            "read_file",
            FunctionParameters::new()
                .required("path", Type::String)
                .with_default("default", Type::String, "")
                .returns(Type::String),
            // relative to the script, `default` when there is no such file
            |context, args| {
                let file = context.project_dir.join(args.get::<String>("path")?);

                return match file.is_file() {
                    true => Ok(system::read_file(&file)?.into()),
                    false => Ok(args.get::<String>("default")?.into()),
                };
            },
        ),
        builtin(
            "match",
            FunctionParameters::new()
                .required("text", Type::String)
                .required("pattern", Type::String)
                .returns(Type::String),
            // the first group of the regular expression, the whole match without groups, "" without a match
            |_, args| {
                let pattern: String = args.get("pattern")?;
                let expression = regex_lite::Regex::new(&pattern).map_err(|error| format!("{pattern} is no regular expression: {error}"))?;
                let text: String = args.get("text")?;

                let found = expression.captures(&text)
                    .and_then(|captures| captures.get(1).or_else(|| captures.get(0)))
                    .map_or("", |found| found.as_str());

                return Ok(found.into());
            },
        ),
        builtin(
            "join",
            FunctionParameters::new()
                .required("items", Type::list(Type::String))
                .required("separator", Type::String)
                .returns(Type::String),
            // the items in one string, like the lines of a file a tool reads: no loop needed for a list
            |_, args| {
                let items: Vec<String> = args.get("items")?;
                let separator: String = args.get("separator")?;
                return Ok(items.join(&separator).into());
            },
        ),
        builtin(
            "replace",
            FunctionParameters::new()
                .required("text", Type::String)
                .required("old", Type::String)
                .required("new", Type::String)
                .returns(Type::String),
            // every occurrence, like the @ of name@version for the space of a go.mod
            |_, args| {
                let text: String = args.get("text")?;
                let old: String = args.get("old")?;
                let new: String = args.get("new")?;

                if old.is_empty() {
                    return Err("replace needs a text to replace, old is empty".to_string());
                }

                return Ok(text.replace(&old, &new).into());
            },
        ),
        builtin(
            "file",
            FunctionParameters::new()
                .required("path", Type::String)
                .required("content", Type::String)
                .returns(Type::list(Type::String)),
            // for the files of build_command, which writes them before its command runs
            |_, args| {
                let path: String = args.get("path")?;
                let content: String = args.get("content")?;
                return Ok(Value::List(vec![path.into(), content.into()]));
            },
        ),
        builtin(
            "c_libraries",
            FunctionParameters::new()
                .required("modules", Type::list(Type::Module))
                .returns(Type::list(Type::Module)),
            // those of build_c, build_cpp, build_cmake and system_library: a function of a plugin builds with them
            // the way their language does, like cgo for Go
            |_, args| {
                let modules: Vec<Value> = args.get("modules")?;
                let libraries = modules.into_iter()
                    .filter(|value| matches!(value, Value::Module(module) if crate::native::LIBRARY_FUNCTIONS.contains(&module.function)))
                    .collect();
                return Ok(Value::List(libraries));
            },
        ),
        builtin(
            "current_time_millis",
            FunctionParameters::new().returns(Type::Int),
            |_, _| {
                let millis = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|error| error.to_string())?
                    .as_millis();
                Ok(Value::Int(millis as i64))
            },
        ),
        builtin(
            "list",
            FunctionParameters::new()
                .variadic("items", Type::Any)
                .returns(Type::list(Type::Any)),
            |_, args| Ok(Value::List(args.rest()?)),
        ),
    ];

    functions.extend(crate::command::builtins());
    functions.extend(crate::tool::builtins());
    return functions;
}

/// "macos", "linux" or "windows", as Rust names the system build-script was built for
fn host_operating_system() -> Value {
    return std::env::consts::OS.into();
}

/// "aarch64" or "x86_64", an error on other processors
fn host_architecture() -> Result<Value, String> {
    match system::host_architecture()? {
        system::HostArchitecture::Aarch64 => return Ok("aarch64".into()),
        system::HostArchitecture::X86_64 => return Ok("x86_64".into()),
    }
}

/// With .exe on Windows
fn host_executable(args: &Arguments) -> Result<Value, String> {
    return Ok(format!("{}{}", args.get::<String>("name")?, std::env::consts::EXE_SUFFIX).into());
}
