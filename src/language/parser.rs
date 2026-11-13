use std::fmt;

use crate::language::lexer::{Span, Token, TokenStream, TokenType};

#[derive(Debug)]
pub enum ParseErrorType {
    UnexpectedToken {
        /// as a message says it: ')', a name
        expected: String,
        got: Token,
    },
    InvalidAssignmentTarget,
    /// use plugins/go.build, which reads as a division
    PluginWithoutQuotes,
}

#[derive(Debug)]
pub struct ParseError {
    pub variant: ParseErrorType,
    pub span: Span,
}

impl ParseError {
    fn unexpected_token(expected: &str, got: Token) -> Self {
        let span = got.span;

        return ParseError {
            variant: ParseErrorType::UnexpectedToken { expected: expected.to_string(), got },
            span,
        };
    }
}

impl fmt::Display for ParseErrorType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ParseErrorType::UnexpectedToken { expected, got } => {
                // an unclosed string runs to the end
                let got = match &got.variant {
                    TokenType::EOF | TokenType::UnclosedString => "the end of the script".to_string(),
                    TokenType::String(text) => format!("\"{text}\""),
                    other => format!("'{}'", other.to_string()),
                };

                return write!(f, "expected {expected}, got {got}");
            }
            ParseErrorType::InvalidAssignmentTarget => {
                return write!(f, "only variables can be assigned to");
            }
            ParseErrorType::PluginWithoutQuotes => {
                return write!(f, "the file of a plugin goes in quotes: use \"./plugins/go.build\"");
            }
        }
    }
}

pub struct Parser<'src> {
    token_stream: TokenStream<'src>,
}

#[derive(Debug, Clone)]
pub enum UnaryOperator {
    Neg,
    Not,
}

#[derive(Debug, Clone)]
pub enum BinaryOperator {
    Add,
    Sub,
    Mul,
    Div,
    Eq,
    NotEq,
    Less,
    LessEq,
    Greater,
    GreaterEq,
    And,
    Or,
}

/// As the script writes them, for messages
impl fmt::Display for UnaryOperator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let symbol = match self {
            UnaryOperator::Neg => "-",
            UnaryOperator::Not => "!",
        };

        return write!(f, "{symbol}");
    }
}

impl fmt::Display for BinaryOperator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let symbol = match self {
            BinaryOperator::Add => "+",
            BinaryOperator::Sub => "-",
            BinaryOperator::Mul => "*",
            BinaryOperator::Div => "/",
            BinaryOperator::Eq => "==",
            BinaryOperator::NotEq => "!=",
            BinaryOperator::Less => "<",
            BinaryOperator::LessEq => "<=",
            BinaryOperator::Greater => ">",
            BinaryOperator::GreaterEq => ">=",
            BinaryOperator::And => "&&",
            BinaryOperator::Or => "||",
        };

        return write!(f, "{symbol}");
    }
}

#[derive(Debug, Clone)]
pub enum ExpressionType {
    Ident(String),
    String(String),
    Unary {
        operator: UnaryOperator,
        target: Box<Expression>,
    },
    Binary {
        left: Box<Expression>,
        operator: BinaryOperator,
        right: Box<Expression>,
    },
    Assignment {
        target: Identifier,
        value: Box<Expression>,
    },
    FunctionCall {
        target: Box<Expression>,
        args: Vec<Expression>,
    },
    /// fn (name: str, count: int = 1) -> str { statements, the last one is the value }
    Function {
        params: Vec<Parameter>,
        returns: Option<TypeExpression>,
        body: Vec<Expression>,
    },
    /// if condition { ... } else { ... }, the value of the block that runs;
    /// `else if` is an else block with one if in it
    If {
        condition: Box<Expression>,
        then: Vec<Expression>,
        otherwise: Option<Vec<Expression>>,
    },
}

#[derive(Debug, Clone)]
pub struct Expression {
    pub variant: ExpressionType,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum TypeExpressionType {
    /// str, int, bool, module, any, unit
    Named(String),
    /// "auto": only this string, in a union one of several like "auto" | "local"
    Variant(String),
    /// list<str>
    List(Box<TypeExpression>),
    /// str | module
    Union(Vec<TypeExpression>),
    /// fn(str, int) -> bool
    Function {
        params: Vec<TypeExpression>,
        returns: Box<TypeExpression>,
    },
}

#[derive(Debug, Clone)]
pub struct TypeExpression {
    pub variant: TypeExpressionType,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Identifier {
    pub name: String,
    pub span: Span,
}

/// `name: str`, or with a default `name: str = "Maggus"`
#[derive(Debug, Clone)]
pub struct Parameter {
    pub name: Identifier,
    pub param_type: TypeExpression,
    pub default: Option<Expression>,
}

#[derive(Debug)]
pub enum StatementType {
    /// `use kotlin`
    Use(Identifier),
    /// `use "./plugins/go.build"`: the functions and values a plugin defines
    UsePlugin(String),
    Expression(Expression),
}

#[derive(Debug)]
pub struct Statement {
    pub variant: StatementType,
    pub span: Span,
}

#[derive(Debug)]
pub struct Script {
    pub statements: Vec<Statement>,
}

/// -x, !x
const PREFIX_PRECEDENCE: u8 = 15;
/// f(x)
const CALL_PRECEDENCE: u8 = 17;
/// everything that binds tighter than =, for default values
const NO_ASSIGNMENT_PRECEDENCE: u8 = 3;

const fn infix_precedence(variant: &TokenType) -> Option<(u8, u8)> {
    return Some(match variant {
        TokenType::Eq => (2, 1),
        TokenType::PipePipe => (3, 4),
        TokenType::AmpAmp => (5, 6),
        TokenType::EqEq | TokenType::BangEq => (7, 8),
        TokenType::Less | TokenType::LessEq | TokenType::Greater | TokenType::GreaterEq => (9, 10),
        TokenType::Plus | TokenType::Minus => (11, 12),
        TokenType::Star | TokenType::Slash => (13, 14),
        _ => return None,
    });
}

impl<'src> Parser<'src> {
    /// Of the script
    pub fn new(input: &'src str) -> Self {
        return Self::of_source(input, 0);
    }

    /// Of a file with its own number in the spans, like a plugin
    pub fn of_source(input: &'src str, source: usize) -> Self {
        return Parser {
            token_stream: TokenStream::new(input, source),
        };
    }

    pub fn parse_script(&mut self) -> Result<Script, ParseError> {
        let mut statements = vec![];

        while self.token_stream.peek().variant != TokenType::EOF {
            statements.push(self.parse_statement()?);
        }

        return Ok(Script { statements });
    }

    fn parse_statement(&mut self) -> Result<Statement, ParseError> {
        if self.token_stream.peek().variant == TokenType::Use {
            return self.parse_use_statement();
        }

        let expression = self.parse_expression(0)?;
        let span = expression.span;

        return Ok(Statement {
            variant: StatementType::Expression(expression),
            span,
        });
    }

    fn parse_use_statement(&mut self) -> Result<Statement, ParseError> {
        let use_token = self.expect(TokenType::Use)?;

        if let TokenType::String(plugin) = self.token_stream.peek().variant {
            let plugin_token = self.token_stream.next();

            return Ok(Statement {
                variant: StatementType::UsePlugin(plugin),
                span: use_token.span.till(plugin_token.span),
            });
        }

        let platform = self.expect_identifier("a platform like kotlin or a plugin like \"./go.build\"")?;
        let span = use_token.span.till(platform.span);

        if platform.name.ends_with(".build") || self.token_stream.peek().variant == TokenType::Slash {
            return Err(ParseError {
                variant: ParseErrorType::PluginWithoutQuotes,
                span,
            });
        }

        return Ok(Statement {
            variant: StatementType::Use(platform),
            span,
        });
    }

    /// The next token, which has to be `expected`
    fn expect(&mut self, expected: TokenType) -> Result<Token, ParseError> {
        let token = self.token_stream.next();

        if token.variant != expected {
            return Err(ParseError::unexpected_token(&format!("'{}'", expected.to_string()), token));
        }

        return Ok(token);
    }

    /// The next token, which has to be a name. `what` says which for the message.
    fn expect_identifier(&mut self, what: &str) -> Result<Identifier, ParseError> {
        let token = self.token_stream.next();

        let TokenType::Ident(name) = token.variant else {
            return Err(ParseError::unexpected_token(what, token));
        };

        return Ok(Identifier { name, span: token.span });
    }

    /// Items up to the closing ')', separated by commas, one after the last allowed.
    /// Returns the span of the ')' too.
    fn comma_separated<T>(
        &mut self,
        mut item: impl FnMut(&mut Self) -> Result<T, ParseError>,
    ) -> Result<(Vec<T>, Span), ParseError> {
        let mut items = vec![];

        while self.token_stream.peek().variant != TokenType::RightParen {
            items.push(item(self)?);

            if self.token_stream.peek().variant != TokenType::Comma {
                break;
            }

            self.token_stream.next();
        }

        let close = self.expect(TokenType::RightParen)?;

        return Ok((items, close.span));
    }

    fn parse_expression(&mut self, min_precedence: u8) -> Result<Expression, ParseError> {
        let mut left_expression = self.parse_prefix_expression()?;

        loop {
            let next_token = self.token_stream.peek();

            if next_token.variant == TokenType::LeftParen {
                if min_precedence > CALL_PRECEDENCE {
                    break;
                }

                left_expression = self.parse_function_call(left_expression)?;
                continue;
            }

            let Some((left_precedence, right_precedence)) = infix_precedence(&next_token.variant)
            else {
                break;
            };

            if left_precedence < min_precedence {
                break;
            }

            self.token_stream.next();
            let right_expression = self.parse_expression(right_precedence)?;

            left_expression =
                Self::build_infix_expression(next_token, left_expression, right_expression)?;
        }

        return Ok(left_expression);
    }

    fn parse_prefix_expression(&mut self) -> Result<Expression, ParseError> {
        let token = self.token_stream.next();
        let span = token.span;

        match token.variant {
            TokenType::Ident(str) => {
                return Ok(Expression {
                    variant: ExpressionType::Ident(str),
                    span,
                });
            }
            TokenType::String(str) => {
                return Ok(Expression {
                    variant: ExpressionType::String(str),
                    span,
                });
            }
            TokenType::UnclosedString => {
                return Err(ParseError::unexpected_token("\" to close the string", Token::new(TokenType::UnclosedString, span)));
            }
            TokenType::Fn => return self.parse_function(span),
            TokenType::If => return self.parse_if(span),
            TokenType::Minus => return self.parse_unary_expression(UnaryOperator::Neg, span),
            TokenType::Bang => return self.parse_unary_expression(UnaryOperator::Not, span),
            TokenType::LeftParen => {
                let inner = self.parse_expression(0)?;
                let close = self.expect(TokenType::RightParen)?;

                return Ok(Expression {
                    variant: inner.variant,
                    span: span.till(close.span),
                });
            }
            other => return Err(ParseError::unexpected_token("an expression", Token::new(other, span))),
        }
    }

    fn parse_function(&mut self, fn_span: Span) -> Result<Expression, ParseError> {
        self.expect(TokenType::LeftParen)?;

        let (params, _) = self.comma_separated(|parser| {
            let name = parser.expect_identifier("a parameter name")?;
            parser.expect(TokenType::Colon)?;
            let param_type = parser.parse_type()?;

            let default = match parser.token_stream.peek().variant {
                TokenType::Eq => {
                    parser.token_stream.next();
                    Some(parser.parse_expression(NO_ASSIGNMENT_PRECEDENCE)?)
                }
                _ => None,
            };

            return Ok(Parameter {
                name,
                param_type,
                default,
            });
        })?;

        let returns = match self.token_stream.peek().variant {
            TokenType::Arrow => {
                self.token_stream.next();
                Some(self.parse_type()?)
            }
            _ => None,
        };

        let (body, close) = self.parse_block()?;

        return Ok(Expression {
            variant: ExpressionType::Function {
                params,
                returns,
                body,
            },
            span: fn_span.till(close),
        });
    }

    fn parse_if(&mut self, if_span: Span) -> Result<Expression, ParseError> {
        let condition = self.parse_expression(0)?;
        let (then, mut close) = self.parse_block()?;

        let otherwise = match self.token_stream.peek().variant {
            TokenType::Else => {
                self.token_stream.next();
                let else_token = self.token_stream.peek();

                if else_token.variant == TokenType::If {
                    self.token_stream.next();
                    let nested = self.parse_if(else_token.span)?;
                    close = nested.span;
                    Some(vec![nested])
                } else {
                    let (block, block_close) = self.parse_block()?;
                    close = block_close;
                    Some(block)
                }
            }
            _ => None,
        };

        return Ok(Expression {
            variant: ExpressionType::If {
                condition: Box::new(condition),
                then,
                otherwise,
            },
            span: if_span.till(close),
        });
    }

    /// `{ expressions }`, with the span of the closing brace
    fn parse_block(&mut self) -> Result<(Vec<Expression>, Span), ParseError> {
        self.expect(TokenType::LeftBrace)?;
        let mut body = vec![];

        while !matches!(
            self.token_stream.peek().variant,
            TokenType::RightBrace | TokenType::EOF
        ) {
            body.push(self.parse_expression(0)?);
        }

        let close = self.expect(TokenType::RightBrace)?;

        return Ok((body, close.span));
    }

    /// A type, members of a union separated by |
    fn parse_type(&mut self) -> Result<TypeExpression, ParseError> {
        let first = self.parse_single_type()?;

        if self.token_stream.peek().variant != TokenType::Pipe {
            return Ok(first);
        }

        let mut members = vec![first];

        while self.token_stream.peek().variant == TokenType::Pipe {
            self.token_stream.next();
            members.push(self.parse_single_type()?);
        }

        let span = members[0].span.till(members[members.len() - 1].span);

        return Ok(TypeExpression {
            variant: TypeExpressionType::Union(members),
            span,
        });
    }

    fn parse_single_type(&mut self) -> Result<TypeExpression, ParseError> {
        let token = self.token_stream.next();
        match token.variant {
            TokenType::Ident(name) if name == "list" => {
                self.expect(TokenType::Less)?;
                let element = self.parse_type()?;
                let close = self.expect(TokenType::Greater)?;

                return Ok(TypeExpression {
                    variant: TypeExpressionType::List(Box::new(element)),
                    span: token.span.till(close.span),
                });
            }
            TokenType::Ident(name) => {
                return Ok(TypeExpression {
                    variant: TypeExpressionType::Named(name),
                    span: token.span,
                });
            }
            TokenType::String(text) => {
                return Ok(TypeExpression {
                    variant: TypeExpressionType::Variant(text),
                    span: token.span,
                });
            }
            TokenType::Fn => {
                self.expect(TokenType::LeftParen)?;
                let (params, _) = self.comma_separated(|parser| parser.parse_type())?;
                self.expect(TokenType::Arrow)?;
                let returns = self.parse_single_type()?;
                let span = token.span.till(returns.span);

                return Ok(TypeExpression {
                    variant: TypeExpressionType::Function {
                        params,
                        returns: Box::new(returns),
                    },
                    span,
                });
            }
            other => return Err(ParseError::unexpected_token("a type", Token::new(other, token.span))),
        }
    }

    fn parse_unary_expression(
        &mut self,
        operator: UnaryOperator,
        operator_span: Span,
    ) -> Result<Expression, ParseError> {
        let target = self.parse_expression(PREFIX_PRECEDENCE)?;
        let span = operator_span.till(target.span);

        return Ok(Expression {
            variant: ExpressionType::Unary {
                operator,
                target: Box::new(target),
            },
            span,
        });
    }

    fn parse_function_call(&mut self, target: Expression) -> Result<Expression, ParseError> {
        self.expect(TokenType::LeftParen)?;
        let (args, close) = self.comma_separated(|parser| parser.parse_expression(0))?;
        let span = target.span.till(close);

        return Ok(Expression {
            variant: ExpressionType::FunctionCall {
                target: Box::new(target),
                args,
            },
            span,
        });
    }

    fn build_infix_expression(
        operator_token: Token,
        left: Expression,
        right: Expression,
    ) -> Result<Expression, ParseError> {
        let span = left.span.till(right.span);

        let operator = match operator_token.variant {
            TokenType::Eq => {
                let ExpressionType::Ident(name) = left.variant else {
                    return Err(ParseError {
                        variant: ParseErrorType::InvalidAssignmentTarget,
                        span: left.span,
                    });
                };

                return Ok(Expression {
                    variant: ExpressionType::Assignment {
                        target: Identifier { name, span: left.span },
                        value: Box::new(right),
                    },
                    span,
                });
            }
            TokenType::EqEq => BinaryOperator::Eq,
            TokenType::BangEq => BinaryOperator::NotEq,
            TokenType::Plus => BinaryOperator::Add,
            TokenType::Minus => BinaryOperator::Sub,
            TokenType::Star => BinaryOperator::Mul,
            TokenType::Slash => BinaryOperator::Div,
            TokenType::Less => BinaryOperator::Less,
            TokenType::LessEq => BinaryOperator::LessEq,
            TokenType::Greater => BinaryOperator::Greater,
            TokenType::GreaterEq => BinaryOperator::GreaterEq,
            TokenType::AmpAmp => BinaryOperator::And,
            TokenType::PipePipe => BinaryOperator::Or,
            other => unreachable!("{other:?} is no binary expression"),
        };

        return Ok(Expression {
            variant: ExpressionType::Binary {
                left: Box::new(left),
                operator,
                right: Box::new(right),
            },
            span,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &str) -> Result<Script, ParseError> {
        return Parser::new(input).parse_script();
    }

    fn error(input: &str) -> String {
        return parse(input).unwrap_err().variant.to_string();
    }

    /// The expression of the only statement, with parentheses for how it binds
    fn grouped(input: &str) -> String {
        fn show(expression: &Expression) -> String {
            match &expression.variant {
                ExpressionType::Ident(name) => return name.clone(),
                ExpressionType::String(text) => return format!("\"{text}\""),
                ExpressionType::Unary { operator, target } => return format!("({operator}{})", show(target)),
                ExpressionType::Binary { left, operator, right } => {
                    return format!("({} {operator} {})", show(left), show(right));
                }
                ExpressionType::Assignment { target, value } => return format!("({} = {})", target.name, show(value)),
                ExpressionType::FunctionCall { target, args } => {
                    let args: Vec<String> = args.iter()
                        .map(show)
                        .collect();
                    return format!("{}({})", show(target), args.join(", "));
                }
                other => return format!("{other:?}"),
            }
        }

        let script = parse(input).unwrap();
        let [Statement { variant: StatementType::Expression(expression), .. }] = script.statements.as_slice() else {
            panic!("one expression expected");
        };

        return show(expression);
    }

    #[test]
    fn operators_bind_by_precedence() {
        assert_eq!(grouped("a = b || c && d == e + f * -g"), "(a = (b || (c && (d == (e + (f * (-g)))))))");
        assert_eq!(grouped("!f(x)(y) < z"), "((!f(x)(y)) < z)");
        assert_eq!(grouped("(a + b) * c"), "((a + b) * c)");
    }

    #[test]
    fn arguments_may_end_with_a_comma_and_be_named() {
        assert_eq!(grouped("build(\"app\", main = true,)"), "build(\"app\", (main = true))");
        assert_eq!(grouped("build()"), "build()");
    }

    #[test]
    fn errors_name_the_tokens_as_the_script_writes_them() {
        assert_eq!(error("f(a"), "expected ')', got the end of the script");
        assert_eq!(error("fn(a str) { a }"), "expected ':', got 'str'");
        assert_eq!(error("fn(\"a\": str) { a }"), "expected a parameter name, got \"a\"");
        assert_eq!(error("use ("), "expected a platform like kotlin or a plugin like \"./go.build\", got '('");
        assert_eq!(error("use plugins/go.build"), "the file of a plugin goes in quotes: use \"./plugins/go.build\"");
        assert_eq!(error("use go.build"), "the file of a plugin goes in quotes: use \"./plugins/go.build\"");
        assert_eq!(error("x = )"), "expected an expression, got ')'");
        assert_eq!(error("f(x) = 1"), "only variables can be assigned to");
    }

    /// The parameters, return type and body of `name = fn(...) { ... }`
    fn function_parts(source: &str) -> (Vec<Parameter>, Option<TypeExpression>, Vec<Expression>) {
        let script = parse(source).unwrap();
        let [Statement { variant: StatementType::Expression(expression), .. }] = script.statements.as_slice() else {
            panic!("one expression expected");
        };

        let ExpressionType::Assignment { value, .. } = &expression.variant else {
            panic!("an assignment expected");
        };

        let ExpressionType::Function { params, returns, body } = &value.variant else {
            panic!("a function expected");
        };

        return (params.clone(), returns.clone(), body.clone());
    }

    /// The variants of a type written as a variant or a union of them
    fn variants(type_expression: &TypeExpression) -> Vec<String> {
        match &type_expression.variant {
            TypeExpressionType::Variant(name) => return vec![name.clone()],
            TypeExpressionType::Union(members) => return members.iter().flat_map(variants).collect(),
            _ => return vec![],
        }
    }

    #[test]
    fn functions_have_typed_parameters_with_defaults() {
        let (params, returns, body) =
            function_parts("greet = fn(name: str | list<str>, count: int = 1, call: fn(str) -> bool) -> str { name }");

        let names: Vec<&str> = params.iter()
            .map(|param| param.name.name.as_str())
            .collect();

        assert_eq!(names, ["name", "count", "call"]);
        assert!(matches!(params[0].param_type.variant, TypeExpressionType::Union(ref members) if members.len() == 2));
        assert!(params[1].default.is_some());
        assert!(matches!(params[2].param_type.variant, TypeExpressionType::Function { ref params, .. } if params.len() == 1));
        assert!(matches!(returns, Some(TypeExpression { variant: TypeExpressionType::Named(name), .. }) if name == "str"));
        assert_eq!(body.len(), 1);
    }

    #[test]
    fn strings_in_types_are_variants() {
        let (params, returns, _) = function_parts(
            "pick = fn(mode: \"auto\" | \"local\" = \"auto\", level: \"debug\", modes: list<\"fast\" | \"small\">, \
             call: fn(\"a\" | \"b\") -> str) -> \"x\" | \"y\" { mode }",
        );

        assert_eq!(variants(&params[0].param_type), ["auto", "local"]);
        assert_eq!(variants(&params[1].param_type), ["debug"]);
        assert!(matches!(&params[2].param_type.variant, TypeExpressionType::List(element) if variants(element) == ["fast", "small"]));
        assert!(matches!(&params[3].param_type.variant, TypeExpressionType::Function { params, .. } if variants(&params[0]) == ["a", "b"]));
        assert_eq!(variants(&returns.unwrap()), ["x", "y"]);
    }
}
