// Values: what expressions of a build script evaluate to and their types
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use super::functions::{BuiltinFunction, FunctionSignature, UserFunction};

use super::*;

#[derive(Debug, Clone)]
pub enum Value {
    String(String),
    Int(i64),
    Bool(bool),
    List(Vec<Value>),
    Module(Arc<PendingModule>),
    Unit,
    Builtin {
        name: &'static str,
        signature: Arc<FunctionSignature>,
        function: BuiltinFunction,
    },
    /// defined by the script with fn (...) { ... }
    Function(Arc<UserFunction>),
}

/// Types of values, checked whenever arguments are bound to parameters. They
/// are written like in scripts: str, list<str>, fn(str) -> bool
#[derive(Debug, Clone)]
pub enum Type {
    Any,
    Unit,
    Bool,
    Int,
    String,
    Module,
    List(Box<Type>),
    Function(Arc<FunctionSignature>),
    /// one of several types: str | module
    Union(Vec<Type>),
}

impl Value {
    pub fn type_of(&self) -> Type {
        match self {
            Value::String(_) => return Type::String,
            Value::Int(_) => return Type::Int,
            Value::Bool(_) => return Type::Bool,
            Value::Module(_) => return Type::Module,
            Value::Unit => return Type::Unit,
            Value::List(items) => {
                let element = match items.split_first() {
                    Some((first, rest)) => {
                        let first = first.type_of();
                        if rest.iter().all(|item| item.type_of() == first) {
                            first
                        } else {
                            Type::Any
                        }
                    }
                    None => Type::Any,
                };
                return Type::list(element);
            }
            Value::Builtin { signature, .. } => return Type::Function(signature.clone()),
            Value::Function(function) => return Type::Function(function.signature.clone()),
        }
    }

    pub fn string_list(values: &[&str]) -> Value {
        return Value::List(values.iter().map(|value| Value::from(*value)).collect());
    }

    /// How the value is written in a script: strings with quotes
    pub fn literal(&self) -> String {
        match self {
            Value::String(str) => {
                let escaped = str.replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', "\\n")
                    .replace('\t', "\\t");
                return format!("\"{escaped}\"");
            }
            other => return other.to_string(),
        }
    }
}

impl Type {
    pub fn list(element: Type) -> Self {
        return Type::List(Box::new(element));
    }

    pub fn accepts(&self, value: &Value) -> bool {
        match (self, value) {
            (Type::Any, _) => return true,
            (Type::Unit, Value::Unit) => return true,
            (Type::Bool, Value::Bool(_)) => return true,
            (Type::Int, Value::Int(_)) => return true,
            (Type::String, Value::String(_)) => return true,
            (Type::Module, Value::Module(_)) => return true,
            (Type::List(element), Value::List(items)) => {
                return items.iter().all(|item| element.accepts(item));
            }
            (Type::Union(types), _) => {
                return types.iter().any(|member_type| member_type.accepts(value));
            }
            (Type::Function(_), Value::Builtin { .. } | Value::Function(_)) => {
                return value.type_of() == *self;
            }
            _ => return false,
        }
    }
}

/// What fingerprints of build arguments see: modules by name and artifact,
/// what they produced is hashed once per module by the build that uses them
impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Value::String(str) => str.hash(state),
            Value::Int(int) => int.hash(state),
            Value::Bool(bool) => bool.hash(state),
            Value::List(items) => items.hash(state),
            Value::Module(module) => match module.wait() {
                Ok(module) => {
                    module.name.hash(state);
                    module.artifact.hash(state);
                }
                Err(_) => module.function.hash(state),
            },
            Value::Unit => {}
            Value::Builtin { name, .. } => name.hash(state),
            Value::Function(function) => function.name.hash(state),
        }
    }
}

/// Structural. Function types are equal with the same parameter types, required in the
/// same places; names and default values do not matter.
impl PartialEq for Type {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Type::Any, Type::Any)
            | (Type::Unit, Type::Unit)
            | (Type::Bool, Type::Bool)
            | (Type::Int, Type::Int)
            | (Type::String, Type::String)
            | (Type::Module, Type::Module) => return true,
            (Type::List(l), Type::List(r)) => return l == r,
            (Type::Union(l), Type::Union(r)) => return l == r,
            (Type::Function(l), Type::Function(r)) => return l.same_shape(r),
            _ => return false,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Value::String(str) => return write!(f, "{str}"),
            Value::Int(int) => return write!(f, "{int}"),
            Value::Bool(bool) => return write!(f, "{bool}"),
            Value::Module(module) => match module.wait() {
                Ok(module) => return write!(f, "{}", module.artifact.display()),
                Err(_) => return write!(f, "<failed {}>", module.function),
            },
            Value::List(items) => {
                let items: Vec<String> = items.iter()
                    .map(|item| item.to_string())
                    .collect();
                return write!(f, "[{}]", items.join(", "));
            }
            Value::Unit => return write!(f, "()"),
            Value::Builtin { name, .. } => return write!(f, "<builtin {name}>"),
            Value::Function(function) => return write!(f, "<fn {}>", function.name),
        }
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Type::Any => return write!(f, "any"),
            Type::Unit => return write!(f, "unit"),
            Type::Bool => return write!(f, "bool"),
            Type::Int => return write!(f, "int"),
            Type::String => return write!(f, "str"),
            Type::Module => return write!(f, "module"),
            Type::List(element) => return write!(f, "list<{element}>"),
            Type::Function(signature) => return write!(f, "{signature}"),
            Type::Union(types) => {
                let names: Vec<String> = types.iter()
                    .map(|member_type| member_type.to_string())
                    .collect();
                return write!(f, "{}", names.join(" | "));
            }
        }
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        return Value::String(value.to_string());
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        return Value::String(value);
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        return Value::Bool(value);
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        return Value::Int(value);
    }
}
