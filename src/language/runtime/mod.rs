// Runtime: evaluates build scripts, `use` loads the build functions of a platform
mod error;
mod functions;
mod values;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

use crate::jvm;
use crate::language::lexer::Span;
use crate::language::parser::{
    BinaryOperator, Expression, ExpressionType, Parameter, Script, Statement, StatementType,
    TypeExpression, TypeExpressionType, UnaryOperator, UseTarget,
};

use functions::{BuiltinFunction, CallArgument, FunctionParameter, UserFunction, bind};

pub use error::{RuntimeError, RuntimeErrorType};
pub use functions::{
    Arguments, FromValue, FunctionParameters, FunctionSignature, ParameterDefault, builtin,
};
pub use values::{
    BuildFailure, BuiltModule, ModuleOutput, ModuleTests, PendingModule, TestCounts, TestOutcome,
    TestRun, TestRunner, TestSuite, Type, Value, launch,
};

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
    /// modules that run their tests once they are built, `build-script test`
    pub tests: TestSelection,
    /// `build-script build --release`: Android apps are built for release
    pub release: bool,
    /// modules write their module files for IntelliJ, see `build-script idea`
    pub idea: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TestSelection {
    /// `build-script build` and `run`
    None,
    All,
    Module(String),
}

pub struct Runtime {
    /// the scope of the script, or of the function being called
    scope: SharedScope,
    context: BuildScriptContext,
    /// every module a builtin returned, in build order, maybe still building
    built: Vec<Arc<PendingModule>>,
    jobs: Arc<Jobs>,
    call_depth: usize,
}

/// How many builds run at the same time, like the workers of Gradle
struct Jobs {
    free: Mutex<usize>,
    released: Condvar,
}

/// One of the jobs, free again when it is dropped
struct Job<'a>(&'a Jobs);

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
        let project_dir = std::fs::canonicalize(&project_dir).unwrap_or(project_dir);

        let mut runtime = Runtime {
            scope: Arc::new(Mutex::new(Scope::default())),
            context: BuildScriptContext {
                project_dir,
                tests: TestSelection::None,
                release: false,
                idea: false,
            },
            built: vec![],
            jobs: Arc::new(Jobs::new(default_jobs())),
            call_depth: 0,
        };

        runtime.define("true", Value::Bool(true));
        runtime.define("false", Value::Bool(false));

        for (name, value) in core_functions() {
            runtime.define(name, value);
        }

        return runtime;
    }

    pub fn with_tests(mut self, tests: TestSelection) -> Self {
        self.context.tests = tests;
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
        self.jobs = Arc::new(Jobs::new(jobs));
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
                Err(BuildFailure::Failed(message)) => errors.push(RuntimeError::builtin_failed(
                    pending.function,
                    message,
                    pending.span,
                )),
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
            StatementType::Use(UseTarget::Ident(platform)) => {
                return self.load_platform(&platform.name, statement.span);
            }
            StatementType::Use(UseTarget::FilePath { path, span }) => {
                let feature = format!("use with a file path (\"{path}\")");
                return Err(RuntimeError::new(RuntimeErrorType::Unsupported { feature }, *span));
            }
        }
    }

    fn load_platform(&mut self, name: &str, span: Span) -> Result<(), RuntimeError> {
        let Some(platform) = platforms().find(|platform| platform.name == name) else {
            let name = name.to_string();
            return Err(RuntimeError::new(RuntimeErrorType::UnknownPlatform { name }, span));
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

            ExpressionType::Assignment { target, value } => {
                let ExpressionType::Ident(name) = &target.variant else {
                    unreachable!("parser only allows identifiers as assignment targets")
                };

                // a function is called by the name it is defined with in messages
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

            ExpressionType::FunctionCall { target, args } => {
                let callee = self.evaluate(target)?;

                let mut arguments = Vec::with_capacity(args.len());

                for arg in args {
                    // `name = value` in an argument list is a named argument, not an assignment
                    if let ExpressionType::Assignment { target, value } = &arg.variant {
                        let ExpressionType::Ident(name) = &target.variant else {
                            unreachable!("parser only allows identifiers as assignment targets")
                        };

                        arguments.push(CallArgument::Named(
                            name.clone(),
                            self.evaluate(value)?,
                            arg.span,
                        ));
                    } else {
                        arguments.push(CallArgument::Positional(self.evaluate(arg)?, arg.span));
                    }
                }

                return self.call(callee, arguments, span);
            }
        }
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
                    .ok_or_else(|| {
                        RuntimeError::new(
                            RuntimeErrorType::InvalidArithmetic {
                                operator: format!("{operator:?}"),
                            },
                            span,
                        )
                    });
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
                return result.map(Value::Int).ok_or_else(|| {
                    RuntimeError::new(
                        RuntimeErrorType::InvalidArithmetic {
                            operator: format!("{operator:?}"),
                        },
                        span,
                    )
                });
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
            TypeExpressionType::List(element) => {
                return Ok(Type::list(Self::resolve_type(element)?));
            }
            TypeExpressionType::Union(members) => {
                return Ok(Type::Union(
                    members.iter()
                        .map(Self::resolve_type)
                        .collect::<Result<_, _>>()?,
                ));
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
            let module = PendingModule::new(name, span);
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

            let _job = jobs.take();
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
        let result = self.evaluate_in(scope, &function.body);
        self.call_depth -= 1;

        let result = result?;

        if !function.signature.returns.accepts(&result) {
            return Err(RuntimeError::new(
                RuntimeErrorType::ReturnTypeMismatch {
                    function: function.name.clone(),
                    expected: function.signature.returns.clone(),
                    got: result.type_of(),
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

impl Jobs {
    fn new(count: usize) -> Self {
        return Jobs {
            free: Mutex::new(count.max(1)),
            released: Condvar::new(),
        };
    }

    /// Waits until one is free
    fn take(&self) -> Job<'_> {
        let mut free = self.free.lock().unwrap();

        while *free == 0 {
            free = self.released.wait(free).unwrap();
        }

        *free -= 1;

        return Job(self);
    }
}

impl Drop for Job<'_> {
    fn drop(&mut self) {
        *self.0.free.lock().unwrap() += 1;
        self.0.released.notify_one();
    }
}

/// Half the cores: kotlinc, javac and d8 use several threads each
pub fn default_jobs() -> usize {
    return std::thread::available_parallelism()
        .map(|cores| (cores.get() / 2).max(1))
        .unwrap_or(1);
}

/// What every script has without `use`
fn core_functions() -> Vec<(&'static str, Value)> {
    return vec![
        (
            "print",
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
        ),
        (
            "type",
            builtin(
                "type",
                FunctionParameters::new()
                    .required("value", Type::Any)
                    .returns(Type::String),
                |_, args| Ok(args.get::<Value>("value")?.type_of().to_string().into()),
            ),
        ),
        (
            "env",
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
        ),
        (
            "has_env",
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
        ),
        (
            "git",
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
        ),
        (
            "operating_system",
            builtin(
                "operating_system",
                FunctionParameters::new().returns(Type::String),
                // "macos", "linux" or "windows"
                |_, _| Ok(std::env::consts::OS.into()),
            ),
        ),
        (
            "current_time_millis",
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
        ),
        (
            "list",
            builtin(
                "list",
                FunctionParameters::new()
                    .variadic("items", Type::Any)
                    .returns(Type::list(Type::Any)),
                |_, args| Ok(Value::List(args.rest()?)),
            ),
        ),
    ];
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use crate::jvm::{JvmOutput, maven};
    use crate::language::parser::Parser;

    use super::*;

    fn run(source: &str) -> Result<Runtime, RuntimeError> {
        return run_in(Runtime::new("."), source);
    }

    fn run_in(mut runtime: Runtime, source: &str) -> Result<Runtime, RuntimeError> {
        let script = Parser::new(source)
            .parse_script()
            .expect("script should parse");

        runtime.run(&script)?;

        // like the command line: the errors of builds count once they are done
        if let Err(mut errors) = runtime.finish() {
            return Err(errors.remove(0));
        }

        return Ok(runtime);
    }

    fn variable(runtime: &Runtime, name: &str) -> Value {
        return runtime.scope.lock()
            .unwrap()
            .get(name)
            .unwrap_or_else(|| panic!("{name} is not defined"));
    }

    fn string(runtime: &Runtime, name: &str) -> String {
        return variable(runtime, name).to_string();
    }

    #[test]
    fn strings_take_escapes_and_must_be_closed() {
        let runtime = run(r#"x = "GPLATFORM=\"darwin\" \\ \t C:\Users""#).unwrap();
        assert_eq!(string(&runtime, "x"), "GPLATFORM=\"darwin\" \\ \t C:\\Users");
        assert_eq!(variable(&runtime, "x").literal(), r#""GPLATFORM=\"darwin\" \\ \t C:\\Users""#);

        let error = Parser::new("x = list(\"abc)").parse_script().unwrap_err();
        assert_eq!(error.variant.to_string(), "expected \" to close the string, got the end of the script");
    }

    #[test]
    fn conditions_choose_their_block() {
        let runtime = run(r#"
            ci = "true"
            jdk = if ci == "true" { "21" } else { "17" }
            size = if 3 > 5 { "big" } else if 3 >= 3 { "same" } else { "small" }
            nothing = if false { "x" }
            both = 1 < 2 && "a" <= "b" || false
            pick = fn (ci: bool) -> str { if ci { "ci" } else { "local" } }
            local = pick(false)
        "#)
        .unwrap();

        assert_eq!(string(&runtime, "jdk"), "21");
        assert_eq!(string(&runtime, "size"), "same");
        assert!(matches!(variable(&runtime, "nothing"), Value::Unit));
        assert_eq!(string(&runtime, "both"), "true");
        assert_eq!(string(&runtime, "local"), "local");

        // the right side only runs when the left one does not decide
        assert!(run("x = false && undefined").is_ok());
        assert!(run("x = true || undefined").is_ok());

        let error = run(r#"x = if "yes" { 1 }"#).err().unwrap();
        assert_eq!(error.to_string(), "the condition of an if must be bool, got str");
        assert_eq!(run("x = 1 && true").err().unwrap().to_string(), "&& must be bool, got int");
        assert!(matches!(
            run(r#"x = 1 < "2""#).err().unwrap().variant,
            RuntimeErrorType::InvalidBinaryOperands { .. }
        ));

        // what a block defines stays in it
        let error = run("if true { inner = 1 }\ny = inner").err().unwrap();
        assert!(matches!(error.variant, RuntimeErrorType::UndefinedVariable { .. }));
    }

    #[test]
    fn environment_variables_with_a_default() {
        let runtime = run(r#"
            home = env("HOME")
            missing = env("BUILD_SCRIPT_SURELY_NOT_SET", default = "fallback")
            has = has_env("HOME") && !has_env("BUILD_SCRIPT_SURELY_NOT_SET")
        "#)
        .unwrap();

        assert_eq!(string(&runtime, "home"), std::env::var("HOME").unwrap());
        assert_eq!(string(&runtime, "missing"), "fallback");
        assert_eq!(string(&runtime, "has"), "true");

        let error = run(r#"x = env("BUILD_SCRIPT_SURELY_NOT_SET")"#).err().unwrap();
        assert!(error.to_string().contains("BUILD_SCRIPT_SURELY_NOT_SET is not set"), "{error}");
    }

    /// A JVM library that cannot be started
    fn library_output() -> ModuleOutput {
        return ModuleOutput::Jvm(JvmOutput {
            classes: None,
            libraries: vec![],
            platforms: vec![],
            android: None,
            launch: None,
            native_library_dirs: vec![],
        });
    }

    /// f(a: String, b?: Bool, c: String = "c", rest: String...) -> String
    fn with_echo() -> Runtime {
        fn echo(_: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
            let a: String = args.get("a")?;
            let b: Option<bool> = args.get("b")?;
            let c: String = args.get("c")?;
            let rest: Vec<String> = args.rest()?;

            return Ok(format!("a={a} b={b:?} c={c} rest={rest:?}").into());
        }

        let signature = FunctionParameters::new()
            .required("a", Type::String)
            .optional("b", Type::Bool)
            .with_default("c", Type::String, "c")
            .variadic("rest", Type::String)
            .returns(Type::String);

        let mut runtime = Runtime::new(".");
        runtime.define("f", builtin("f", signature, echo));

        return runtime;
    }

    #[test]
    fn arguments_bind_positionally_by_name_and_by_default() {
        let runtime = run_in(
            with_echo(),
            r#"
            a = "outer"
            positional = f("x", true, "y", "1", "2")
            defaults = f("x")
            named = f(c = "z", a = a)
            skipped = f("x", c = "z")
            rest = f("x", rest = list("1", "2"))
            in_place = f(a = "x", b = true, "y")
        "#,
        )
        .unwrap();

        assert_eq!(
            string(&runtime, "positional"),
            r#"a=x b=Some(true) c=y rest=["1", "2"]"#
        );
        assert_eq!(string(&runtime, "defaults"), "a=x b=None c=c rest=[]");
        assert_eq!(string(&runtime, "named"), "a=outer b=None c=z rest=[]");
        assert_eq!(string(&runtime, "skipped"), "a=x b=None c=z rest=[]");
        assert_eq!(
            string(&runtime, "rest"),
            r#"a=x b=None c=c rest=["1", "2"]"#
        );
        // named arguments at their own position may be followed by positional ones
        assert_eq!(string(&runtime, "in_place"), "a=x b=Some(true) c=y rest=[]");

        // a named argument is not an assignment
        assert_eq!(string(&runtime, "a"), "outer");
    }

    #[test]
    fn invalid_arguments_are_reported() {
        let error = |source: &str| run_in(with_echo(), source).err().expect(source).variant;

        assert!(
            matches!(error("f()"), RuntimeErrorType::MissingArgument { function, param } if function == "f" && param == "a")
        );
        assert!(
            matches!(error("f(\"x\", d = \"?\")"), RuntimeErrorType::UnknownParameter { param, .. } if param == "d")
        );
        assert!(
            matches!(error("f(\"x\", a = \"y\")"), RuntimeErrorType::DuplicateArgument { param, .. } if param == "a")
        );
        assert!(
            matches!(error("f(\"x\", true, \"c\", \"1\", rest = list())"), RuntimeErrorType::DuplicateArgument { param, .. } if param == "rest")
        );
        assert!(
            matches!(error("f(c = \"x\", true)"), RuntimeErrorType::PositionalAfterNamed { function } if function == "f")
        );
        assert!(
            matches!(error("type(\"x\", \"y\")"), RuntimeErrorType::TooManyArguments { function, max: 1, got: 2 } if function == "type")
        );

        let RuntimeErrorType::TypeMismatch {
            param,
            expected,
            got,
            ..
        } = error("f(\"x\", \"no bool\")")
        else {
            panic!()
        };

        assert_eq!(
            (param.as_str(), expected.to_string(), got.to_string()),
            ("b", "bool".to_string(), "str".to_string())
        );

        let RuntimeErrorType::TypeMismatch {
            param,
            expected,
            got,
            ..
        } = error("f(\"x\", true, \"y\", list(\"1\"))")
        else {
            panic!()
        };

        assert_eq!(
            (param.as_str(), expected.to_string(), got.to_string()),
            ("rest", "str".to_string(), "list<str>".to_string())
        );

        let RuntimeErrorType::TypeMismatch { expected, .. } = error("f(\"x\", rest = list(true))")
        else {
            panic!()
        };

        assert_eq!(expected.to_string(), "list<str>");
    }

    #[test]
    fn builtins_must_return_their_declared_type() {
        let mut runtime = Runtime::new(".");
        let signature = FunctionParameters::new().returns(Type::String);
        runtime.define(
            "bad",
            builtin("bad", signature, |_, _| Ok(Value::Bool(true))),
        );

        let RuntimeErrorType::BuiltinFailed { message, .. } =
            run_in(runtime, "bad()").err().unwrap().variant
        else {
            panic!()
        };

        assert!(message.contains("returned bool, declared str"), "{message}");
    }

    #[test]
    fn types_describe_values_and_functions() {
        let runtime = run(r#"
            use kotlin
            strings = type(list("a", "b"))
            empty = type(list())
            mixed = type(list("a", true))
            nested = type(list(list("a")))
            printer = type(print)
            builder = type(build_kotlin)
        "#)
        .unwrap();

        assert_eq!(string(&runtime, "strings"), "list<str>");
        assert_eq!(string(&runtime, "empty"), "list<any>");
        assert_eq!(string(&runtime, "mixed"), "list<any>");
        assert_eq!(string(&runtime, "nested"), "list<list<str>>");
        assert_eq!(string(&runtime, "printer"), "fn(values: any...) -> unit");
        assert_eq!(
            string(&runtime, "builder"),
            r#"fn(name: str, dependencies: list<str | module>, repositories: list<str>, main_class: str?, fat_jar: bool = false, relocate: list<str> = [], compile_only: list<str | module> = [], plugins: list<str> = [], plugin_options: list<str> = [], kotlin_args: list<str> = [], annotation_processors: list<str | module> = [], ksp: list<str | module> = [], ksp_options: list<str> = [], ksp_version: str = "2.3.12", kotlin_version: str = "2.4.21", jdk: str = "21", directory: str = ".", source_dirs: list<str> = [src/main/kotlin, src/main/java], generated_sources: list<str> = [], resource_dirs: list<str> = [src/main/resources], resource_files: list<str> = [], resource_texts: list<str> = [], resource_tokens: list<str> = [], test_dependencies: list<str | module> = [], test_source_dirs: list<str> = [src/test/kotlin, src/test/java], test_annotation_processors: list<str | module> = [], test_ksp: list<str | module> = [], test_main_class: str?) -> module"#,
        );

        // function types compare by shape, parameter names do not matter
        let shape = |name: &str| {
            Type::Function(Arc::new(
                FunctionParameters::new()
                    .required(name, Type::String)
                    .returns(Type::Unit),
            ))
        };

        let optional = Type::Function(Arc::new(
            FunctionParameters::new()
                .optional("x", Type::String)
                .returns(Type::Unit),
        ));

        assert_eq!(shape("x"), shape("y"));
        assert_ne!(shape("x"), optional);
        assert!(Type::list(Type::String).accepts(&Value::List(vec![])));
        assert!(!Type::list(Type::String).accepts(&Value::List(vec![Value::Bool(true)])));
    }

    #[test]
    fn use_kotlin_loads_build_functions() {
        let runtime = run("use kotlin\nrepos = list(maven_central(), google(),)").unwrap();

        for function in [
            "build_kotlin",
            "jdk",
            "maven_local",
            "maven",
            "platform",
            "generate_sources",
        ] {
            assert!(
                runtime.scope.lock().unwrap().get(function).is_some(),
                "{function} is missing"
            );
        }

        assert!(matches!(
            Some(variable(&runtime, "build_kotlin")),
            Some(Value::Builtin {
                name: "build_kotlin",
                ..
            })
        ));
        assert_eq!(
            variable(&runtime, "repos").to_string(),
            format!("[{}, {}]", maven::MAVEN_CENTRAL, maven::GOOGLE)
        );
    }

    #[test]
    fn build_functions_need_use() {
        let error = run("build_kotlin(\"app\", list(), list())").err().unwrap();
        assert!(matches!(
            error.variant,
            RuntimeErrorType::UndefinedVariable { .. }
        ));

        // scripts with the names from before learn the new one
        let error = run("use kotlin\nbuildKotlin(\"app\", list(), list())")
            .err()
            .unwrap();

        assert_eq!(
            error.to_string(),
            "variable 'buildKotlin' is not defined, the function is called build_kotlin"
        );

        let error =
            run("use kotlin\nbuild_kotlin(\"app\", list(), list(), mainClass = \"MainKt\")")
                .err()
                .unwrap();

        assert_eq!(
            error.to_string(),
            "build_kotlin has no parameter 'mainClass', it is called main_class"
        );

        let error = run("use cobol").err().unwrap();
        assert!(matches!(
            error.variant,
            RuntimeErrorType::UnknownPlatform { .. }
        ));
    }

    #[test]
    fn build_kotlin_validates_arguments() {
        let error = run("use kotlin\nbuild_kotlin(\"app\")").err().unwrap();
        assert!(
            matches!(&error.variant, RuntimeErrorType::MissingArgument { function, param } if function == "build_kotlin" && param == "dependencies")
        );

        let error = run("use kotlin\nbuild_kotlin(\"app\", \"broken\", list(maven_central()))")
            .err()
            .unwrap();

        assert_eq!(
            error.to_string(),
            "build_kotlin: argument 'dependencies' must be list<str | module>, got str"
        );
        assert_eq!((error.span.start, error.span.end), (31, 39));

        let error = run("use kotlin\nbuild_kotlin(\"app\", list(\"broken\"), list(maven_central()), jdk = \"21\")").err().unwrap();
        let RuntimeErrorType::BuiltinFailed { name, message } = error.variant else {
            panic!()
        };

        assert_eq!(name, "build_kotlin");
        assert!(message.contains("'broken'"), "{message}");
    }

    #[test]
    fn functions_are_values_and_return_their_last_expression() {
        let runtime = run(r#"
            my_fn = fn (param1: str) -> str { param1 }
            same = my_fn("x")

            // defaults, named arguments and local variables
            greet = fn (name: str, greeting: str = "Hello") -> str {
                text = greeting + ", " + name
                text
            }
            hello = greet("Ada")
            hi = greet(greeting = "Hi", name = "Bob")

            // closures see the variables where they were defined
            prefix = "lib-"
            named = fn (name: str) -> str { prefix + name }

            // functions as arguments and as results
            apply = fn (f: fn(str) -> str, value: str) -> str { f(value) }
            applied = apply(named, "core")
            adder = fn (a: int) -> fn(int) -> int { fn (b: int) -> int { a + b } }
            five = adder(2)(3)

            // builtins inside, no declared type, nothing to return
            libraries = fn (extra: list<str>) { list("a") + extra }
            all = libraries(list("b"))
            nothing = fn () -> unit { local = 1 }
            empty = nothing()

            types = type(greet)
            applier = type(apply)
        "#)
        .unwrap();

        assert_eq!(string(&runtime, "same"), "x");
        assert_eq!(string(&runtime, "hello"), "Hello, Ada");
        assert_eq!(string(&runtime, "hi"), "Hi, Bob");
        assert_eq!(string(&runtime, "applied"), "lib-core");
        assert_eq!(string(&runtime, "five"), "5");
        assert_eq!(string(&runtime, "all"), "[a, b]");
        assert_eq!(string(&runtime, "empty"), "()");
        assert_eq!(
            string(&runtime, "types"),
            r#"fn(name: str, greeting: str = "Hello") -> str"#
        );
        assert_eq!(
            string(&runtime, "applier"),
            "fn(f: fn(str) -> str, value: str) -> str"
        );
        assert_eq!(string(&runtime, "my_fn"), "<fn my_fn>");
        // what a call assigns stays in its scope
        assert!(
            runtime.scope.lock().unwrap().get("text").is_none()
                && runtime.scope.lock().unwrap().get("local").is_none()
        );
    }

    #[test]
    fn functions_check_their_types() {
        let error = |source: &str| run(source).err().expect(source).variant;

        let RuntimeErrorType::ReturnTypeMismatch {
            function,
            expected,
            got,
        } = error("bad = fn () -> str { 1 }\nbad()")
        else {
            panic!()
        };

        assert_eq!(
            (function.as_str(), expected.to_string(), got.to_string()),
            ("bad", "str".to_string(), "int".to_string())
        );

        let RuntimeErrorType::TypeMismatch {
            function, param, ..
        } = error("f = fn (count: int) -> int { count }\nf(\"x\")")
        else {
            panic!()
        };

        assert_eq!((function.as_str(), param.as_str()), ("f", "count"));

        assert!(
            matches!(error("f = fn (x: text) -> str { x }"), RuntimeErrorType::UnknownType { name } if name == "text")
        );
        assert!(
            matches!(error("f = fn (x: str, x: int) { x }"), RuntimeErrorType::DuplicateParameter { param, .. } if param == "x")
        );
        assert!(matches!(
            error("f = fn (x: int = \"one\") { x }"),
            RuntimeErrorType::TypeMismatch { .. }
        ));
        assert!(
            matches!(error("f = fn (x: str) { x }\nf()"), RuntimeErrorType::MissingArgument { function, .. } if function == "f")
        );
        assert!(
            matches!(error("again = fn () -> int { again() }\nagain()"), RuntimeErrorType::CallsTooDeep { function } if function == "again")
        );

        // a function type only fits functions of that shape
        assert!(matches!(
            error("apply = fn (f: fn(str) -> str) { f(\"x\") }\napply(fn (n: int) -> int { n })"),
            RuntimeErrorType::TypeMismatch { .. }
        ));
    }

    #[test]
    fn lists_concatenate_and_compare() {
        let runtime = run("a = list(\"x\") + list(\"y\")\nsame = a == list(\"x\", \"y\")").unwrap();
        assert_eq!(variable(&runtime, "a").to_string(), "[x, y]");
        assert!(matches!(variable(&runtime, "same"), Value::Bool(true)));
    }

    #[test]
    fn comments_are_ignored() {
        let runtime = run(r#"
            // a line comment
            url = "https://example.com" // after code, // in strings stays
            /* a block comment
               over two lines */
            half = 6 / 3 /* between */ + 1
            // the last line"#)
        .unwrap();

        assert_eq!(string(&runtime, "url"), "https://example.com");
        assert_eq!(string(&runtime, "half"), "3");
    }

    #[test]
    fn numbers_and_union_types() {
        let runtime = run(r#"
            sum = 1 + 2 * 3
            negative = -4 - 1
            same = sum == 7
            sum_type = type(sum)
        "#)
        .unwrap();

        assert_eq!(string(&runtime, "sum"), "7");
        assert_eq!(string(&runtime, "negative"), "-5");
        assert_eq!(string(&runtime, "same"), "true");
        assert_eq!(string(&runtime, "sum_type"), "int");

        let error = run("x = 9223372036854775807 + 1").err().unwrap();
        assert!(matches!(
            error.variant,
            RuntimeErrorType::InvalidArithmetic { .. }
        ));
        assert!(matches!(
            run("x = 1 / 0").err().unwrap().variant,
            RuntimeErrorType::InvalidArithmetic { .. }
        ));

        let dependency = Type::Union(vec![Type::String, Type::Module]);
        assert_eq!(dependency.to_string(), "str | module");
        assert!(dependency.accepts(&Value::from("a:b:1")));
        assert!(!dependency.accepts(&Value::Int(1)));

        let error = run("use kotlin\nbuild_kotlin(\"app\", list(true), list(maven_central()))")
            .err()
            .unwrap();

        let RuntimeErrorType::TypeMismatch { expected, got, .. } = error.variant else {
            panic!()
        };

        assert_eq!(
            (expected.to_string(), got.to_string()),
            ("list<str | module>".to_string(), "list<bool>".to_string())
        );
    }

    #[test]
    fn modules_list_their_dependencies_once() {
        let module = |name: &str, modules: Vec<Arc<BuiltModule>>| {
            Arc::new(BuiltModule {
                name: name.to_string(),
                directory: PathBuf::from(name),
                artifact: PathBuf::from(format!("{name}.jar")),
                modules,
                output: library_output(),
                tests: vec![],
            })
        };

        let core = module("core", vec![]);
        let data = module("data", vec![core.clone()]);
        let ui = module("ui", vec![core.clone()]);

        let names: Vec<String> = BuiltModule::transitive(&[data, ui])
            .iter()
            .map(|module| module.name.clone())
            .collect();

        assert_eq!(names, ["core", "data", "ui"]);
        assert_eq!(Value::from(core).to_string(), "core.jar");
    }

    #[test]
    fn independent_modules_build_in_parallel_after_their_dependencies() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static RUNNING: AtomicUsize = AtomicUsize::new(0);
        static MOST: AtomicUsize = AtomicUsize::new(0);
        static FINISHED: Mutex<Vec<String>> = Mutex::new(vec![]);

        fn slow(_: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
            let name: String = args.get("name")?;

            for dependency in args.modules() {
                if !FINISHED.lock().unwrap().contains(&dependency.name) {
                    return Err(format!("{} was not built before {name}", dependency.name));
                }
            }

            if name == "broken" {
                return Err("broken on purpose".to_string());
            }

            let running = RUNNING.fetch_add(1, Ordering::SeqCst) + 1;
            MOST.fetch_max(running, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(200));
            RUNNING.fetch_sub(1, Ordering::SeqCst);
            FINISHED.lock()
                .unwrap()
                .push(name.clone());

            return Ok(Value::from(Arc::new(BuiltModule {
                artifact: PathBuf::from(format!("{name}.jar")),
                name,
                directory: PathBuf::from("."),
                modules: vec![],
                output: library_output(),
                tests: vec![],
            })));
        }

        let runtime_with_slow = || {
            let signature = FunctionParameters::new()
                .required("name", Type::String)
                .with_default("dependencies", Type::list(Type::Module), Value::List(vec![]))
                .returns(Type::Module);
            let mut runtime = Runtime::new(".").with_jobs(2);
            runtime.define("slow", builtin("slow", signature, slow));

            return runtime;
        };

        let source = "a = slow(\"a\")\nb = slow(\"b\")\nc = slow(\"c\", dependencies = list(a, b))";
        run_in(runtime_with_slow(), source).unwrap();
        assert_eq!(MOST.load(Ordering::SeqCst), 2);
        assert_eq!(FINISHED.lock().unwrap().last().map(String::as_str), Some("c"));

        // a module that fails is the error, not the modules that depend on it
        let mut runtime = runtime_with_slow();
        let script = Parser::new("x = slow(\"broken\")\ny = slow(\"after\", dependencies = list(x))")
            .parse_script()
            .unwrap();
        runtime.run(&script).unwrap();
        let errors = runtime.finish()
            .err()
            .unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].to_string(), "slow: broken on purpose");
    }

    #[test]
    fn built_modules_are_remembered_for_run() {
        fn library(_: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
            let name: String = args.get("name")?;
            return Ok(Value::from(Arc::new(BuiltModule {
                name: name.clone(),
                directory: PathBuf::from("."),
                artifact: PathBuf::from(format!("{name}.jar")),
                modules: vec![],
                output: library_output(),
                tests: vec![],
            })));
        }

        let mut runtime = Runtime::new(".");

        let signature = FunctionParameters::new()
            .required("name", Type::String)
            .returns(Type::Module);

        runtime.define("library", builtin("library", signature, library));
        let runtime = run_in(runtime, "core = library(\"core\")\nlibrary(\"app\")").unwrap();

        let built = runtime.finish().unwrap();
        let names: Vec<&str> = built.iter()
            .map(|module| module.name.as_str())
            .collect();

        assert_eq!(names, ["core", "app"]);

        let error = launch(&built[0], &[]).unwrap_err();
        assert!(error.contains("core cannot be run"), "{error}");
    }
}
