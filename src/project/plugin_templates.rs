// Templates of plugins for `build new` and `build add`: templates/<name>/ next to their plugin.build, and the
// extensions of a language by a plugin in extends/<language>/<variant>/, like cpp:vulkan-glfw. module.build holds
// the build call, before it what the script uses besides the language and the plugin and the modules the call takes,
// like its shaders. The other files with {{build.name}}, {{build.identifier}}, {{build.domain}}, {{build.package}}
// and {{build.package_dir}} replaced.
use std::path::{Path, PathBuf};

use crate::common::system;
use crate::jvm::templates;
use crate::language::parser::{Expression, ExpressionType, Parser, StatementType};
use crate::plugins::{self, MODULE_BUILD, PluginSource};

use super::{ModuleTemplate, TemplateCall};

/// the template of a kind without :<template>
pub const DEFAULT_TEMPLATE: &str = "app";

/// What templates write where build-script fills in: {{build.name}}. Unusual in any language, and without a
/// colon, which Windows does not take in file names.
const PLACEHOLDER_START: &str = "{{build.";

const PLACEHOLDERS: &[&str] = &["name", "identifier", "domain", "package", "package_dir"];

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

    /// The directory of the plugin, at the release the lock keeps
    fn plugin_dir(&self, cache: &Path) -> Result<PathBuf, String> {
        let plugin_file = plugins::locate(&self.source, cache)?;
        return Ok(plugin_file.parent().unwrap_or(Path::new(".")).to_path_buf());
    }

    /// The name and directory of the template, of the plugin at the release the lock keeps: the one after :,
    /// otherwise app or the only one
    pub fn template_dir(&self, cache: &Path) -> Result<(String, PathBuf), String> {
        let plugin_dir = self.plugin_dir(cache)?;
        let plugin_dir = plugin_dir.as_path();
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

/// A template `new` and `add` found: of a plugin, or an extension of a language by one
#[derive(Debug)]
pub struct FoundTemplate {
    /// what the script uses for it: the language, when it is a plugin or needs a use, then the plugin
    pub uses: Vec<String>,
    /// of a language of build-script, like Maven Central for Kotlin
    pub repositories: &'static [&'static str],
    pub name: String,
    pub directory: PathBuf,
    /// for the log: the template cli of go, the extension glfw of vulkan for cpp
    pub description: String,
}

/// What a language of build-script needs in a script: its use and the repositories of its modules
pub struct BuiltinLanguage {
    pub use_argument: String,
    pub repositories: &'static [&'static str],
}

/// The template `kind` names that is no kind of build-script: a plugin with the template app or its only one (go),
/// a template of a plugin (go:cli), or a plugin extending a language with a variant of its own: cpp:vulkan for the
/// variant app, cpp:vulkan-glfw or cpp:vulkan:glfw for others, go:vulkan for a language of a plugin. A plugin of a
/// repository like for use: cpp:someone/vulkan-glfw. `builtin` knows the languages of build-script.
pub fn find_template(
    kind: &str,
    plugins: &PluginLookup,
    builtin: impl Fn(&str) -> Option<BuiltinLanguage>,
) -> Result<FoundTemplate, String> {
    let Some((language, rest)) = kind.split_once(':') else {
        let plugin = PluginKind::parse(kind, &plugins.official)?;
        let (name, directory) = plugin.template_dir(&plugins.cache)?;

        return Ok(FoundTemplate {
            description: format!("the template {name} of {}", plugin.source.name),
            uses: vec![plugin.use_argument],
            repositories: &[],
            name,
            directory,
        });
    };

    if let Some(builtin) = builtin(language) {
        return extension(language, vec![builtin.use_argument], builtin.repositories, rest, plugins);
    }

    // a language of a plugin: a template of its own first, like go:cli
    let plugin = PluginKind::parse(language, &plugins.official)?;
    let plugin_dir = plugin.plugin_dir(&plugins.cache)
        .map_err(|error| format!("{language} is no language of build-script and no plugin: {error}"))?;

    let templates = plugins::template_names(&plugin_dir);

    if templates.iter().any(|name| name == rest) {
        return Ok(FoundTemplate {
            description: format!("the template {rest} of {}", plugin.source.name),
            uses: vec![plugin.use_argument],
            repositories: &[],
            name: rest.to_string(),
            directory: plugin_dir.join(plugins::TEMPLATES).join(rest),
        });
    }

    return extension(language, vec![plugin.use_argument.clone()], &[], rest, plugins).map_err(|error| match templates.is_empty() {
        true => error,
        false => format!("{} has no template {rest} (templates: {}) and {error}", plugin.source.name, templates.join(", ")),
    });
}

/// The variant of a plugin extending the language: rest is plugin, plugin-variant or plugin:variant
fn extension(
    language: &str,
    uses: Vec<String>,
    repositories: &'static [&'static str],
    rest: &str,
    plugins: &PluginLookup,
) -> Result<FoundTemplate, String> {
    // vulkan-glfw is the variant glfw of vulkan, unless a plugin is named so
    let candidates: Vec<(&str, &str)> = match rest.split_once(':') {
        Some((plugin, variant)) => vec![(plugin, variant)],
        None => rest.rsplit_once('-')
            .into_iter()
            .chain([(rest, DEFAULT_TEMPLATE)])
            .collect(),
    };

    let mut problem = None;

    for (name, variant) in candidates {
        let plugin = PluginKind::parse(name, &plugins.official)?;

        // a candidate that is no plugin, like vulkan-glfw
        let Ok(plugin_dir) = plugin.plugin_dir(&plugins.cache) else { continue };
        let variants = plugins::extension_names(&plugin_dir, language);

        if variants.iter().any(|known| known == variant) {
            return Ok(FoundTemplate {
                description: format!("the extension {variant} of {} for {language}", plugin.source.name),
                uses: [uses, vec![plugin.use_argument]].concat(),
                repositories,
                name: variant.to_string(),
                directory: plugin_dir.join(plugins::EXTENDS).join(language).join(variant),
            });
        }

        problem.get_or_insert_with(|| match variants.is_empty() {
            true => format!("{name} extends no {language}"),
            false => {
                let known: Vec<String> = variants.iter().map(|known| extension_kind(language, name, known)).collect();
                format!("{name} has no {variant} for {language} ({})", known.join(", "))
            }
        });
    }

    return Err(problem.unwrap_or_else(|| format!("no plugin {rest} extends {language}")));
}

/// How new and add name the variant of a plugin for a language: cpp:vulkan for app, cpp:vulkan-glfw for others
pub fn extension_kind(language: &str, plugin: &str, variant: &str) -> String {
    return match variant == DEFAULT_TEMPLATE {
        true => format!("{language}:{plugin}"),
        false => format!("{language}:{plugin}-{variant}"),
    };
}

/// What the template in `directory` makes of a module: its files with the placeholders replaced and the build
/// call of module.build. The domain is asked for only when a file needs {{build.domain}} or {{build.package}}.
/// A {{build.…}} that is none of the placeholders is an error, most likely a typo.
pub fn module_template(
    found: &FoundTemplate,
    name: &str,
    package: Option<&str>,
    domain: impl FnOnce() -> Result<String, String>,
) -> Result<ModuleTemplate, String> {
    return template_in(&found.uses, found.repositories, &found.name, &found.directory, name, package, domain);
}

fn template_in(
    uses: &[String],
    repositories: &'static [&'static str],
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

    let package_dir = package.replace('.', "/");
    let values = [name, identifier.as_str(), domain.as_str(), package.as_str(), package_dir.as_str()];
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
    let parsed = parse_module_build(&call).map_err(|error| format!("{MODULE_BUILD} of {template_name}: {error}"))?;

    let mut more_uses: Vec<String> = uses.iter().skip(1).cloned().collect();
    for used in parsed.uses {
        if !uses.contains(&used) && !more_uses.contains(&used) {
            more_uses.push(used);
        }
    }

    let (function, positional, named) = parsed.call;

    return Ok(ModuleTemplate {
        use_argument: uses.first().cloned().unwrap_or_default(),
        more_uses,
        repositories,
        // a library says so in its name, every other template makes a program
        is_app: !["library", "lib"].iter().any(|library| template_name == *library || template_name.ends_with(&format!("-{library}"))),
        files,
        helpers: parsed.helpers,
        function,
        positional,
        named,
    });
}

/// What module.build holds: the uses before the language and the plugin, the modules the build call takes and the
/// call
struct ModuleBuild {
    uses: Vec<String>,
    helpers: Vec<TemplateCall>,
    call: (String, Vec<String>, Vec<(String, String)>),
}

/// The statements of module.build: uses, then assignments of calls to variables, then the build call
fn parse_module_build(text: &str) -> Result<ModuleBuild, String> {
    let script = Parser::new(text)
        .parse_script()
        .map_err(|error| error.variant.to_string())?;

    let mut uses = vec![];
    let mut helpers = vec![];
    let mut call = None;
    let one_call = "it has to hold one call, like build_go(\"{{build.name}}\"), after the modules it takes, like shaders = build_shaders(...)";

    for statement in &script.statements {
        if call.is_some() {
            return Err(one_call.to_string());
        }

        match &statement.variant {
            StatementType::Use(platform) => uses.push(platform.name.clone()),
            StatementType::UsePlugin(plugin) => uses.push(format!("\"{plugin}\"")),
            StatementType::Expression(Expression { variant: ExpressionType::Assignment { target, value }, .. }) => {
                let (function, positional, named) = call_parts(text, value).ok_or_else(|| one_call.to_string())?;
                helpers.push(TemplateCall { variable: target.name.clone(), function, positional, named });
            }
            StatementType::Expression(expression) => call = Some(call_parts(text, expression).ok_or_else(|| one_call.to_string())?),
        }
    }

    return Ok(ModuleBuild {
        uses,
        helpers,
        call: call.ok_or_else(|| one_call.to_string())?,
    });
}

/// The function of a call and its arguments as they are written: the positional ones and the named ones. None for
/// what is no call of a function by its name.
fn call_parts(text: &str, expression: &Expression) -> Option<(String, Vec<String>, Vec<(String, String)>)> {
    let ExpressionType::FunctionCall { target, args } = &expression.variant else {
        return None;
    };

    let ExpressionType::Ident(function) = &target.variant else {
        return None;
    };

    let written = |expression: &Expression| text[expression.span.start..expression.span.end].to_string();
    let mut positional = vec![];
    let mut named = vec![];

    for argument in args {
        match &argument.variant {
            ExpressionType::Assignment { target, value } => named.push((target.name.clone(), written(value))),
            _ => positional.push(written(argument)),
        }
    }

    return Some((function.clone(), positional, named));
}

/// A {{build.…}} left after the placeholders are replaced, like {{build.nmae}}
fn unknown_placeholder(text: &str) -> Option<String> {
    let start = text.find(PLACEHOLDER_START)?;
    let end = text[start..].find("}}").map_or(text.len(), |end| start + end + 2);

    return Some(text[start..end].to_string());
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

    fn go() -> Vec<String> {
        return vec!["go".to_string()];
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
        let template = template_in(&go(), &[], "app", &directory, "my-tool", None, || {
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
        let template = template_in(&go(), &[], "library", &directory, "x", None, || Err("asked".to_string())).unwrap();
        assert!(!template.is_app);

        // a typo is an error, not a placeholder left in the file
        write("go.mod", "module {{build.nmae}}\n");
        let error = template_in(&go(), &[], "app", &directory, "x", None, || Ok(String::new())).err().unwrap();
        assert!(error.starts_with("{{build.nmae}} in go.mod of the template app is no placeholder ({{build.name}}, "), "{error}");

        write("go.mod", "module {{build.name}}\n");
        write("module.build", "x = 1\n");
        let error = template_in(&go(), &[], "app", &directory, "x", None, || Ok(String::new())).err().unwrap();
        assert!(error.contains("it has to hold one call"), "{error}");
    }
}
