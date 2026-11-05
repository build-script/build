// Functions: signatures with typed parameters, how arguments bind to them, builtins and functions of the script
use std::fmt;
use std::sync::Arc;

use crate::language::lexer::Span;
use crate::language::parser::Expression;

use super::error::{RuntimeError, RuntimeErrorType, snake_case};
use super::{BuiltModule, PendingModule, Type, Value};
use super::{BuildScriptContext, SharedScope};

pub type BuiltinFunction = fn(&BuildScriptContext, &Arguments) -> Result<Value, String>;

/// Named, typed parameters of a function, optionally followed by a variadic one.
/// Built like `FunctionParameters::new().required("url", Type::String).returns(Type::String)`.
#[derive(Debug, Clone)]
pub struct FunctionParameters {
    /// in the order positional arguments bind to them
    pub fixed: Vec<FunctionParameter>,
    pub variadic: Option<VariadicParameter>,
}

#[derive(Debug, Clone)]
pub struct FunctionParameter {
    pub name: String,
    pub param_type: Type,
    pub default: ParameterDefault,
}

#[derive(Debug, Clone)]
pub enum ParameterDefault {
    Required,
    /// `name: str?`, read as None
    Optional,
    /// `name: str = "Maggus"`
    Value(Value),
}

/// Collects all remaining positional arguments: `values: any...` of print
#[derive(Debug, Clone)]
pub struct VariadicParameter {
    pub name: String,
    pub param_type: Type,
}

/// The type of a function: its parameters and what it returns
#[derive(Debug, Clone)]
pub struct FunctionSignature {
    pub parameters: FunctionParameters,
    pub returns: Type,
}

/// A function the script defines: `fn (name: str) -> str { body }`
pub struct UserFunction {
    /// the variable it was assigned to when it was defined, for messages
    pub name: String,
    pub signature: Arc<FunctionSignature>,
    /// the last expression is the value of a call
    pub(super) body: Vec<Expression>,
    /// where it was defined, the body sees the variables there
    pub(super) scope: SharedScope,
}

pub enum CallArgument {
    Positional(Value, Span),
    /// `name = value`
    Named(String, Value, Span),
}

/// The arguments of a call, bound to the parameters, for builtins to read as Rust types
#[derive(Debug, Hash)]
pub struct Arguments {
    pub function: String,
    /// one entry per declared parameter, None = omitted optional argument
    pub values: Vec<(String, Option<Value>)>,
    /// arguments of the variadic parameter
    pub rest: Vec<Value>,
}

/// Rust types that builtins can read their arguments as
pub trait FromValue: Sized {
    fn from_value(value: &Value) -> Option<Self>;

    /// Reading an omitted optional argument, only `Option<T>` supports it
    fn from_absent() -> Option<Self> {
        return None;
    }
}

pub fn builtin(
    name: &'static str,
    signature: FunctionSignature,
    function: BuiltinFunction,
) -> (&'static str, Value) {
    let value = Value::Builtin {
        name,
        signature: Arc::new(signature),
        function,
    };

    return (name, value);
}

/// Like Kotlin: positional arguments in order, named ones in any order, defaults for
/// the rest. Positional arguments may follow named ones that stand at the position
/// of their parameter. Every argument is checked against the type of its parameter.
pub fn bind(
    function: &str,
    parameters: &FunctionParameters,
    arguments: Vec<CallArgument>,
    span: Span,
) -> Result<Arguments, RuntimeError> {
    let mut slots: Vec<Option<Value>> = vec![None; parameters.fixed.len()];
    let mut rest = vec![];
    let mut rest_named = false;
    let mut named_out_of_place = false;

    let positional_count = arguments.iter()
        .filter(|arg| matches!(arg, CallArgument::Positional(..)))
        .count();

    let error = |variant| RuntimeError::new(variant, span);

    // the value, when the parameter takes it
    let checked = |parameter: &str, expected: &Type, value: Value, value_span: Span| {
        if !expected.accepts(&value) {
            return Err(RuntimeError::type_mismatch(function, parameter, expected, &value, value_span));
        }

        return Ok(value);
    };

    for (index, argument) in arguments.into_iter().enumerate() {
        match argument {
            CallArgument::Positional(value, arg_span) => {
                if named_out_of_place {
                    return Err(RuntimeError::new(
                        RuntimeErrorType::PositionalAfterNamed {
                            function: function.to_string(),
                        },
                        arg_span,
                    ));
                }

                if let Some(param) = parameters.fixed.get(index) {
                    slots[index] = Some(checked(&param.name, &param.param_type, value, arg_span)?);
                } else if let Some(variadic) = &parameters.variadic {
                    rest.push(checked(&variadic.name, &variadic.param_type, value, arg_span)?);
                } else {
                    return Err(error(RuntimeErrorType::TooManyArguments {
                        function: function.to_string(),
                        max: parameters.fixed.len(),
                        got: positional_count,
                    }));
                }
            }

            CallArgument::Named(name, value, arg_span) => {
                let duplicate = || {
                    RuntimeError::new(
                        RuntimeErrorType::DuplicateArgument {
                            function: function.to_string(),
                            param: name.clone(),
                        },
                        arg_span,
                    )
                };

                let position = parameters.fixed.iter().position(|param| param.name == name);

                if let Some(param_index) = position {
                    named_out_of_place |= param_index != index;
                    let param = &parameters.fixed[param_index];

                    if slots[param_index].is_some() {
                        return Err(duplicate());
                    }

                    slots[param_index] = Some(checked(&param.name, &param.param_type, value, arg_span)?);
                    continue;
                }

                // the variadic parameter takes a list when passed by name
                named_out_of_place = true;
                let Some(variadic) = parameters.variadic.as_ref()
                    .filter(|variadic| variadic.name == name)
                else {
                    let renamed = Some(snake_case(&name)).filter(|renamed| {
                        *renamed != name
                            && parameters.fixed.iter().any(|param| param.name == *renamed)
                    });
                    return Err(RuntimeError::new(
                        RuntimeErrorType::UnknownParameter {
                            function: function.to_string(),
                            param: name,
                            renamed,
                        },
                        arg_span,
                    ));
                };

                if rest_named || !rest.is_empty() {
                    return Err(duplicate());
                }

                // a list type takes only lists
                if let Value::List(items) = checked(&variadic.name, &Type::list(variadic.param_type.clone()), value, arg_span)? {
                    rest = items;
                }

                rest_named = true;
            }
        }
    }

    let mut values = Vec::with_capacity(parameters.fixed.len());

    for (param, slot) in parameters.fixed.iter().zip(slots) {
        let value = match (slot, &param.default) {
            (Some(value), _) => Some(value),
            (None, ParameterDefault::Value(default)) => Some(default.clone()),
            (None, ParameterDefault::Optional) => None,
            (None, ParameterDefault::Required) => {
                return Err(error(RuntimeErrorType::MissingArgument {
                    function: function.to_string(),
                    param: param.name.clone(),
                }));
            }
        };

        values.push((param.name.clone(), value));
    }

    return Ok(Arguments {
        function: function.to_string(),
        values,
        rest,
    });
}

impl FunctionParameters {
    pub fn new() -> Self {
        return FunctionParameters {
            fixed: vec![],
            variadic: None,
        };
    }

    pub fn required(self, name: &str, param_type: Type) -> Self {
        return self.param(name, param_type, ParameterDefault::Required);
    }

    pub fn optional(self, name: &str, param_type: Type) -> Self {
        return self.param(name, param_type, ParameterDefault::Optional);
    }

    pub fn with_default(self, name: &str, param_type: Type, value: impl Into<Value>) -> Self {
        let value = value.into();
        assert!(
            param_type.accepts(&value),
            "default value {} of '{name}' is not a {param_type}",
            value.literal()
        );

        return self.param(name, param_type, ParameterDefault::Value(value));
    }

    /// A list of strings, empty when the call leaves it out
    pub fn strings(self, name: &str) -> Self {
        return self.with_default(name, Type::list(Type::String), Value::string_list(&[]));
    }

    /// A list of strings, `default` when the call leaves it out
    pub fn strings_or(self, name: &str, default: &[&str]) -> Self {
        return self.with_default(name, Type::list(Type::String), Value::string_list(default));
    }

    pub fn variadic(mut self, name: &str, param_type: Type) -> Self {
        self.assert_new_name(name);
        self.variadic = Some(VariadicParameter {
            name: name.to_string(),
            param_type,
        });

        return self;
    }

    pub fn returns(self, returns: Type) -> FunctionSignature {
        return FunctionSignature {
            parameters: self,
            returns,
        };
    }

    fn param(mut self, name: &str, param_type: Type, default: ParameterDefault) -> Self {
        self.assert_new_name(name);
        assert!(
            self.variadic.is_none(),
            "'{name}' comes after the variadic parameter"
        );

        self.fixed.push(FunctionParameter {
            name: name.to_string(),
            param_type,
            default,
        });

        return self;
    }

    fn assert_new_name(&self, name: &str) {
        let taken = self.fixed.iter().any(|param| param.name == name)
            || self.variadic.as_ref()
                .is_some_and(|variadic| variadic.name == name);

        assert!(!taken, "parameter '{name}' is declared twice");
    }
}

impl FunctionSignature {
    pub fn same_shape(&self, other: &FunctionSignature) -> bool {
        let required =
            |param: &FunctionParameter| matches!(param.default, ParameterDefault::Required);

        let same_params = self.parameters.fixed.len() == other.parameters.fixed.len()
            && self.parameters.fixed.iter()
                .zip(&other.parameters.fixed)
                .all(|(left, right)| {
                    left.param_type == right.param_type && required(left) == required(right)
                });

        let same_variadic = match (&self.parameters.variadic, &other.parameters.variadic) {
            (Some(left), Some(right)) => left.param_type == right.param_type,
            (None, None) => true,
            _ => false,
        };

        return same_params && same_variadic && self.returns == other.returns;
    }
}

impl Arguments {
    pub fn get<T: FromValue>(&self, name: &str) -> Result<T, String> {
        let Some((_, value)) = self.values.iter().find(|(param, _)| param == name) else {
            return Err(format!(
                "internal error: {} has no parameter '{name}'",
                self.function
            ));
        };

        let converted = match value {
            Some(value) => T::from_value(value),
            None => T::from_absent(),
        };

        return converted.ok_or_else(|| self.conversion_error::<T>(name));
    }

    /// "name=value" entries, like manifest_placeholders: split at the first '=', the name trimmed
    pub fn assignments(&self, name: &str) -> Result<Vec<(String, String)>, String> {
        return self.get::<Vec<String>>(name)?
            .iter()
            .map(|entry| {
                entry.split_once('=')
                    .map(|(key, value)| (key.trim().to_string(), value.to_string()))
                    .ok_or_else(|| format!("{name} entry '{entry}' must look like 'name=value'"))
            })
            .collect();
    }

    /// The modules among all arguments, also inside lists, once they are built
    pub fn modules(&self) -> Vec<Arc<BuiltModule>> {
        return self.pending_modules()
            .iter()
            .filter_map(|module| module.wait().ok())
            .collect();
    }

    /// The modules among all arguments, also inside lists, maybe still building
    pub fn pending_modules(&self) -> Vec<Arc<PendingModule>> {
        fn collect(value: &Value, modules: &mut Vec<Arc<PendingModule>>) {
            match value {
                Value::Module(module) => modules.push(module.clone()),
                Value::List(items) => items.iter().for_each(|item| collect(item, modules)),
                _ => {}
            }
        }

        let mut modules = vec![];

        for value in self.values.iter()
            .filter_map(|(_, value)| value.as_ref())
            .chain(&self.rest)
        {
            collect(value, &mut modules);
        }

        return modules;
    }

    /// Arguments of the variadic parameter
    pub fn rest<T: FromValue>(&self) -> Result<Vec<T>, String> {
        return self.rest.iter()
            .map(|value| T::from_value(value).ok_or_else(|| self.conversion_error::<T>("...")))
            .collect();
    }

    /// Only when the declaration and the implementation of a builtin disagree
    fn conversion_error<T>(&self, name: &str) -> String {
        return format!(
            "internal error: argument '{name}' of {} cannot be read as {}",
            self.function,
            std::any::type_name::<T>()
        );
    }
}

impl FromValue for Value {
    fn from_value(value: &Value) -> Option<Self> {
        return Some(value.clone());
    }
}

impl FromValue for String {
    fn from_value(value: &Value) -> Option<Self> {
        let Value::String(str) = value else {
            return None;
        };
        return Some(str.clone());
    }
}

impl FromValue for i64 {
    fn from_value(value: &Value) -> Option<Self> {
        let Value::Int(int) = value else { return None };
        return Some(*int);
    }
}

impl FromValue for bool {
    fn from_value(value: &Value) -> Option<Self> {
        let Value::Bool(bool) = value else {
            return None;
        };
        return Some(*bool);
    }
}

impl FromValue for Arc<BuiltModule> {
    fn from_value(value: &Value) -> Option<Self> {
        let Value::Module(module) = value else {
            return None;
        };
        return module.wait().ok();
    }
}

impl<T: FromValue> FromValue for Vec<T> {
    fn from_value(value: &Value) -> Option<Self> {
        let Value::List(items) = value else {
            return None;
        };
        return items.iter()
            .map(T::from_value)
            .collect();
    }
}

impl<T: FromValue> FromValue for Option<T> {
    fn from_value(value: &Value) -> Option<Self> {
        return T::from_value(value).map(Some);
    }

    fn from_absent() -> Option<Self> {
        return Some(None);
    }
}

impl fmt::Display for FunctionParameter {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // parameters of function types have no names: fn(str) -> str
        if !self.name.is_empty() {
            write!(f, "{}: ", self.name)?;
        }

        match &self.default {
            ParameterDefault::Required => return write!(f, "{}", self.param_type),
            ParameterDefault::Optional => return write!(f, "{}?", self.param_type),
            ParameterDefault::Value(value) => {
                return write!(f, "{} = {}", self.param_type, value.literal());
            }
        }
    }
}

impl fmt::Display for FunctionParameters {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut parts: Vec<String> = self.fixed.iter()
            .map(|param| param.to_string())
            .collect();

        if let Some(variadic) = &self.variadic {
            parts.push(format!("{}: {}...", variadic.name, variadic.param_type));
        }

        return write!(f, "{}", parts.join(", "));
    }
}

impl fmt::Display for FunctionSignature {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        return write!(f, "fn({}) -> {}", self.parameters, self.returns);
    }
}

/// Only the name: the scope may contain the function itself
impl fmt::Debug for UserFunction {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        return write!(f, "UserFunction({})", self.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assignments_split_at_the_first_equals_sign() {
        let args = |entries: &[&str]| Arguments {
            function: "f".to_string(),
            values: vec![("pairs".to_string(), Some(Value::string_list(entries)))],
            rest: vec![],
        };

        assert_eq!(
            args(&[" a.txt = x=y "]).assignments("pairs").unwrap(),
            [("a.txt".to_string(), " x=y ".to_string())]
        );
        assert_eq!(
            args(&["version"]).assignments("pairs").unwrap_err(),
            "pairs entry 'version' must look like 'name=value'"
        );
    }
}
