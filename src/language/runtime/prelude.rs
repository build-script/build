// What every script has without `use`: print, type, env, git, the system, files, the time, list,
// build_command and tool
use std::path::Path;

use super::*;

pub fn core_functions() -> Vec<(&'static str, Value)> {
    let mut functions = vec![
        builtin(
            "print",
            FunctionParameters::new()
                .variadic("values", Type::Any)
                .returns(Type::Unit),
            |_, args| {
                let parts: Vec<String> = args
                    .rest::<Value>()?
                    .iter()
                    .map(|value| value.to_string())
                    .collect();
                println!("{}", parts.join(" "));
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
        builtin(
            "operating_system",
            FunctionParameters::new().returns(Type::String),
            // "macos", "linux" or "windows"
            |_, _| Ok(std::env::consts::OS.into()),
        ),
        builtin(
            "architecture",
            FunctionParameters::new().returns(Type::String),
            // "aarch64" or "x86_64"
            |_, _| match system::host_architecture()? {
                system::HostArchitecture::Aarch64 => Ok("aarch64".into()),
                system::HostArchitecture::X86_64 => Ok("x86_64".into()),
            },
        ),
        builtin(
            "executable",
            FunctionParameters::new()
                .required("name", Type::String)
                .returns(Type::String),
            // with .exe on Windows
            |_, args| Ok(format!("{}{}", args.get::<String>("name")?, std::env::consts::EXE_SUFFIX).into()),
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
