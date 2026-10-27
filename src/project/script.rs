// Adding to a build script: a module with its `use`, its repositories and its build call, placed so apps
// come last, or a library to a module's dependencies. The script is parsed to find the places, everything
// else stays as it is written.
use std::sync::Arc;

use crate::language::parser::{Expression, ExpressionType, Parser, Script, StatementType, UseTarget};
use crate::language::runtime::{self, FunctionSignature, Type, Value};

use super::{ModuleTemplate, NewModule};

/// The script with the module added as `variable`. Libraries go before the
/// first app, so apps can depend on them, apps go to the end.
pub fn with_module(
    source: &str,
    variable: &str,
    module: &NewModule,
    template: &ModuleTemplate,
) -> Result<String, String> {
    let script = Parser::new(source).parse_script().map_err(|error| {
        let line = source[..error.span.start.min(source.len())].matches('\n').count() + 1;
        format!("line {line}: {}", error.variant)
    })?;

    let mut uses: Vec<(&str, usize)> = vec![];
    let mut variables: Vec<&str> = vec![];
    let mut module_names: Vec<&str> = vec![];
    let mut repositories: Option<(&Expression, usize)> = None;
    let mut first_app: Option<usize> = None;

    for statement in &script.statements {
        let expression = match &statement.variant {
            StatementType::Use(UseTarget::Ident(platform)) => {
                uses.push((platform.name.as_str(), statement.span.end));
                continue;
            }
            StatementType::Use(_) => continue,
            StatementType::Expression(expression) => expression,
        };

        let value = match &expression.variant {
            ExpressionType::Assignment { target, value } => {
                let name = identifier(target).unwrap_or_default();
                variables.push(name);
                if name == "repositories" {
                    repositories = Some((value, statement.span.start));
                }
                value.as_ref()
            }
            _ => expression,
        };

        if let ExpressionType::FunctionCall { target, args } = &value.variant {
            if let Some(ExpressionType::String(name)) = args.first().map(|arg| &arg.variant) {
                module_names.push(name);
            }

            let is_app = identifier(target) == Some("build_android")
                || args.iter()
                    .any(|arg| argument_name(arg) == Some("main_class"))
                || args.iter().any(|arg| match &arg.variant {
                    ExpressionType::Assignment { target, value } => {
                        identifier(target) == Some("program") && identifier(value) == Some("true")
                    }
                    _ => false,
                });

            if is_app && first_app.is_none() {
                first_app = Some(statement.span.start);
            }
        }
    }

    if module_names.contains(&module.name.as_str()) {
        return Err(format!(
            "a module named {} is already in the script",
            module.name
        ));
    }

    if variables.contains(&variable) {
        return Err(format!("the script already has a variable {variable}"));
    }

    let module_position = match first_app {
        Some(app) if !template.is_app => Some(start_of_line_with_comments(source, app)),
        _ => None,
    };

    // inserted at their offset, at equal offsets in the order they were added
    let mut edits: Vec<(usize, String)> = vec![];
    // C and C++ modules have none, their ports come from vcpkg
    let mut repositories_argument = (!template.repositories.is_empty()).then(|| "repositories".to_string());
    let mut new_repositories = None;

    match repositories {
        _ if template.repositories.is_empty() => {}
        Some((value, start)) if module_position.is_none_or(|position| start < position) => {
            match list_arguments(source, value) {
                Some((after_paren, arguments)) => {
                    let missing: Vec<&str> = template.repositories.iter()
                        .filter(|repository| {
                            !arguments.iter()
                                .any(|argument| {
                                    calls_function(argument, repository.trim_end_matches("()"))
                                })
                        })
                        .copied()
                        .collect();

                    if !missing.is_empty() {
                        let separator = if arguments.is_empty() { "" } else { ", " };
                        edits.push((
                            after_paren,
                            format!("{}{separator}", missing.join(", "))
                        ));
                    }
                }
                // no list(...) to extend: the module names its repositories itself
                None => {
                    repositories_argument = Some(format!(
                        "list({})",
                        template.repositories.join(", ")
                    ))
                }
            }
        }
        Some(_) => repositories_argument = Some(format!("list({})", template.repositories.join(", "))),
        None => {
            new_repositories = Some(format!(
                "repositories = list({})",
                template.repositories.join(", ")
            ))
        }
    }

    let needs_use = !uses.iter()
        .any(|(platform, _)| *platform == template.platform);

    let has_statements = !script.statements.is_empty();

    match uses.last() {
        // below the last use
        Some((_, end)) => {
            let mut text = String::new();

            if needs_use {
                text.push_str(&format!("\nuse {}", template.platform));
            }

            if let Some(repositories) = &new_repositories {
                text.push_str(&format!("\n\n{repositories}"));
            }

            edits.push((*end, text));
        }
        None if has_statements => {
            let mut text = format!("use {}\n", template.platform);

            if let Some(repositories) = &new_repositories {
                text.push_str(&format!("\n{repositories}\n"));
            }

            text.push('\n');
            edits.push((start_of_line(source, script.statements[0].span.start), text));
        }
        None => {
            let mut text = if source.is_empty() || source.ends_with('\n') {
                String::new()
            } else {
                "\n".to_string()
            };

            text.push_str(&format!("use {}\n", template.platform));

            if let Some(repositories) = &new_repositories {
                text.push_str(&format!("\n{repositories}\n"));
            }

            edits.push((source.len(), text));
        }
    }

    let call = build_call(variable, module, template, repositories_argument.as_deref());
    match module_position {
        Some(position) => edits.push((position, format!("{call}\n\n"))),
        None if !has_statements || source.ends_with('\n') => {
            edits.push((source.len(), format!("\n{call}\n")))
        }
        None => edits.push((source.len(), format!("\n\n{call}\n"))),
    }

    return Ok(apply_edits(source, edits));
}

/// `variable = function(positional..., repositories, named = value..., directory = "...")`
fn build_call(
    variable: &str,
    module: &NewModule,
    template: &ModuleTemplate,
    repositories: Option<&str>,
) -> String {
    let mut lines = vec![format!("{variable} = {}(", template.function)];
    lines.extend(
        template.positional.iter()
            .map(|argument| format!("    {argument},")),
    );
    lines.extend(repositories.map(|repositories| format!("    {repositories},")));
    lines.extend(
        template.named.iter()
            .map(|(name, value)| format!("    {name} = {value},")),
    );
    if let Some(directory) = &module.directory {
        lines.push(format!("    directory = \"{directory}\","));
    }
    lines.push(")".to_string());
    return lines.join("\n");
}

/// A build call of a module at the top of a script: `app = build_kotlin("app", ...)`
pub struct ModuleCall<'a> {
    /// what its name argument says, else the variable it is assigned to
    pub name: String,
    pub function: &'a str,
    /// whose `use` brought the function
    pub platform: &'static str,
    pub signature: Arc<FunctionSignature>,
    /// where its statement starts: the variables it uses are assigned before
    pub statement_start: usize,
    call: &'a Expression,
    /// by parameter name, positional ones too
    arguments: Vec<(String, &'a Expression)>,
}

/// How a library got into the dependencies of a module
#[derive(Debug, PartialEq)]
pub enum DependencyChange {
    Added,
    /// another version was there
    Replaced(String),
    /// with this very version
    Present,
}

#[derive(Debug)]
pub struct DependencyEdit {
    pub script: String,
    pub change: DependencyChange,
    /// whose list(...) changed when the module names a variable: other modules may use it as well
    pub variable: Option<String>,
}

/// The build calls of modules at the top level of the script: calls of the
/// functions of its platforms that build a module
pub fn module_calls(script: &Script) -> Vec<ModuleCall<'_>> {
    let mut functions: Vec<(&'static str, &'static str, Arc<FunctionSignature>)> = vec![];
    let mut calls = vec![];

    for statement in &script.statements {
        let expression = match &statement.variant {
            StatementType::Use(UseTarget::Ident(name)) => {
                let platform = runtime::platforms().find(|platform| platform.name == name.name);

                if let Some(platform) = platform {
                    for (function, value) in (platform.functions)() {
                        if let Value::Builtin { signature, .. } = value
                            && matches!(signature.returns, Type::Module)
                        {
                            functions.push((platform.name, function, signature));
                        }
                    }
                }
                continue;
            }
            StatementType::Use(_) => continue,
            StatementType::Expression(expression) => expression,
        };

        let (variable, value) = match &expression.variant {
            ExpressionType::Assignment { target, value } => (identifier(target), value.as_ref()),
            _ => (None, expression),
        };

        let ExpressionType::FunctionCall { target, args } = &value.variant else {
            continue;
        };

        let Some((platform, function, signature)) = identifier(target)
            .and_then(|name| functions.iter().find(|(_, function, _)| *function == name))
        else {
            continue;
        };

        let arguments: Vec<(String, &Expression)> = args.iter()
            .enumerate()
            .filter_map(|(index, argument)| match &argument.variant {
                ExpressionType::Assignment { target, value } => {
                    Some((identifier(target)?.to_string(), value.as_ref()))
                }
                _ => Some((signature.parameters.fixed.get(index)?.name.clone(), argument)),
            })
            .collect();

        let name = match arguments.iter().find(|(name, _)| name == "name") {
            Some((_, Expression { variant: ExpressionType::String(name), .. })) => name.clone(),
            _ => variable.unwrap_or(function).to_string(),
        };

        calls.push(ModuleCall {
            name,
            function,
            platform,
            signature: signature.clone(),
            statement_start: statement.span.start,
            call: value,
            arguments,
        });
    }

    return calls;
}

impl ModuleCall<'_> {
    pub fn argument(&self, parameter: &str) -> Option<&Expression> {
        return self.arguments.iter()
            .find(|(name, _)| name == parameter)
            .map(|(_, argument)| *argument);
    }

    pub fn has_parameter(&self, parameter: &str) -> bool {
        return self.signature.parameters.fixed.iter().any(|param| param.name == parameter);
    }

    /// The variable the argument names, if it does
    pub fn variable(&self, parameter: &str) -> Option<&str> {
        return self.argument(parameter).and_then(identifier);
    }
}

/// The script with `notation` in the list `parameter` of the module: a variable's list
/// where it is assigned, a missing argument added to the call
pub fn with_dependency(
    source: &str,
    script: &Script,
    module: &ModuleCall,
    parameter: &str,
    notation: &str,
) -> Result<DependencyEdit, String> {
    let quoted = format!("\"{notation}\"");

    let Some(argument) = module.argument(parameter) else {
        let ExpressionType::FunctionCall { target, args } = &module.call.variant else {
            unreachable!("a module call is a function call");
        };

        let argument = format!("{parameter} = list({quoted})");
        let edits = appended_argument(source, target, module.call, args, &argument);

        return Ok(DependencyEdit {
            script: apply_edits(source, edits),
            change: DependencyChange::Added,
            variable: None,
        });
    };

    let variable = identifier(argument);

    let list = match variable {
        Some(variable) => last_assignment(script, variable, module.statement_start).ok_or_else(|| {
            format!(
                "{} names {variable} as its {parameter}, which is assigned nowhere before",
                module.name
            )
        })?,
        None => argument,
    };

    let ExpressionType::FunctionCall { target, args } = &list.variant else {
        return Err(not_a_list(module, parameter, notation));
    };

    if identifier(target) != Some("list") {
        return Err(not_a_list(module, parameter, notation));
    }

    let library = library_of(notation);

    let existing = args.iter().find_map(|argument| match &argument.variant {
        ExpressionType::String(present) if library_of(present) == library => {
            Some((argument, present))
        }
        _ => None,
    });

    let (script, change) = match existing {
        Some((_, present)) if present == notation => {
            (source.to_string(), DependencyChange::Present)
        }
        Some((argument, present)) => {
            let mut script = source.to_string();
            script.replace_range(argument.span.start..argument.span.end, &quoted);
            (script, DependencyChange::Replaced(present.clone()))
        }
        None => (
            apply_edits(source, appended_argument(source, target, list, args, &quoted)),
            DependencyChange::Added,
        ),
    };

    return Ok(DependencyEdit {
        script,
        change,
        variable: variable.map(str::to_string),
    });
}

fn not_a_list(module: &ModuleCall, parameter: &str, notation: &str) -> String {
    return format!(
        "the {parameter} of {} are no list(...) install can add to, add \"{notation}\" yourself",
        module.name
    );
}

/// group:artifact of a Maven notation, which versions do not change, else the vcpkg port, which its
/// features do not change: llvm of llvm[clang]
fn library_of(notation: &str) -> &str {
    match notation.match_indices(':').nth(1) {
        Some((second_colon, _)) => return &notation[..second_colon],
        None => return notation.split('[').next().unwrap_or(notation),
    }
}

/// The value of the last top level assignment to `variable` before `before`
fn last_assignment<'a>(
    script: &'a Script,
    variable: &str,
    before: usize,
) -> Option<&'a Expression> {
    return script.statements.iter()
        .take_while(|statement| statement.span.start < before)
        .filter_map(|statement| match &statement.variant {
            StatementType::Expression(Expression {
                variant: ExpressionType::Assignment { target, value },
                ..
            }) if identifier(target) == Some(variable) => Some(value.as_ref()),
            _ => None,
        })
        .last();
}

/// `text` as the last argument of a call, in its style: on a line of its own
/// when the arguments are, with a comma after it when the last one has one
fn appended_argument(
    source: &str,
    target: &Expression,
    call: &Expression,
    args: &[Expression],
    text: &str,
) -> Vec<(usize, String)> {
    let open = target.span.end + source[target.span.end..].find('(').unwrap_or(0);
    let close = call.span.end - 1;

    let Some(last) = args.last() else {
        return vec![(open + 1, text.to_string())];
    };

    let after_last = &source[last.span.end..close];
    let comma = after_last.find(|char: char| !char.is_whitespace())
        .filter(|index| after_last[*index..].starts_with(','))
        .map(|index| last.span.end + index + 1);

    let on_own_lines = source[open..args[0].span.start].contains('\n');

    if !on_own_lines {
        return match comma {
            Some(comma) => vec![(comma, format!(" {text}"))],
            None => vec![(last.span.end, format!(", {text}"))],
        };
    }

    let line = start_of_line(source, last.span.start);
    let indent: String = source[line..].chars()
        .take_while(|char| *char == ' ' || *char == '\t')
        .collect();

    // after a comment that ends the line of the last argument
    let line_end = |from: usize| {
        source[from..close].find('\n')
            .map_or(from, |newline| from + newline)
    };

    return match comma {
        Some(comma) => vec![(line_end(comma), format!("\n{indent}{text},"))],
        None => vec![
            (last.span.end, ",".to_string()),
            (line_end(last.span.end), format!("\n{indent}{text}")),
        ],
    };
}

fn apply_edits(source: &str, mut edits: Vec<(usize, String)>) -> String {
    let mut result = source.to_string();

    // from the end, so earlier offsets stay valid; at one offset the last added goes in first
    let mut order: Vec<usize> = (0..edits.len()).collect();
    order.sort_by_key(|index| std::cmp::Reverse((edits[*index].0, *index)));
    for index in order {
        let (offset, text) = std::mem::take(&mut edits[index]);
        result.insert_str(offset, &text);
    }

    return result;
}

/// The arguments of a `list(...)` call and the offset right after its parenthesis
fn list_arguments<'a>(source: &str, value: &'a Expression) -> Option<(usize, &'a [Expression])> {
    let ExpressionType::FunctionCall { target, args } = &value.variant else {
        return None;
    };

    if identifier(target) != Some("list") {
        return None;
    }

    let paren = target.span.end + source[target.span.end..].find('(')?;

    return Some((paren + 1, args));
}

/// Whether the expression calls the function `name`
fn calls_function(expression: &Expression, name: &str) -> bool {
    let ExpressionType::FunctionCall { target, .. } = &expression.variant else {
        return false;
    };
    return identifier(target) == Some(name);
}

fn identifier(expression: &Expression) -> Option<&str> {
    let ExpressionType::Ident(name) = &expression.variant else {
        return None;
    };
    return Some(name);
}

/// `name` of a named argument `name = value`
fn argument_name(argument: &Expression) -> Option<&str> {
    let ExpressionType::Assignment { target, .. } = &argument.variant else {
        return None;
    };
    return identifier(target);
}

fn start_of_line(source: &str, offset: usize) -> usize {
    return source[..offset].rfind('\n')
        .map(|newline| newline + 1)
        .unwrap_or(0);
}

/// The start of the line, or of the comment lines right above it, which belong to it
fn start_of_line_with_comments(source: &str, offset: usize) -> usize {
    let mut start = start_of_line(source, offset);
    while start > 0 {
        let previous = start_of_line(source, start - 1);
        if !source[previous..start].trim_start().starts_with("//") {
            break;
        }
        start = previous;
    }
    return start;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(name: &str) -> NewModule {
        return NewModule {
            name: name.to_string(),
            directory: Some(name.to_string()),
            package: format!("com.example.{name}"),
            dependencies: vec![],
        };
    }

    fn template(
        platform: &'static str,
        repositories: &'static [&'static str],
        is_app: bool,
    ) -> ModuleTemplate {
        return ModuleTemplate {
            platform,
            repositories,
            is_app,
            files: vec![],
            function: if platform == "android" {
                "build_android_library"
            } else {
                "build_kotlin"
            },
            positional: vec!["\"lib\"".to_string(), "list()".to_string()],
            named: vec![],
        };
    }

    #[test]
    fn libraries_go_before_the_first_app_with_its_comments() {
        let source = "use kotlin\n\nrepositories = list(maven_central())\n\n// the app\n// starts here\napp = build_kotlin(\"app\", list(), repositories, main_class = \"MainKt\")\n";
        let result = with_module(
            source,
            "lib",
            &module("lib"),
            &template("kotlin", &["maven_central()"], false),
        )
        .unwrap();
        assert_eq!(
            result,
            "use kotlin\n\nrepositories = list(maven_central())\n\nlib = build_kotlin(\n    \"lib\",\n    list(),\n    repositories,\n    directory = \"lib\",\n)\n\n// the app\n// starts here\napp = build_kotlin(\"app\", list(), repositories, main_class = \"MainKt\")\n"
        );
    }

    #[test]
    fn missing_uses_and_repositories_are_added() {
        let source = "use kotlin\nrepositories = list(maven_central()) // central\ncore = build_kotlin(\"core\", list(), repositories)";
        let result = with_module(
            source,
            "ui",
            &module("ui"),
            &template("android", &["google()", "maven_central()"], false),
        )
        .unwrap();
        assert_eq!(
            result,
            "use kotlin\nuse android\nrepositories = list(google(), maven_central()) // central\ncore = build_kotlin(\"core\", list(), repositories)\n\nui = build_android_library(\n    \"lib\",\n    list(),\n    repositories,\n    directory = \"ui\",\n)\n"
        );

        // a script without anything gets all of it
        let result = with_module(
            "",
            "lib",
            &module("lib"),
            &template("kotlin", &["maven_central()"], false),
        )
        .unwrap();
        assert_eq!(
            result,
            "use kotlin\n\nrepositories = list(maven_central())\n\nlib = build_kotlin(\n    \"lib\",\n    list(),\n    repositories,\n    directory = \"lib\",\n)\n"
        );

        // repositories that are no list(...) stay as they are, the module names its own
        let result = with_module(
            "use kotlin\nrepositories = shared\n",
            "lib",
            &module("lib"),
            &template("kotlin", &["maven_central()"], false),
        )
        .unwrap();
        assert!(result.contains("    list(maven_central()),\n"), "{result}");
    }

    #[test]
    fn names_already_in_the_script_are_refused() {
        let source = "use kotlin\ncore = build_kotlin(\"core\", list(), list(maven_central()))\n";
        let error = with_module(
            source,
            "core_lib",
            &module("core"),
            &template("kotlin", &["maven_central()"], false),
        )
        .unwrap_err();
        assert!(error.contains("module named core"), "{error}");
        let error = with_module(
            source,
            "core",
            &module("other"),
            &template("kotlin", &["maven_central()"], false),
        )
        .unwrap_err();
        assert!(error.contains("variable core"), "{error}");
        assert!(
            with_module(
                "x = list(",
                "lib",
                &module("lib"),
                &template("kotlin", &[], false)
            )
            .unwrap_err()
            .starts_with("line 1:")
        );
    }

    fn installed(
        source: &str,
        module: &str,
        parameter: &str,
        notation: &str,
    ) -> Result<DependencyEdit, String> {
        let script = Parser::new(source).parse_script().unwrap();
        let calls = module_calls(&script);
        let call = calls.iter()
            .find(|call| call.name == module)
            .unwrap();
        return with_dependency(source, &script, call, parameter, notation);
    }

    #[test]
    fn module_calls_map_positional_arguments_by_the_signature() {
        let source = "use java\nuse android\n\nrepositories = list(google())\nlib = build_java(\"core\", list(), repositories)\napp = build_android(\"app\", \"com.example\", list(lib), repositories)\n";
        let script = Parser::new(source).parse_script().unwrap();
        let calls = module_calls(&script);

        let names: Vec<(&str, &str, &str)> = calls.iter()
            .map(|call| (call.name.as_str(), call.function, call.platform))
            .collect();
        assert_eq!(
            names,
            [("core", "build_java", "java"), ("app", "build_android", "android")]
        );

        // the namespace comes before the dependencies of Android modules
        assert!(matches!(
            &calls[1].argument("dependencies").unwrap().variant,
            ExpressionType::FunctionCall { .. }
        ));
        assert_eq!(calls[1].variable("repositories"), Some("repositories"));
        assert!(calls[1].has_parameter("android_test_dependencies"));
        assert!(!calls[0].has_parameter("android_test_dependencies"));
    }

    #[test]
    fn libraries_go_into_the_list_in_its_style() {
        // a missing argument is added to the call, on a line of its own like the others
        let source = "use java\n\nplugin = build_java(\n    \"plugin\",\n    list(),\n    list(maven_central()),\n)\n";
        let edit = installed(source, "plugin", "compile_only", "org.spigotmc:spigot-api:26.3-R0.1-SNAPSHOT").unwrap();
        assert_eq!(
            edit.script,
            "use java\n\nplugin = build_java(\n    \"plugin\",\n    list(),\n    list(maven_central()),\n    compile_only = list(\"org.spigotmc:spigot-api:26.3-R0.1-SNAPSHOT\"),\n)\n"
        );
        assert_eq!((edit.change, edit.variable), (DependencyChange::Added, None));

        // empty and one line lists
        let source = "use kotlin\napp = build_kotlin(\"app\", list(), list(maven_central()), test_dependencies = list(\"a:b:1\"))\n";
        assert_eq!(
            installed(source, "app", "dependencies", "g:x:2").unwrap().script,
            "use kotlin\napp = build_kotlin(\"app\", list(\"g:x:2\"), list(maven_central()), test_dependencies = list(\"a:b:1\"))\n"
        );
        assert_eq!(
            installed(source, "app", "test_dependencies", "g:x:2").unwrap().script,
            "use kotlin\napp = build_kotlin(\"app\", list(), list(maven_central()), test_dependencies = list(\"a:b:1\", \"g:x:2\"))\n"
        );

        // a list in a variable, without a comma after its last entry, which a comment follows
        let source = "use kotlin\ndeps = list(\n    \"a:b:1\", // first\n    \"c:d:2\" // second\n)\napp = build_kotlin(\"app\", deps, list(maven_central()))\n";
        let edit = installed(source, "app", "dependencies", "g:x:2").unwrap();
        assert_eq!(
            edit.script,
            "use kotlin\ndeps = list(\n    \"a:b:1\", // first\n    \"c:d:2\", // second\n    \"g:x:2\"\n)\napp = build_kotlin(\"app\", deps, list(maven_central()))\n"
        );
        assert_eq!(edit.variable.as_deref(), Some("deps"));

        // with a comma after the last entry, and a comment after that
        let source = "use kotlin\napp = build_kotlin(\n    \"app\",\n    list(\n        \"a:b:1\", // first\n    ),\n    list(maven_central()),\n)\n";
        assert_eq!(
            installed(source, "app", "dependencies", "g:x:2").unwrap().script,
            "use kotlin\napp = build_kotlin(\n    \"app\",\n    list(\n        \"a:b:1\", // first\n        \"g:x:2\",\n    ),\n    list(maven_central()),\n)\n"
        );
    }

    #[test]
    fn other_versions_are_replaced_and_lists_install_cannot_change_are_refused() {
        let source = "use kotlin\napp = build_kotlin(\"app\", list(\"a:b:1\", \"g:x:1\"), list(maven_central()))\n";

        let edit = installed(source, "app", "dependencies", "g:x:2").unwrap();
        assert_eq!(
            edit.script,
            "use kotlin\napp = build_kotlin(\"app\", list(\"a:b:1\", \"g:x:2\"), list(maven_central()))\n"
        );
        assert_eq!(edit.change, DependencyChange::Replaced("g:x:1".to_string()));

        let edit = installed(source, "app", "dependencies", "g:x:1").unwrap();
        assert_eq!((edit.script.as_str(), edit.change), (source, DependencyChange::Present));

        let computed = "use kotlin\nextra = list()\napp = build_kotlin(\"app\", list(\"a:b:1\") + extra, list(maven_central()))\n";
        let error = installed(computed, "app", "dependencies", "g:x:2").unwrap_err();
        assert!(error.contains("no list(...)"), "{error}");

        let unassigned = "use kotlin\napp = build_kotlin(\"app\", deps, list(maven_central()))\ndeps = list()\n";
        let error = installed(unassigned, "app", "dependencies", "g:x:2").unwrap_err();
        assert!(error.contains("assigned nowhere before"), "{error}");
    }
}
