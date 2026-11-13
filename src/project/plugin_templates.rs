// Templates of plugins for `build new` and `build add`: templates/<name>/ next to their plugin.build, the
// build call in module.build, the other files with {{build.name}}, {{build.identifier}}, {{build.domain}} and
// {{build.package}} replaced
use std::path::{Path, PathBuf};

use crate::common::system;
use crate::jvm::templates;
use crate::language::parser::{Expression, ExpressionType, Parser, StatementType};
use crate::plugins::{self, MODULE_BUILD, PluginSource};

use super::ModuleTemplate;

/// the template of a kind without :<template>
pub const DEFAULT_TEMPLATE: &str = "app";

/// What templates write where build-script fills in: {{build.name}}. Unusual in any language, and without a
/// colon, which Windows does not take in file names.
const PLACEHOLDER_START: &str = "{{build.";

const PLACEHOLDERS: &[&str] = &["name", "identifier", "domain", "package"];

/// Where plugins come from: the official list and the cache of their checkouts
pub struct PluginLookup {
    pub official: String,
    pub cache: PathBuf,
}

impl PluginLookup {
    pub fn of_this_machine() -> Self {
        return PluginLookup {
            official: plugins::official_repository(),
            cache: system::cache_dir("git"),
        };
    }
}

/// A kind of `new` and `add` that is no kind of build-script: go, go:cli, go@1, someone/zig:lib
pub struct PluginKind {
    /// how `use` names the plugin: go, go@1, "someone/zig"
    pub use_argument: String,
    source: PluginSource,
    template: Option<String>,
}

impl PluginKind {
    pub fn parse(kind: &str, official: &str) -> Result<Self, String> {
        let (plugin, template) = match kind.split_once(':') {
            Some((plugin, template)) => (plugin, Some(template.to_string())),
            None => (kind, None),
        };

        // a repository has a slash, an official plugin is a name
        return match plugin.contains('/') {
            true => Ok(PluginKind {
                use_argument: format!("\"{plugin}\""),
                source: PluginSource::of_repository(plugin)?,
                template,
            }),
            false => Ok(PluginKind {
                use_argument: plugin.to_string(),
                source: PluginSource::official(plugin, official),
                template,
            }),
        };
    }

    /// The name and directory of the template, of the plugin at the release the lock keeps: the one after :,
    /// otherwise app or the only one
    pub fn template_dir(&self, cache: &Path) -> Result<(String, PathBuf), String> {
        let plugin_file = plugins::locate(&self.source, cache)?;
        let plugin_dir = plugin_file.parent().unwrap_or(Path::new("."));
        let names = plugins::template_names(plugin_dir);
        let plugin = &self.source.name;

        if names.is_empty() {
            return Err(format!("{plugin} has no templates for new and add"));
        }

        let name = match &self.template {
            Some(name) if names.contains(name) => name.clone(),
            Some(name) => return Err(format!("{plugin} has no template {name} (templates: {})", names.join(", "))),
            None if names.iter().any(|name| name == DEFAULT_TEMPLATE) => DEFAULT_TEMPLATE.to_string(),
            None if names.len() == 1 => names[0].clone(),
            None => {
                return Err(format!(
                    "{plugin} has several templates, name one like {plugin}:{} (templates: {})",
                    names[0],
                    names.join(", ")
                ));
            }
        };

        return Ok((name.clone(), plugin_dir.join(plugins::TEMPLATES).join(name)));
    }
}

/// What the template in `directory` makes of a module: its files with the placeholders replaced and the build
/// call of module.build. The domain is asked for only when a file needs {{build.domain}} or {{build.package}}.
/// A {{build.…}} that is none of the placeholders is an error, most likely a typo.
pub fn module_template(
    use_argument: &str,
    template_name: &str,
    directory: &Path,
    name: &str,
    package: Option<&str>,
    domain: impl FnOnce() -> Result<String, String>,
) -> Result<ModuleTemplate, String> {
    let mut contents = vec![];

    for file in system::files_below(&[directory.to_path_buf()]) {
        contents.push((system::relative_with_slashes(directory, &file), system::read_file(&file)?));
    }

    let mentions = |placeholder: &str| {
        let written = format!("{PLACEHOLDER_START}{placeholder}}}}}");
        return contents.iter().any(|(path, content): &(String, String)| path.contains(&written) || content.contains(&written));
    };
    let needs_package = package.is_none() && mentions("package");

    let domain = match mentions("domain") || needs_package {
        true => domain()?,
        false => String::new(),
    };

    let package = match package {
        Some(package) => package.to_string(),
        None if needs_package => format!("{}.{}", templates::package_of_domain(&domain), templates::package_segment(name)),
        None => String::new(),
    };

    // my-tool becomes my_tool, for identifiers of the language
    let identifier: String = name.chars()
        .map(|character| if character.is_ascii_alphanumeric() { character } else { '_' })
        .collect();

    let values = [name, identifier.as_str(), domain.as_str(), package.as_str()];
    let replaced = |text: &str| {
        return PLACEHOLDERS.iter()
            .zip(values)
            .fold(text.to_string(), |text, (placeholder, value)| text.replace(&format!("{PLACEHOLDER_START}{placeholder}}}}}"), value));
    };

    let mut files = vec![];
    let mut call = None;

    for (path, content) in &contents {
        let (path_replaced, content_replaced) = (replaced(path), replaced(content));

        if let Some(unknown) = [&path_replaced, &content_replaced].iter().find_map(|text| unknown_placeholder(text)) {
            let known: Vec<String> = PLACEHOLDERS.iter()
                .map(|placeholder| format!("{PLACEHOLDER_START}{placeholder}}}}}"))
                .collect();
            return Err(format!("{unknown} in {path} of the template {template_name} is no placeholder ({})", known.join(", ")));
        }

        match path == MODULE_BUILD {
            true => call = Some(content_replaced),
            false => files.push((path_replaced, content_replaced)),
        }
    }

    let call = call.ok_or_else(|| format!("{} has no {MODULE_BUILD}", directory.display()))?;
    let (function, positional, named) = parse_call(&call).map_err(|error| format!("{MODULE_BUILD} of {template_name}: {error}"))?;

    return Ok(ModuleTemplate {
        use_argument: use_argument.to_string(),
        repositories: &[],
        is_app: template_name == DEFAULT_TEMPLATE || template_name.ends_with("-app"),
        files,
        function,
        positional,
        named,
    });
}

/// A {{build.…}} left after the placeholders are replaced, like {{build.nmae}}
fn unknown_placeholder(text: &str) -> Option<String> {
    let start = text.find(PLACEHOLDER_START)?;
    let end = text[start..].find("}}").map_or(text.len(), |end| start + end + 2);

    return Some(text[start..end].to_string());
}

/// The function of the one call in module.build and its arguments as they are written: the positional ones
/// and the named ones
fn parse_call(call: &str) -> Result<(String, Vec<String>, Vec<(String, String)>), String> {
    let script = Parser::new(call)
        .parse_script()
        .map_err(|error| error.variant.to_string())?;

    let [statement] = script.statements.as_slice() else {
        return Err("it has to hold one call, like build_go(\"{{build.name}}\")".to_string());
    };

    let StatementType::Expression(Expression { variant: ExpressionType::FunctionCall { target, args }, .. }) = &statement.variant else {
        return Err("it has to hold one call, like build_go(\"{{build.name}}\")".to_string());
    };

    let ExpressionType::Ident(function) = &target.variant else {
        return Err("the call has to name its function".to_string());
    };

    let text = |expression: &Expression| call[expression.span.start..expression.span.end].to_string();
    let mut positional = vec![];
    let mut named = vec![];

    for argument in args {
        match &argument.variant {
            ExpressionType::Assignment { target, value } => named.push((target.name.clone(), text(value))),
            _ => positional.push(text(argument)),
        }
    }

    return Ok((function.clone(), positional, named));
}

#[cfg(test)]
mod tests {
    use crate::common::test_support::ScratchDir;

    use super::*;

    #[test]
    fn kinds_name_plugins_and_their_templates() {
        let official = "https://github.com/build-script/plugins";

        let go = PluginKind::parse("go", official).unwrap();
        assert_eq!((go.use_argument.as_str(), go.template.as_deref()), ("go", None));

        let release = PluginKind::parse("go@1:cli", official).unwrap();
        assert_eq!((release.use_argument.as_str(), release.template.as_deref()), ("go@1", Some("cli")));
        assert_eq!(release.source.selector.as_deref(), Some("1"));

        let repository = PluginKind::parse("someone/zig:lib", official).unwrap();
        assert_eq!((repository.use_argument.as_str(), repository.template.as_deref()), ("\"someone/zig\"", Some("lib")));
    }

    #[test]
    fn templates_replace_placeholders_and_read_their_call() {
        let directory = ScratchDir::new("plugin-template");
        let write = |name: &str, content: &str| system::write_file(&directory.join(name), content).unwrap();
        write("module.build", "// a Go program\nbuild_go(\"{{build.name}}\", flags = list(\"-race\"), version = \"1.27.4\")\n");
        write("go.mod", "module {{build.domain}}/{{build.name}}\n\ngo 1.27\n");
        // braces of the language stay as they are
        write("cmd/{{build.identifier}}.go", "package main // {{build.name}}\nfunc main() { fmt.Printf(\"{name}\") }\n");

        let mut asked = false;
        let template = module_template("go", "app", &directory, "my-tool", None, || {
            asked = true;
            return Ok("example.com".to_string());
        }).unwrap();

        assert!(asked);
        assert!(template.is_app);
        assert_eq!(template.function, "build_go");
        assert_eq!(template.positional, ["\"my-tool\""]);
        assert_eq!(template.named, [("flags".to_string(), "list(\"-race\")".to_string()), ("version".to_string(), "\"1.27.4\"".to_string())]);

        let mut files = template.files.clone();
        files.sort();
        assert_eq!(files, [
            ("cmd/my_tool.go".to_string(), "package main // my-tool\nfunc main() { fmt.Printf(\"{name}\") }\n".to_string()),
            ("go.mod".to_string(), "module example.com/my-tool\n\ngo 1.27\n".to_string()),
        ]);

        // without {{build.domain}} and {{build.package}} nobody is asked
        write("go.mod", "module {{build.name}}\n");
        let template = module_template("go", "library", &directory, "x", None, || Err("asked".to_string())).unwrap();
        assert!(!template.is_app);

        // a typo is an error, not a placeholder left in the file
        write("go.mod", "module {{build.nmae}}\n");
        let error = module_template("go", "app", &directory, "x", None, || Ok(String::new())).err().unwrap();
        assert!(error.starts_with("{{build.nmae}} in go.mod of the template app is no placeholder ({{build.name}}, "), "{error}");

        write("go.mod", "module {{build.name}}\n");
        write("module.build", "x = 1\n");
        let error = module_template("go", "app", &directory, "x", None, || Ok(String::new())).err().unwrap();
        assert!(error.contains("it has to hold one call"), "{error}");
    }
}
