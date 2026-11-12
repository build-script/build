// Runtime errors: what can go wrong while evaluating a build script
use std::fmt;

use crate::language::lexer::Span;
use crate::language::parser::{BinaryOperator, UnaryOperator};

use super::{Type, Value};
use super::{builtin_names, platform_names};

#[derive(Debug)]
pub enum RuntimeErrorType {
    UndefinedVariable {
        name: String,
    },
    InvalidUnaryOperand {
        operator: UnaryOperator,
        operand: Type,
    },
    InvalidBinaryOperands {
        operator: BinaryOperator,
        left: Type,
        right: Type,
    },
    NotCallable {
        value_type: Type,
    },
    /// overflow or division by zero
    InvalidArithmetic {
        operator: String,
    },
    TooManyArguments {
        function: String,
        max: usize,
        got: usize,
    },
    MissingArgument {
        function: String,
        param: String,
    },
    UnknownParameter {
        function: String,
        param: String,
        /// the parameter's name in snake case, when the function has that one
        renamed: Option<String>,
    },
    DuplicateArgument {
        function: String,
        param: String,
    },
    PositionalAfterNamed {
        function: String,
    },
    TypeMismatch {
        function: String,
        param: String,
        expected: Type,
        /// the type of the value, or the string itself where the type has variants
        got: String,
    },
    UnknownPlatform {
        name: String,
    },
    BuiltinFailed {
        name: String,
        message: String,
    },
    /// a function of the script returned a value of another type than it declares
    ReturnTypeMismatch {
        function: String,
        expected: Type,
        got: String,
    },
    UnknownType {
        name: String,
    },
    /// a variant written twice in a union: "auto" | "local" | "auto"
    DuplicateVariant {
        variant: Type,
        union: Type,
    },
    /// a variant beside str or any, which accept it anyway: "auto" | str
    RedundantVariant {
        variant: Type,
        wider: Type,
        union: Type,
    },
    DuplicateParameter {
        function: String,
        param: String,
    },
    /// endless recursion, most likely
    CallsTooDeep {
        function: String,
    },
    /// of an if, &&, ||
    NotBool {
        what: &'static str,
        got: Type,
    },
    PluginNotFound {
        plugin: String,
        path: String,
    },
    /// use "owner/repository", until repositories are supported
    PluginRepository {
        plugin: String,
    },
    /// a plugin uses a plugin that uses it
    PluginCycle {
        plugin: String,
    },
    /// what a plugin defines is defined already
    PluginNameTaken {
        plugin: String,
        name: String,
    },
    /// a plugin does not parse, the span is in its file
    PluginParse {
        message: String,
    },
    /// a module built while a plugin is loaded
    ModuleInPlugin {
        function: String,
    },
}

#[derive(Debug)]
pub struct RuntimeError {
    pub variant: RuntimeErrorType,
    pub span: Span,
    /// for an error in a plugin: the function the script called and where, the script's line is shown first
    pub called_from: Option<(String, Span)>,
}

impl RuntimeError {
    pub fn new(variant: RuntimeErrorType, span: Span) -> Self {
        return RuntimeError {
            variant,
            span,
            called_from: None,
        };
    }

    pub fn invalid_binary_operands(
        operator: &BinaryOperator,
        left: &Value,
        right: &Value,
        span: Span,
    ) -> Self {
        return Self::new(
            RuntimeErrorType::InvalidBinaryOperands {
                operator: operator.clone(),
                left: left.type_of(),
                right: right.type_of(),
            },
            span,
        );
    }

    pub fn type_mismatch(
        function: &str,
        param: &str,
        expected: &Type,
        value: &Value,
        span: Span,
    ) -> Self {
        return Self::new(
            RuntimeErrorType::TypeMismatch {
                function: function.to_string(),
                param: param.to_string(),
                expected: expected.clone(),
                got: expected.describe_mismatch(value),
            },
            span,
        );
    }

    /// Of + - * / and the negation of numbers
    pub fn invalid_arithmetic(operator: &impl fmt::Display, span: Span) -> Self {
        return Self::new(RuntimeErrorType::InvalidArithmetic { operator: operator.to_string() }, span);
    }

    pub fn builtin_failed(name: &str, message: String, span: Span) -> Self {
        return Self::new(
            RuntimeErrorType::BuiltinFailed {
                name: name.to_string(),
                message,
            },
            span,
        );
    }
}

/// The message, where it happened is in `span`
impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.variant {
            RuntimeErrorType::UndefinedVariable { name } => {
                // the builtins were called like buildKotlin before
                let renamed = snake_case(name);
                if renamed != *name && builtin_names().contains(&renamed.as_str()) {
                    return write!(
                        f,
                        "variable '{name}' is not defined, the function is called {renamed}"
                    );
                }

                return write!(f, "variable '{name}' is not defined");
            }
            RuntimeErrorType::InvalidUnaryOperand { operator, operand } => {
                return write!(f, "operator {operator} cannot be applied to {operand}");
            }
            RuntimeErrorType::InvalidBinaryOperands {
                operator,
                left,
                right,
            } => {
                return write!(f, "operator {operator} cannot be applied to {left} and {right}");
            }
            RuntimeErrorType::NotCallable { value_type } => {
                return write!(f, "{value_type} is not a function");
            }
            RuntimeErrorType::InvalidArithmetic { operator } => {
                return write!(f, "operator {operator} overflows or divides by zero");
            }
            RuntimeErrorType::TooManyArguments { function, max, got } => {
                return write!(f, "{function} takes at most {max} argument(s), got {got}");
            }
            RuntimeErrorType::MissingArgument { function, param } => {
                return write!(f, "{function}: missing argument '{param}'");
            }
            RuntimeErrorType::UnknownParameter {
                function,
                param,
                renamed: Some(renamed),
            } => {
                return write!(
                    f,
                    "{function} has no parameter '{param}', it is called {renamed}"
                );
            }
            RuntimeErrorType::UnknownParameter {
                function,
                param,
                renamed: None,
            } => return write!(f, "{function} has no parameter '{param}'"),
            RuntimeErrorType::DuplicateArgument { function, param } => {
                return write!(f, "{function}: argument '{param}' is passed more than once");
            }
            RuntimeErrorType::PositionalAfterNamed { function } => {
                return write!(
                    f,
                    "{function}: a positional argument may only follow named arguments that stand at the position of their parameter"
                );
            }
            RuntimeErrorType::TypeMismatch {
                function,
                param,
                expected,
                got,
            } => {
                return write!(
                    f,
                    "{function}: argument '{param}' must be {expected}, got {got}"
                );
            }
            RuntimeErrorType::UnknownPlatform { name } => {
                return write!(
                    f,
                    "unknown platform '{name}' (available: {})",
                    platform_names().join(", ")
                );
            }
            RuntimeErrorType::BuiltinFailed { name, message } => {
                return write!(f, "{name}: {message}");
            }
            RuntimeErrorType::ReturnTypeMismatch {
                function,
                expected,
                got,
            } => return write!(f, "{function} must return {expected}, got {got}"),
            RuntimeErrorType::UnknownType { name } => {
                return write!(
                    f,
                    "unknown type '{name}' (types: str, int, bool, module, any, unit, list<...>, fn(...) -> ..., a | b, \"a variant\")"
                );
            }
            RuntimeErrorType::DuplicateVariant { variant, union } => {
                return write!(f, "{variant} is listed twice in {union}");
            }
            RuntimeErrorType::RedundantVariant {
                variant,
                wider,
                union,
            } => {
                return write!(f, "{wider} already accepts {variant}, leave the variant out of {union}");
            }
            RuntimeErrorType::DuplicateParameter { function, param } => {
                return write!(f, "{function}: parameter '{param}' is declared twice");
            }
            RuntimeErrorType::NotBool { what, got } => {
                return write!(f, "{what} must be bool, got {got}");
            }
            RuntimeErrorType::CallsTooDeep { function } => {
                return write!(
                    f,
                    "{function}: calls nest too deeply, does it call itself without end?"
                );
            }
            RuntimeErrorType::PluginNotFound { plugin, path } => {
                return write!(f, "no plugin {plugin}: {path} does not exist");
            }
            RuntimeErrorType::PluginRepository { plugin } => {
                return write!(f, "{plugin} names a repository, plugins come from files like \"./plugins/go.build\" for now");
            }
            RuntimeErrorType::PluginCycle { plugin } => {
                return write!(f, "{plugin} uses itself through the plugins it uses");
            }
            RuntimeErrorType::PluginNameTaken { plugin, name } => {
                return write!(f, "{plugin} defines {name}, which is defined already; a plugin keeps names starting with _ to itself");
            }
            RuntimeErrorType::PluginParse { message } => return write!(f, "{message}"),
            RuntimeErrorType::ModuleInPlugin { function } => {
                return write!(f, "{function}: plugins define functions, their modules come from the scripts that call them");
            }
        }
    }
}

/// buildAndroidLibrary -> build_android_library, how the builtins and their parameters were renamed
pub fn snake_case(name: &str) -> String {
    let mut snake_case = String::new();

    for char in name.chars() {
        if char.is_ascii_uppercase() {
            snake_case.push('_');
        }

        snake_case.push(char.to_ascii_lowercase());
    }

    return snake_case;
}
