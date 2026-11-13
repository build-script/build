// Runtime: evaluates build scripts, `use` loads the build functions of a platform
mod error;
mod functions;
mod modules;
mod prelude;
mod testing;
mod values;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::common::semaphore::Semaphore;
use crate::common::system;
use crate::jvm;
use crate::language::lexer::{Span, line_column};
use crate::language::parser::{
    BinaryOperator, Expression, ExpressionType, Identifier, Parameter, Parser, Script, Statement, StatementType,
    TypeExpression, TypeExpressionType, UnaryOperator,
};

use prelude::core_functions;
use functions::{BuiltinFunction, CallArgument, FunctionParameter, UserFunction, bind};

pub use error::{RuntimeError, RuntimeErrorType};
pub use functions::{
    Arguments, FromValue, FunctionParameters, FunctionSignature, ParameterDefault, builtin,
};
pub use modules::{BuildFailure, BuiltModule, ModuleOutput, PendingModule, launch};
pub use testing::{ModuleTests, TestCounts, TestOutcome, TestRun, TestRunner, TestSuite};
pub use values::{Type, Value};

/// The build functions a script loads with `use <name>`
pub struct Platform {
    pub name: &'static str,
    pub functions: fn() -> Vec<(&'static str, Value)>,
}

/// What builtins know about the running script
#[derive(Clone)]
pub struct BuildScriptContext {
    /// relative paths of the script are resolved against it
    pub project_dir: PathBuf,
    /// modules that run their tests once they are built, `build test`
    pub tests: TestSelection,
    /// after `build test --`, for the test programs of C and C++ modules, like --gtest_filter=Parser.*
    pub test_arguments: Vec<String>,
    /// `build --release`: Android apps are built for release
    pub release: bool,
    /// modules write their module files for IntelliJ, see `build idea`
    pub idea: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TestSelection {
    /// `build` and `run`
    None,
    All,
    Module(String),
}

pub struct Runtime {
    /// the scope of the script, or of the function being called
    scope: SharedScope,
    /// true, false and the core functions: the script and every plugin have a scope of their own below it
    core: SharedScope,
    context: BuildScriptContext,
    /// every module a builtin returned, in build order, maybe still building
    built: Vec<Arc<PendingModule>>,
    jobs: Arc<Semaphore>,
    call_depth: usize,
    /// the script and the plugins, what the sources of spans count: the script is 0
    sources: Vec<SourceFile>,
    /// what each plugin loaded defines, by its canonical path
    plugins: HashMap<PathBuf, Vec<(String, Value)>>,
    /// plugins being loaded, a plugin that uses one of them uses itself
    loading: Vec<PathBuf>,
    /// the script functions being called, outermost first: an error in a plugin names where the script called it
    calls: Vec<(String, Span)>,
    /// the repository of the official plugins
    official_plugins: String,
}

/// A file the spans of a runtime point into
struct SourceFile {
    path: PathBuf,
    text: String,
}

/// The platforms of every language category
const PLATFORMS: &[&[Platform]] = &[jvm::PLATFORMS, crate::native::PLATFORMS];

/// A function that calls itself without an if that ends it never returns, this catches it
const MAX_CALL_DEPTH: usize = 64;

/// Every name a script has without defining it: true and false, the core
/// functions and those of every platform
pub fn builtin_names() -> Vec<&'static str> {
    let mut names = vec!["true", "false"];
    names.extend(core_functions().into_iter().map(|(name, _)| name));

    for platform in platforms() {
        names.extend((platform.functions)().into_iter().map(|(name, _)| name));
    }

    return names;
}

impl TestSelection {
    pub fn includes(&self, module_name: &str) -> bool {
        match self {
            TestSelection::None => return false,
            TestSelection::All => return true,
            TestSelection::Module(name) => return name == module_name,
        }
    }
}

impl Runtime {
    pub fn new(project_dir: impl Into<PathBuf>) -> Self {
        // absolute, so paths such as the one returned by build_kotlin stay valid everywhere
        let project_dir = project_dir.into();
        let project_dir = system::canonical_path(&project_dir).unwrap_or(project_dir);

        let core = Arc::new(Mutex::new(Scope::default()));

        {
            let mut core = core.lock().unwrap();
            core.variables.insert("true".to_string(), Value::Bool(true));
            core.variables.insert("false".to_string(), Value::Bool(false));

            for (name, value) in core_functions() {
                core.variables.insert(name.to_string(), value);
            }
        }

        return Runtime {
            scope: Scope::child(&core),
            core,
            sources: vec![SourceFile {
                path: project_dir.join("project.build"),
                text: String::new(),
            }],
            context: BuildScriptContext {
                project_dir,
                tests: TestSelection::None,
                test_arguments: vec![],
                release: false,
                idea: false,
            },
            built: vec![],
            jobs: Arc::new(Semaphore::new(default_jobs())),
            call_depth: 0,
            plugins: HashMap::new(),
            loading: vec![],
            calls: vec![],
            official_plugins: crate::plugins::official_repository(),
        };
    }

    /// Another repository of the official plugins, a local one in tests
    #[cfg(test)]
    pub fn with_official_plugins(mut self, repository: &str) -> Self {
        self.official_plugins = repository.to_string();
        return self;
    }

    /// The script as the command line names it, for the places of errors
    pub fn with_script(mut self, path: &Path, text: &str) -> Self {
        self.sources[0] = SourceFile {
            path: path.to_path_buf(),
            text: text.to_string(),
        };
        return self;
    }

    /// path:line:column of a span, a plugin's path relative to the project when it is in there, with / like
    /// scripts write it in use, also on Windows
    pub fn location(&self, span: Span) -> String {
        let Some(source) = self.sources.get(span.source) else {
            return format!("?:{}", span.start);
        };

        let path = system::relative_with_slashes(&self.context.project_dir, &source.path);
        let (line, column) = line_column(&source.text, span.start);

        return format!("{path}:{line}:{column}");
    }

    /// The error with where it happened. One in a plugin starts with where the script called it.
    pub fn describe(&self, error: &RuntimeError) -> String {
        match &error.called_from {
            Some((function, call)) => {
                return format!("{}: {function}: {error} (in {})", self.location(*call), self.location(error.span));
            }
            None => return format!("{}: {error}", self.location(error.span)),
        }
    }

    pub fn with_tests(mut self, tests: TestSelection) -> Self {
        self.context.tests = tests;
        return self;
    }

    pub fn with_test_arguments(mut self, arguments: Vec<String>) -> Self {
        self.context.test_arguments = arguments;
        return self;
    }

    pub fn with_release(mut self, release: bool) -> Self {
        self.context.release = release;
        return self;
    }

    pub fn with_idea(mut self, idea: bool) -> Self {
        self.context.idea = idea;
        return self;
    }

    pub fn with_jobs(mut self, jobs: usize) -> Self {
        self.jobs = Arc::new(Semaphore::new(jobs));
        return self;
    }

    pub fn run(&mut self, script: &Script) -> Result<(), RuntimeError> {
        for statement in &script.statements {
            self.run_statement(statement)?;
        }
        return Ok(());
    }

    /// The value of `expression` after the assignments before `before`, without
    /// building: of the platform functions only `functions` are there, assignments
    /// that need others are left out. For install, which reads a module's repositories.
    pub fn evaluate_unbuilt(
        &mut self,
        script: &Script,
        before: usize,
        functions: Vec<(&'static str, Value)>,
        expression: &Expression,
    ) -> Result<Value, RuntimeError> {
        for (name, value) in functions {
            self.define(name, value);
        }

        for statement in &script.statements {
            if statement.span.start >= before {
                break;
            }

            let StatementType::Expression(expression) = &statement.variant else {
                continue;
            };

            // their variables stay undefined, whatever uses them fails
            if matches!(expression.variant, ExpressionType::Assignment { .. }) {
                let _ = self.evaluate(expression);
            }
        }

        return self.evaluate(expression);
    }

    /// Waits for the builds still running. The modules in build order, or why builds
    /// failed: a module whose dependency failed leaves the error to that one.
    pub fn finish(&self) -> Result<Vec<Arc<BuiltModule>>, Vec<RuntimeError>> {
        let mut modules = vec![];
        let mut errors = vec![];

        for pending in &self.built {
            match pending.wait() {
                Ok(module) => modules.push(module),
                Err(BuildFailure::Failed(message)) => {
                    let mut error = RuntimeError::builtin_failed(pending.function, message, pending.span);
                    error.called_from = pending.called_from.clone();
                    errors.push(error);
                }
                Err(BuildFailure::DependencyFailed) => {}
            }
        }

        if errors.is_empty() {
            return Ok(modules);
        }

        return Err(errors);
    }

    /// In the current scope, also when an outer scope has the name
    fn define(&mut self, name: &str, value: Value) {
        self.scope
            .lock().unwrap()
            .variables
            .insert(name.to_string(), value);
    }

    fn run_statement(&mut self, statement: &Statement) -> Result<(), RuntimeError> {
        match &statement.variant {
            StatementType::Expression(expression) => {
                self.evaluate(expression)?;
                return Ok(());
            }
            StatementType::Use(platform) => return self.load_platform(&platform.name, statement.span),
            StatementType::UsePlugin(plugin) => return self.load_plugin(plugin, statement.span),
        }
    }

    /// A file of the project or a plugin of a repository: `use "./go.build"`, `use "owner/repository"`
    fn load_plugin(&mut self, plugin: &str, span: Span) -> Result<(), RuntimeError> {
        if plugin.starts_with("./") || plugin.starts_with("../") {
            // relative to the file with the use
            let file = self.source_directory(span.source).join(plugin);
            return self.load_plugin_file(plugin, file, span);
        }

        let file = crate::plugins::PluginSource::of_repository(plugin)
            .and_then(|source| crate::plugins::locate(&source, &system::cache_dir("git")))
            .map_err(|message| {
                let plugin = plugin.to_string();
                return RuntimeError::new(RuntimeErrorType::PluginFailed { plugin, message }, span);
            })?;

        return self.load_plugin_file(plugin, file, span);
    }

    /// Defines what the plugin defines, loaded once and in a scope of its own
    fn load_plugin_file(&mut self, plugin: &str, file: PathBuf, span: Span) -> Result<(), RuntimeError> {
        let path = system::canonical_path(&file).map_err(|_| {
            let (plugin, path) = (plugin.to_string(), file.display().to_string());
            return RuntimeError::new(RuntimeErrorType::PluginNotFound { plugin, path }, span);
        })?;

        if self.loading.contains(&path) {
            let plugin = plugin.to_string();
            return Err(RuntimeError::new(RuntimeErrorType::PluginCycle { plugin }, span));
        }

        let definitions = match self.plugins.get(&path) {
            Some(definitions) => definitions.clone(),
            None => {
                let definitions = self.evaluate_plugin(&path, span)?;
                self.plugins.insert(path, definitions.clone());
                definitions
            }
        };

        for (name, value) in definitions {
            let defined = self.scope.lock().unwrap().get(&name);

            // the same plugin used twice defines the same values
            if let Some(defined) = defined && Self::values_equal(&defined, &value) != Some(true) {
                let plugin = plugin.to_string();
                return Err(RuntimeError::new(RuntimeErrorType::PluginNameTaken { plugin, name }, span));
            }

            self.define(&name, value);
        }

        return Ok(());
    }

    /// The names a plugin assigns at its top, but those starting with _, and their values
    fn evaluate_plugin(&mut self, path: &Path, span: Span) -> Result<Vec<(String, Value)>, RuntimeError> {
        let text = std::fs::read_to_string(path).map_err(|error| {
            let (plugin, path) = (path.display().to_string(), format!("{}: {error}", path.display()));
            return RuntimeError::new(RuntimeErrorType::PluginNotFound { plugin, path }, span);
        })?;

        let source = self.sources.len();
        self.sources.push(SourceFile {
            path: path.to_path_buf(),
            text: text.clone(),
        });

        let script = Parser::of_source(&text, source)
            .parse_script()
            .map_err(|error| {
                let message = error.variant.to_string();
                return RuntimeError::new(RuntimeErrorType::PluginParse { message }, error.span);
            })?;

        // the plugin sees the core functions, never the variables of the script
        let scope = Scope::child(&self.core);
        let directory = path.parent().unwrap_or(path).display().to_string();
        scope.lock().unwrap().variables.insert("plugin_directory".to_string(), Value::String(directory));

        let outer = std::mem::replace(&mut self.scope, scope.clone());
        self.loading.push(path.to_path_buf());
        let evaluated = self.run(&script);
        self.loading.pop();
        self.scope = outer;
        evaluated?;

        let mut definitions: Vec<(String, Value)> = vec![];

        for statement in &script.statements {
            let StatementType::Expression(expression) = &statement.variant else { continue };
            let ExpressionType::Assignment { target, .. } = &expression.variant else { continue };

            if target.name.starts_with('_') || definitions.iter().any(|(name, _)| *name == target.name) {
                continue;
            }

            if let Some(value) = scope.lock().unwrap().variables.get(&target.name) {
                definitions.push((target.name.clone(), value.clone()));
            }
        }

        return Ok(definitions);
    }

    /// Of the file with this number: the project for the script
    fn source_directory(&self, source: usize) -> PathBuf {
        if source == 0 {
            return self.context.project_dir.clone();
        }

        return self.sources[source].path
            .parent()
            .map_or_else(|| self.context.project_dir.clone(), Path::to_path_buf);
    }

    /// A platform of build-script, otherwise a plugin of the official list: `use go`, `use go@1.4`
    fn load_platform(&mut self, name: &str, span: Span) -> Result<(), RuntimeError> {
        let Some(platform) = platforms().find(|platform| platform.name == name) else {
            let repository = self.official_plugins.clone();
            let source = crate::plugins::PluginSource::official(name, &repository);

            let file = crate::plugins::locate(&source, &system::cache_dir("git")).map_err(|message| {
                // the list helps with a misspelled name, when it can be had
                let official = crate::plugins::official_plugins(&repository, &system::cache_dir("git"))
                    .map(|plugins| plugins.into_iter().map(|(name, ..)| name).collect())
                    .unwrap_or_default();
                let name = name.to_string();
                return RuntimeError::new(RuntimeErrorType::UnknownPlugin { name, official, message }, span);
            })?;

            return self.load_plugin_file(name, file, span);
        };

        for (function, value) in (platform.functions)() {
            self.define(function, value);
        }

        return Ok(());
    }

    fn evaluate(&mut self, expression: &Expression) -> Result<Value, RuntimeError> {
        let span = expression.span;

        match &expression.variant {
            ExpressionType::String(content) => return Ok(Value::String(content.clone())),

            ExpressionType::Ident(name) => {
                // the lexer reads numbers as identifiers
                if let Ok(int) = name.parse::<i64>() {
                    return Ok(Value::Int(int));
                }
                return self.scope.lock()
                    .unwrap()
                    .get(name)
                    .ok_or_else(|| {
                        let name = name.clone();
                        RuntimeError::new(RuntimeErrorType::UndefinedVariable { name }, span)
                    });
            }

            ExpressionType::Assignment { target, value } => return self.evaluate_assignment(target, value),

            ExpressionType::Function {
                params,
                returns,
                body,
            } => return self.define_function("fn", params, returns.as_ref(), body),

            ExpressionType::Unary { operator, target } => {
                let value = self.evaluate(target)?;
                return Self::evaluate_unary(operator, value, span);
            }

            // the right side only runs when the left one does not decide
            ExpressionType::Binary {
                left,
                operator: operator @ (BinaryOperator::And | BinaryOperator::Or),
                right,
            } => {
                let what = if matches!(operator, BinaryOperator::And) { "&&" } else { "||" };
                let left = self.evaluate_bool(left, what)?;

                if left == matches!(operator, BinaryOperator::Or) {
                    return Ok(Value::Bool(left));
                }

                return Ok(Value::Bool(self.evaluate_bool(right, what)?));
            }

            ExpressionType::Binary {
                left,
                operator,
                right,
            } => {
                let left = self.evaluate(left)?;
                let right = self.evaluate(right)?;

                return Self::evaluate_binary(operator, left, right, span);
            }

            ExpressionType::If {
                condition,
                then,
                otherwise,
            } => {
                let block = match self.evaluate_bool(condition, "the condition of an if")? {
                    true => Some(then),
                    false => otherwise.as_ref(),
                };

                return match block {
                    Some(block) => self.evaluate_in(Scope::child(&self.scope), block),
                    None => Ok(Value::Unit),
                };
            }

            ExpressionType::FunctionCall { target, args } => return self.evaluate_call(target, args, span),
        }
    }

    /// A function is called by the name it is defined with in messages
    fn evaluate_assignment(&mut self, target: &Identifier, value: &Expression) -> Result<Value, RuntimeError> {
        let name = &target.name;

        let value = match &value.variant {
            ExpressionType::Function {
                params,
                returns,
                body,
            } => self.define_function(name, params, returns.as_ref(), body)?,
            _ => self.evaluate(value)?,
        };

        self.define(name, value);

        return Ok(Value::Unit);
    }

    /// `name = value` in an argument list is a named argument, not an assignment
    fn evaluate_call(&mut self, target: &Expression, args: &[Expression], span: Span) -> Result<Value, RuntimeError> {
        let callee = self.evaluate(target)?;
        let mut arguments = Vec::with_capacity(args.len());

        for arg in args {
            if let ExpressionType::Assignment { target, value } = &arg.variant {
                arguments.push(CallArgument::Named(target.name.clone(), self.evaluate(value)?, arg.span));
            } else {
                arguments.push(CallArgument::Positional(self.evaluate(arg)?, arg.span));
            }
        }

        return self.call(callee, arguments, span);
    }

    fn evaluate_bool(
        &mut self,
        expression: &Expression,
        what: &'static str,
    ) -> Result<bool, RuntimeError> {
        match self.evaluate(expression)? {
            Value::Bool(bool) => return Ok(bool),
            other => {
                return Err(RuntimeError::new(
                    RuntimeErrorType::NotBool { what, got: other.type_of() },
                    expression.span,
                ));
            }
        }
    }

    /// A block or a function body in its own scope, the value of its last expression
    fn evaluate_in(
        &mut self,
        scope: SharedScope,
        block: &[Expression],
    ) -> Result<Value, RuntimeError> {
        let outer_scope = std::mem::replace(&mut self.scope, scope);
        let mut result = Ok(Value::Unit);

        for expression in block {
            result = self.evaluate(expression);
            if result.is_err() {
                break;
            }
        }

        self.scope = outer_scope;

        return result;
    }

    fn evaluate_unary(
        operator: &UnaryOperator,
        value: Value,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match (operator, &value) {
            (UnaryOperator::Not, Value::Bool(bool)) => return Ok(Value::Bool(!bool)),
            (UnaryOperator::Neg, Value::Int(int)) => {
                return int.checked_neg()
                    .map(Value::Int)
                    .ok_or_else(|| RuntimeError::invalid_arithmetic(operator, span));
            }
            _ => {
                let variant = RuntimeErrorType::InvalidUnaryOperand {
                    operator: operator.clone(),
                    operand: value.type_of(),
                };
                return Err(RuntimeError::new(variant, span));
            }
        }
    }

    fn evaluate_binary(
        operator: &BinaryOperator,
        left: Value,
        right: Value,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match (operator, &left, &right) {
            (BinaryOperator::Add, Value::String(l), Value::String(r)) => {
                return Ok(Value::String(format!("{l}{r}")));
            }
            (BinaryOperator::Add, Value::String(l), Value::Int(r)) => {
                return Ok(Value::String(format!("{l}{r}")));
            }
            (BinaryOperator::Add, Value::Int(l), Value::String(r)) => {
                return Ok(Value::String(format!("{l}{r}")));
            }
            (BinaryOperator::Add, Value::List(l), Value::List(r)) => {
                return Ok(Value::List(l.iter().chain(r).cloned().collect()));
            }

            (
                BinaryOperator::Add
                | BinaryOperator::Sub
                | BinaryOperator::Mul
                | BinaryOperator::Div,
                Value::Int(l),
                Value::Int(r),
            ) => {
                let result = match operator {
                    BinaryOperator::Add => l.checked_add(*r),
                    BinaryOperator::Sub => l.checked_sub(*r),
                    BinaryOperator::Mul => l.checked_mul(*r),
                    _ => l.checked_div(*r),
                };
                return result.map(Value::Int).ok_or_else(|| RuntimeError::invalid_arithmetic(operator, span));
            }

            (
                BinaryOperator::Less
                | BinaryOperator::LessEq
                | BinaryOperator::Greater
                | BinaryOperator::GreaterEq,
                _,
                _,
            ) => {
                let ordering = match (&left, &right) {
                    (Value::Int(l), Value::Int(r)) => l.cmp(r),
                    (Value::String(l), Value::String(r)) => l.cmp(r),
                    _ => {
                        return Err(RuntimeError::invalid_binary_operands(
                            operator, &left, &right, span,
                        ));
                    }
                };

                let holds = match operator {
                    BinaryOperator::Less => ordering.is_lt(),
                    BinaryOperator::LessEq => ordering.is_le(),
                    BinaryOperator::Greater => ordering.is_gt(),
                    _ => ordering.is_ge(),
                };

                return Ok(Value::Bool(holds));
            }

            (BinaryOperator::Eq | BinaryOperator::NotEq, _, _) => {
                let Some(equal) = Self::values_equal(&left, &right) else {
                    return Err(RuntimeError::invalid_binary_operands(
                        operator, &left, &right, span,
                    ));
                };
                return Ok(Value::Bool(if matches!(operator, BinaryOperator::Eq) {
                    equal
                } else {
                    !equal
                }));
            }

            _ => {
                return Err(RuntimeError::invalid_binary_operands(
                    operator, &left, &right, span,
                ));
            }
        }
    }

    /// None = the types cannot be compared
    fn values_equal(left: &Value, right: &Value) -> Option<bool> {
        match (left, right) {
            (Value::String(l), Value::String(r)) => return Some(l == r),
            (Value::Int(l), Value::Int(r)) => return Some(l == r),
            (Value::Bool(l), Value::Bool(r)) => return Some(l == r),
            (Value::Module(l), Value::Module(r)) => return Some(Arc::ptr_eq(l, r)),
            (Value::List(l), Value::List(r)) => {
                if l.len() != r.len() {
                    return Some(false);
                }

                for (l, r) in l.iter().zip(r) {
                    if !Self::values_equal(l, r)? {
                        return Some(false);
                    }
                }

                return Some(true);
            }
            (Value::Unit, Value::Unit) => return Some(true),
            (Value::Builtin { name: l, .. }, Value::Builtin { name: r, .. }) => {
                return Some(l == r);
            }
            (Value::Function(l), Value::Function(r)) => return Some(Arc::ptr_eq(l, r)),
            _ => return None,
        }
    }

    /// Resolves the types and evaluates the defaults once. The body runs in a scope below this one.
    fn define_function(
        &mut self,
        name: &str,
        params: &[Parameter],
        returns: Option<&TypeExpression>,
        body: &[Expression],
    ) -> Result<Value, RuntimeError> {
        let mut declared = FunctionParameters::new();

        for param in params {
            let param_name = &param.name.name;

            if declared.fixed.iter()
                .any(|known| known.name == *param_name)
            {
                return Err(RuntimeError::new(
                    RuntimeErrorType::DuplicateParameter {
                        function: name.to_string(),
                        param: param_name.clone(),
                    },
                    param.name.span,
                ));
            }

            let param_type = Self::resolve_type(&param.param_type)?;

            let default = match &param.default {
                Some(expression) => {
                    let value = self.evaluate(expression)?;

                    if !param_type.accepts(&value) {
                        return Err(RuntimeError::type_mismatch(
                            name,
                            param_name,
                            &param_type,
                            &value,
                            expression.span,
                        ));
                    }
                    ParameterDefault::Value(value)
                }
                None => ParameterDefault::Required,
            };

            declared.fixed.push(FunctionParameter {
                name: param_name.clone(),
                param_type,
                default,
            });
        }

        // without a declared type any value may come back
        let returns = match returns {
            Some(returns) => Self::resolve_type(returns)?,
            None => Type::Any,
        };

        return Ok(Value::Function(Arc::new(UserFunction {
            name: name.to_string(),
            signature: Arc::new(declared.returns(returns)),
            body: body.to_vec(),
            scope: self.scope.clone(),
        })));
    }

    fn resolve_type(expression: &TypeExpression) -> Result<Type, RuntimeError> {
        match &expression.variant {
            TypeExpressionType::Named(name) => match name.as_str() {
                "str" => return Ok(Type::String),
                "int" => return Ok(Type::Int),
                "bool" => return Ok(Type::Bool),
                "module" => return Ok(Type::Module),
                "any" => return Ok(Type::Any),
                "unit" => return Ok(Type::Unit),
                _ => {
                    return Err(RuntimeError::new(
                        RuntimeErrorType::UnknownType { name: name.clone() },
                        expression.span,
                    ));
                }
            },
            TypeExpressionType::Variant(name) => return Ok(Type::Variant(name.clone())),
            TypeExpressionType::List(element) => {
                return Ok(Type::list(Self::resolve_type(element)?));
            }
            TypeExpressionType::Union(members) => {
                let types: Vec<Type> = members.iter()
                    .map(Self::resolve_type)
                    .collect::<Result<_, _>>()?;

                Self::check_variants(&types, members)?;

                return Ok(Type::Union(types));
            }
            TypeExpressionType::Function { params, returns } => {
                let mut parameters = FunctionParameters::new();

                for param in params {
                    parameters.fixed.push(FunctionParameter {
                        name: String::new(),
                        param_type: Self::resolve_type(param)?,
                        default: ParameterDefault::Required,
                    });
                }

                return Ok(Type::Function(Arc::new(
                    parameters.returns(Self::resolve_type(returns)?),
                )));
            }
        }
    }

    /// The variants of a union: each once, and not beside str or any, which accept them anyway
    fn check_variants(types: &[Type], members: &[TypeExpression]) -> Result<(), RuntimeError> {
        let union = Type::Union(types.to_vec());
        let wider = types.iter().find(|member_type| matches!(member_type, Type::String | Type::Any));

        for (index, member_type) in types.iter().enumerate() {
            if !matches!(member_type, Type::Variant(_)) {
                continue;
            }

            if types[..index].contains(member_type) {
                return Err(RuntimeError::new(
                    RuntimeErrorType::DuplicateVariant {
                        variant: member_type.clone(),
                        union,
                    },
                    members[index].span,
                ));
            }

            if let Some(wider) = wider {
                return Err(RuntimeError::new(
                    RuntimeErrorType::RedundantVariant {
                        variant: member_type.clone(),
                        wider: wider.clone(),
                        union,
                    },
                    members[index].span,
                ));
            }
        }

        return Ok(());
    }

    fn call(
        &mut self,
        callee: Value,
        arguments: Vec<CallArgument>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let (name, signature, function) = match callee {
            Value::Builtin {
                name,
                signature,
                function,
            } => (name, signature, function),
            Value::Function(function) => return self.call_function(&function, arguments, span),
            other => {
                let value_type = other.type_of();
                return Err(RuntimeError::new(RuntimeErrorType::NotCallable { value_type }, span));
            }
        };

        let args = bind(name, &signature.parameters, arguments, span)?;

        // modules build on threads of their own, the script goes on meanwhile
        if matches!(signature.returns, Type::Module) {
            if !self.loading.is_empty() {
                let function = name.to_string();
                return Err(RuntimeError::new(RuntimeErrorType::ModuleInPlugin { function }, span));
            }

            // built in a plugin's function: the script's call comes first in its error
            let called_from = self.calls
                .first()
                .filter(|(_, call)| call.source != span.source)
                .cloned();
            let module = PendingModule::new(name, span, called_from);
            self.built.push(module.clone());
            self.build_in_background(module.clone(), function, args);

            return Ok(Value::Module(module));
        }

        let result = function(&self.context, &args)
            .map_err(|message| RuntimeError::builtin_failed(name, message, span))?;

        if !signature.returns.accepts(&result) {
            let message = format!(
                "internal error: returned {}, declared {}",
                result.type_of(),
                signature.returns
            );
            return Err(RuntimeError::builtin_failed(name, message, span));
        }

        return Ok(result);
    }

    /// Once the modules it depends on are built, without holding a job while it waits
    fn build_in_background(
        &self,
        module: Arc<PendingModule>,
        function: BuiltinFunction,
        args: Arguments,
    ) {
        let context = self.context.clone();
        let jobs = self.jobs.clone();

        std::thread::spawn(move || {
            let dependencies_built = args.pending_modules()
                .iter()
                .all(|dependency| dependency.wait().is_ok());

            if !dependencies_built {
                module.finish(Err(BuildFailure::DependencyFailed));
                return;
            }

            let _job = jobs.acquire();
            let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                function(&context, &args)
            }));

            let result = match built {
                Ok(Ok(Value::Module(built))) => built.wait(),
                Ok(Ok(other)) => Err(BuildFailure::Failed(format!(
                    "internal error: returned {}, declared module",
                    other.type_of()
                ))),
                Ok(Err(message)) => Err(BuildFailure::Failed(message)),
                Err(_) => Err(BuildFailure::Failed(
                    "internal error: the build panicked".to_string(),
                )),
            };

            module.finish(result);
        });
    }

    /// The body runs in a scope below the one the function was defined in, with the arguments as variables
    fn call_function(
        &mut self,
        function: &UserFunction,
        arguments: Vec<CallArgument>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        if self.call_depth >= MAX_CALL_DEPTH {
            return Err(RuntimeError::new(
                RuntimeErrorType::CallsTooDeep {
                    function: function.name.clone(),
                },
                span,
            ));
        }

        let args = bind(&function.name, &function.signature.parameters, arguments, span)?;
        let scope = Scope::child(&function.scope);

        for (name, value) in args.values {
            if let Some(value) = value {
                scope.lock().unwrap().variables.insert(name, value);
            }
        }

        self.call_depth += 1;
        self.calls.push((function.name.clone(), span));
        let result = self.evaluate_in(scope, &function.body);
        self.calls.pop();
        self.call_depth -= 1;

        // an error in a plugin starts with where the script called it
        let result = result.map_err(|mut error| {
            if error.called_from.is_none() && error.span.source != span.source {
                error.called_from = Some((function.name.clone(), span));
            }

            return error;
        })?;

        if !function.signature.returns.accepts(&result) {
            return Err(RuntimeError::new(
                RuntimeErrorType::ReturnTypeMismatch {
                    function: function.name.clone(),
                    expected: function.signature.returns.clone(),
                    got: function.signature.returns.describe_mismatch(&result),
                },
                span,
            ));
        }

        return Ok(result);
    }
}

type SharedScope = Arc<Mutex<Scope>>;

/// The variables of the script or of a function call
#[derive(Default)]
struct Scope {
    variables: HashMap<String, Value>,
    /// where names that are not defined here are looked up
    parent: Option<SharedScope>,
}

impl Scope {
    fn child(parent: &SharedScope) -> SharedScope {
        return Arc::new(Mutex::new(Scope {
            variables: HashMap::new(),
            parent: Some(parent.clone()),
        }));
    }

    fn get(&self, name: &str) -> Option<Value> {
        if let Some(value) = self.variables.get(name) {
            return Some(value.clone());
        }
        return self.parent.as_ref()?
            .lock()
            .unwrap()
            .get(name);
    }
}

pub fn platforms() -> impl Iterator<Item = &'static Platform> {
    return PLATFORMS.iter().flat_map(|category| category.iter());
}

fn platform_names() -> Vec<&'static str> {
    return platforms().map(|platform| platform.name).collect();
}

/// Half the cores: kotlinc, javac and d8 use several threads each
pub fn default_jobs() -> usize {
    return std::thread::available_parallelism()
        .map(|cores| (cores.get() / 2).max(1))
        .unwrap_or(1);
}

#[cfg(test)]
mod tests;
