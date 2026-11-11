// What every script has without `use`: print, type, env, git, operating_system, the time, list and
// build_command
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
    return functions;
}
