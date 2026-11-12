#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    /// the file: 0 for the script, plugins have numbers of their own
    pub source: usize,
}

impl Span {
    pub fn till(&self, end: Span) -> Span {
        return Span {
            end: end.end,
            ..*self
        };
    }

    pub fn new(start: usize, end: usize) -> Self {
        return Span { start, end, source: 0 };
    }
}

/// Line and column of a byte offset in the script, both counted from 1
pub fn line_column(source: &str, offset: usize) -> (usize, usize) {
    let before = source.get(..offset).unwrap_or(source);
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;

    return (line, column);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub variant: TokenType,
    pub span: Span,
}

impl Token {
    pub fn new(variant: TokenType, span: Span) -> Self {
        return Token { variant, span };
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenType {
    // Keywords
    Use,
    Fn,
    If,
    Else,

    // Operators
    EqEq,
    BangEq,
    Eq,
    Minus,
    Plus,
    Star,
    Slash,
    Bang,
    AmpAmp,
    PipePipe,
    LessEq,
    GreaterEq,

    // Brackets and separators
    LeftParen,
    RightParen,
    LeftBrace,
    RightBrace,
    Comma,

    // Types
    Colon,
    Arrow,
    Less,
    Greater,
    Pipe,

    Ident(String),
    String(String),
    /// the rest of the script, which no " closes
    UnclosedString,

    EOF,
}

impl ToString for TokenType {
    fn to_string(&self) -> String {
        return String::from(match self {
            TokenType::Use => "use",
            TokenType::Fn => "fn",
            TokenType::If => "if",
            TokenType::Else => "else",
            TokenType::EqEq => "==",
            TokenType::BangEq => "!=",
            TokenType::Eq => "=",
            TokenType::Minus => "-",
            TokenType::Plus => "+",
            TokenType::Star => "*",
            TokenType::Slash => "/",
            TokenType::Bang => "!",
            TokenType::AmpAmp => "&&",
            TokenType::PipePipe => "||",
            TokenType::LessEq => "<=",
            TokenType::GreaterEq => ">=",
            TokenType::LeftParen => "(",
            TokenType::RightParen => ")",
            TokenType::LeftBrace => "{",
            TokenType::RightBrace => "}",
            TokenType::Comma => ",",
            TokenType::Colon => ":",
            TokenType::Arrow => "->",
            TokenType::Less => "<",
            TokenType::Greater => ">",
            TokenType::Pipe => "|",
            TokenType::Ident(ident) => ident,
            TokenType::String(str) => str,
            TokenType::UnclosedString => "\"",
            TokenType::EOF => "<<EOF>>",
        });
    }
}

// longer symbols first: "->" before "-"
const CONTROL_SYMBOLS: &[(&'static str, TokenType)] = &[
    ("(", TokenType::LeftParen),
    (")", TokenType::RightParen),
    ("{", TokenType::LeftBrace),
    ("}", TokenType::RightBrace),
    (",", TokenType::Comma),
    (":", TokenType::Colon),
    ("<=", TokenType::LessEq),
    ("<", TokenType::Less),
    (">=", TokenType::GreaterEq),
    (">", TokenType::Greater),
    ("||", TokenType::PipePipe),
    ("|", TokenType::Pipe),
    ("&&", TokenType::AmpAmp),
    ("==", TokenType::EqEq),
    ("!=", TokenType::BangEq),
    ("=", TokenType::Eq),
    ("->", TokenType::Arrow),
    ("-", TokenType::Minus),
    ("+", TokenType::Plus),
    ("*", TokenType::Star),
    ("/", TokenType::Slash),
    ("!", TokenType::Bang),
];

const fn control_chars() -> [char; CONTROL_SYMBOLS.len()] {
    let mut out: [char; CONTROL_SYMBOLS.len()] = ['\0'; CONTROL_SYMBOLS.len()];

    let mut index: usize = 0;
    while index < CONTROL_SYMBOLS.len() {
        out[index] = CONTROL_SYMBOLS[index].0.as_bytes()[0] as char;
        index += 1;
    }

    return out;
}

const CONTROL_CHARS: [char; CONTROL_SYMBOLS.len()] = control_chars();

const KEYWORDS: &[(&'static str, TokenType)] = &[
    ("use", TokenType::Use),
    ("fn", TokenType::Fn),
    ("if", TokenType::If),
    ("else", TokenType::Else),
];

pub struct TokenStream<'src> {
    pub input: &'src str,
    pub current_pos: usize,
    /// what the spans of the tokens name as their file
    pub source: usize,
}

impl<'src> TokenStream<'src> {
    pub fn new(input: &'src str, source: usize) -> Self {
        return TokenStream {
            input,
            current_pos: 0,
            source,
        };
    }

    pub fn lex_token(&self, pos: usize) -> Token {
        let mut current_pos = self.skip_whitespace_and_comments(pos);
        let trimmed = &self.input[current_pos..];

        let start_pos = current_pos;

        let first_char = if let Some(next_char) = trimmed.chars().next() {
            next_char
        } else {
            return Token {
                variant: TokenType::EOF,
                span: Span::new(start_pos, start_pos),
            };
        };

        // \" and \\ for themselves, \n and \t for a new line and a tab, other backslashes stay
        if first_char == '"' {
            let mut text = String::new();
            let mut chars = trimmed[1..].char_indices();

            while let Some((index, char)) = chars.next() {
                match (char, chars.clone().next().map(|(_, next)| next)) {
                    ('"', _) => {
                        current_pos += index + 2;
                        return Token::new(TokenType::String(text), Span::new(start_pos, current_pos));
                    }
                    ('\\', Some(next @ ('"' | '\\' | 'n' | 't'))) => {
                        text.push(match next {
                            'n' => '\n',
                            't' => '\t',
                            other => other,
                        });
                        chars.next();
                    }
                    _ => text.push(char),
                }
            }

            current_pos += trimmed.len();
            return Token::new(TokenType::UnclosedString, Span::new(start_pos, current_pos));
        };

        if let Some((variant, len)) = Self::control_symbol(trimmed) {
            current_pos += len;
            return Token::new(variant, Span::new(start_pos, current_pos));
        };

        let mut word_length = trimmed.find(|char: char| char.is_whitespace() || char == '"' || CONTROL_CHARS.contains(&char))
            .unwrap_or(trimmed.len());

        // a character that only starts a symbol, like a single &: a token of its own, the parser names it
        if word_length == 0 {
            word_length = first_char.len_utf8();
        }

        let word = &trimmed[..word_length];
        current_pos += word_length;

        if let Some(keyword) = Self::keyword(word) {
            return Token::new(keyword, Span::new(start_pos, current_pos));
        };

        return Token::new(
            TokenType::Ident(word.to_string()),
            Span::new(start_pos, current_pos),
        );
    }

    pub fn next(&mut self) -> Token {
        let token = self.peek();
        self.current_pos = token.span.end;

        return token;
    }

    pub fn peek(&self) -> Token {
        let mut token = self.lex_token(self.current_pos);
        token.span.source = self.source;
        return token;
    }

    /// Position of the next token, after whitespace, `// …` and `/* … */`
    fn skip_whitespace_and_comments(&self, pos: usize) -> usize {
        let mut current_pos = pos;
        loop {
            let remaining = &self.input[current_pos..];
            let trimmed = remaining.trim_start();
            current_pos += remaining.len() - trimmed.len();

            if trimmed.starts_with("//") {
                current_pos += trimmed.find('\n').unwrap_or(trimmed.len());
            } else if trimmed.starts_with("/*") {
                current_pos += trimmed.find("*/")
                    .map(|end| end + 2)
                    .unwrap_or(trimmed.len());
            } else {
                return current_pos;
            }
        }
    }

    fn control_symbol(remaining: &str) -> Option<(TokenType, usize)> {
        for (symbol, token) in CONTROL_SYMBOLS {
            if remaining.starts_with(*symbol) {
                return Some((token.clone(), symbol.len()));
            }
        }

        return None;
    }

    fn keyword(word: &str) -> Option<TokenType> {
        for (keyword, token) in KEYWORDS {
            if *keyword == word {
                return Some(token.clone());
            }
        }

        return None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(input: &str) -> Vec<TokenType> {
        let mut stream = TokenStream::new(input, 0);
        let mut tokens = vec![];

        loop {
            let token = stream.next();

            if token.variant == TokenType::EOF {
                return tokens;
            }

            tokens.push(token.variant);
        }
    }

    #[test]
    fn longer_symbols_win_and_keywords_are_whole_words() {
        assert_eq!(
            tokens("fn(a: str) -> list<int> { a <= b && !c }"),
            vec![
                TokenType::Fn,
                TokenType::LeftParen,
                TokenType::Ident("a".to_string()),
                TokenType::Colon,
                TokenType::Ident("str".to_string()),
                TokenType::RightParen,
                TokenType::Arrow,
                TokenType::Ident("list".to_string()),
                TokenType::Less,
                TokenType::Ident("int".to_string()),
                TokenType::Greater,
                TokenType::LeftBrace,
                TokenType::Ident("a".to_string()),
                TokenType::LessEq,
                TokenType::Ident("b".to_string()),
                TokenType::AmpAmp,
                TokenType::Bang,
                TokenType::Ident("c".to_string()),
                TokenType::RightBrace,
            ]
        );

        assert_eq!(tokens("useful iffy"), vec![TokenType::Ident("useful".to_string()), TokenType::Ident("iffy".to_string())]);
    }

    #[test]
    fn strings_unescape_and_comments_are_skipped() {
        assert_eq!(
            tokens("// a comment\n\"say \\\"hi\\\"\\n\" /* another */ \"C:\\dir\""),
            vec![
                TokenType::String("say \"hi\"\n".to_string()),
                TokenType::String("C:\\dir".to_string()),
            ]
        );

        assert_eq!(tokens("x = \"open"), vec![
            TokenType::Ident("x".to_string()),
            TokenType::Eq,
            TokenType::UnclosedString,
        ]);
    }

    #[test]
    fn errors_point_at_line_and_column() {
        let source = "use kotlin\nx = list(\n    y)";
        assert_eq!(line_column(source, 0), (1, 1));
        assert_eq!(line_column(source, 11), (2, 1));
        assert_eq!(line_column(source, source.len() - 2), (3, 5));
    }
}
