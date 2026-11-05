use std::fmt;

use crate::language::lexer::{Span, Token, TokenStream, TokenType};

macro_rules! expect_token {
    ($parser: expr, $pattern: pat => $out: expr) => {{
        let token = $parser.token_stream.peek();
        match token.variant {
            $pattern => {
                $parser.token_stream.current_pos = token.span.end;
                Ok(($out, token.span))
            }
            _ => Err(ParseError::unexpected_token(
                vec![stringify!($pattern).to_string()],
                Some(token.clone()),
                token.span,
            )),
        }
    }};

    ($parser: expr, $pattern: pat) => {{
        let token = $parser.token_stream.peek();
        if matches!(token.variant, $pattern) {
            $parser.token_stream.current_pos = token.span.end;
            Ok(token)
        } else {
            Err(ParseError::unexpected_token(
                vec![stringify!($pattern).to_string()],
                Some(token.clone()),
                token.span,
            ))
        }
    }};
}

#[derive(Debug)]
pub enum ParseErrorType {
    UnexpectedToken {
        /// alternatives
        expected: Vec<String>,
        got: Option<Token>,
    },
    InvalidAssignmentTarget,
}

#[derive(Debug)]
pub struct ParseError {
    pub variant: ParseErrorType,
    pub span: Span,
}

impl ParseError {
    fn unexpected_token(expected: Vec<String>, got: Option<Token>, span: Span) -> Self {
        return ParseError {
            variant: ParseErrorType::UnexpectedToken { expected, got },
            span,
        };
    }
}

impl fmt::Display for ParseErrorType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ParseErrorType::UnexpectedToken { expected, got } => {
                let expected: Vec<&str> = expected.iter()
                    .map(|name| name.trim_start_matches("TokenType::"))
                    .collect();

                let got = match got.as_ref().map(|token| &token.variant) {
                    None | Some(TokenType::EOF) => "the end of the script".to_string(),
                    Some(TokenType::String(text)) => format!("\"{text}\""),
                    Some(other) => format!("'{}'", other.to_string()),
                };

                return write!(f, "expected {}, got {got}", expected.join(" or "));
            }
            ParseErrorType::InvalidAssignmentTarget => {
                return write!(f, "only variables can be assigned to");
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
        target: Box<Expression>,
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

/// `use kotlin`, or a script: `use "other.build"`
#[derive(Debug)]
pub enum UseTarget {
    Ident(Identifier),
    FilePath { path: String, span: Span },
}

#[derive(Debug)]
pub enum StatementType {
    Use(UseTarget),
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
    pub fn new(input: &'src str) -> Self {
        return Parser {
            token_stream: TokenStream::new(input),
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
        let use_token = expect_token!(self, TokenType::Use)?;

        let token = self.token_stream.next();

        let target = match token.variant {
            TokenType::Ident(name) => UseTarget::Ident(Identifier {
                name,
                span: token.span,
            }),
            TokenType::String(path) => UseTarget::FilePath {
                path,
                span: token.span,
            },
            other => {
                return Err(ParseError::unexpected_token(
                    vec!["Ident".to_string(), "String".to_string()],
                    Some(Token::new(other, token.span)),
                    token.span,
                ));
            }
        };

        return Ok(Statement {
            variant: StatementType::Use(target),
            span: use_token.span.till(token.span),
        });
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
                return Err(ParseError::unexpected_token(vec!["\" to close the string".to_string()], None, span));
            }
            TokenType::Fn => return self.parse_function(span),
            TokenType::If => return self.parse_if(span),
            TokenType::Minus => return self.parse_unary_expression(UnaryOperator::Neg, span),
            TokenType::Bang => return self.parse_unary_expression(UnaryOperator::Not, span),
            TokenType::LeftParen => {
                let inner = self.parse_expression(0)?;
                let close = expect_token!(self, TokenType::RightParen)?;

                return Ok(Expression {
                    variant: inner.variant,
                    span: span.till(close.span),
                });
            }
            other => {
                return Err(ParseError::unexpected_token(
                    vec!["Expression".to_string()],
                    Some(Token::new(other, span)),
                    span,
                ));
            }
        }
    }

    fn parse_function(&mut self, fn_span: Span) -> Result<Expression, ParseError> {
        expect_token!(self, TokenType::LeftParen)?;
        let mut params = vec![];

        while self.token_stream.peek().variant != TokenType::RightParen {
            let (name, span) = expect_token!(self, TokenType::Ident(name) => name)?;
            let name = Identifier { name, span };
            expect_token!(self, TokenType::Colon)?;
            let param_type = self.parse_type()?;

            let default = match self.token_stream.peek().variant {
                TokenType::Eq => {
                    self.token_stream.next();
                    Some(self.parse_expression(NO_ASSIGNMENT_PRECEDENCE)?)
                }
                _ => None,
            };

            params.push(Parameter {
                name,
                param_type,
                default,
            });

            if self.token_stream.peek().variant != TokenType::Comma {
                break;
            }

            self.token_stream.next();
        }

        expect_token!(self, TokenType::RightParen)?;

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
        expect_token!(self, TokenType::LeftBrace)?;
        let mut body = vec![];

        while !matches!(
            self.token_stream.peek().variant,
            TokenType::RightBrace | TokenType::EOF
        ) {
            body.push(self.parse_expression(0)?);
        }

        let close = expect_token!(self, TokenType::RightBrace)?;

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
                expect_token!(self, TokenType::Less)?;
                let element = self.parse_type()?;
                let close = expect_token!(self, TokenType::Greater)?;

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
            TokenType::Fn => {
                expect_token!(self, TokenType::LeftParen)?;
                let mut params = vec![];

                while self.token_stream.peek().variant != TokenType::RightParen {
                    params.push(self.parse_type()?);
                    if self.token_stream.peek().variant != TokenType::Comma {
                        break;
                    }

                    self.token_stream.next();
                }

                expect_token!(self, TokenType::RightParen)?;
                expect_token!(self, TokenType::Arrow)?;
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
            other => {
                return Err(ParseError::unexpected_token(
                    vec!["Type".to_string()],
                    Some(Token::new(other, token.span)),
                    token.span,
                ));
            }
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
        expect_token!(self, TokenType::LeftParen)?;

        let mut args = vec![];

        if self.token_stream.peek().variant != TokenType::RightParen {
            loop {
                args.push(self.parse_expression(0)?);
                if self.token_stream.peek().variant != TokenType::Comma {
                    break;
                }

                self.token_stream.next();

                // allows a comma after the last argument
                if self.token_stream.peek().variant == TokenType::RightParen {
                    break;
                }
            }
        }

        let close = expect_token!(self, TokenType::RightParen)?;
        let span = target.span.till(close.span);

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
                if !matches!(left.variant, ExpressionType::Ident(_)) {
                    return Err(ParseError {
                        variant: ParseErrorType::InvalidAssignmentTarget,
                        span: left.span,
                    });
                }

                return Ok(Expression {
                    variant: ExpressionType::Assignment {
                        target: Box::new(left),
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
