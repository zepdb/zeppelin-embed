// Token/error separation, punctuation dispatch and comment scanning adapted from
// Shopify/cypher-parser a7b822fbece9ee2c3f2b57ecfd4e90a2ca215383, src/lexer.rs.
// Copyright (c) 2025-present Shopify Inc. See ../SHOPIFY-LICENSE-MIT.
// Byte spans, borrowing, escapes, numbers, allocation and interruption are local.
use crate::resources::{charge, poll, push};
use crate::{CompileLimits, ErrorKind, LimitKind, ParseError, ResourceError, Resources, Span};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Ident,
    QuotedName,
    Integer,
    Float,
    String,
    Parameter,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Colon,
    Dot,
    DotDot,
    Star,
    Pipe,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Minus,
    Plus,
    Slash,
    Percent,
    Semicolon,
}
#[derive(Debug)]
pub(crate) struct Token {
    pub kind: Kind,
    pub span: Span,
    decoded: Option<String>,
}
impl Token {
    pub fn text<'a>(&'a self, source: &'a str) -> Option<&'a str> {
        self.decoded
            .as_deref()
            .or_else(|| source.get(self.span.start..self.span.end))
    }
}

pub(crate) fn tokenize(
    source: &str,
    limits: CompileLimits,
    resources: &mut dyn Resources,
) -> Result<Vec<Token>, ParseError> {
    let mut lexer = Lexer {
        source,
        position: 0,
        resources,
    };
    let mut tokens = Vec::new();
    while let Some(c) = lexer.peek() {
        let start = lexer.position;
        let mut decoded = None;
        if c.is_ascii_whitespace() {
            lexer.advance()?;
            continue;
        }
        if lexer.starts("//") {
            while lexer.peek().is_some_and(|c| c != '\n') {
                lexer.advance()?;
            }
            continue;
        }
        if lexer.starts("/*") {
            lexer.advance()?;
            lexer.advance()?;
            while !lexer.starts("*/") {
                if lexer.advance()?.is_none() {
                    return Err(lexer.error(
                        start,
                        ErrorKind::Syntax,
                        "unterminated block comment",
                    ));
                }
            }
            lexer.advance()?;
            lexer.advance()?;
            continue;
        }
        let kind = match c {
            '(' => Kind::LParen,
            ')' => Kind::RParen,
            '[' => Kind::LBracket,
            ']' => Kind::RBracket,
            '{' => Kind::LBrace,
            '}' => Kind::RBrace,
            ',' => Kind::Comma,
            ':' => Kind::Colon,
            '*' => Kind::Star,
            '|' => Kind::Pipe,
            '=' => Kind::Eq,
            '-' => Kind::Minus,
            '+' => Kind::Plus,
            '/' => Kind::Slash,
            '%' => Kind::Percent,
            ';' => Kind::Semicolon,
            '.' if lexer.starts("..") => {
                lexer.advance()?;
                Kind::DotDot
            }
            '<' if lexer.starts("<=") => {
                lexer.advance()?;
                Kind::Le
            }
            '<' if lexer.starts("<>") => {
                lexer.advance()?;
                Kind::Ne
            }
            '>' if lexer.starts(">=") => {
                lexer.advance()?;
                Kind::Ge
            }
            '<' => Kind::Lt,
            '>' => Kind::Gt,
            '`' | '\'' | '"' => {
                decoded = Some(lexer.quoted(c)?);
                if c == '`' {
                    Kind::QuotedName
                } else {
                    Kind::String
                }
            }
            '$' => {
                lexer.advance()?;
                if lexer.peek() == Some('`') {
                    decoded = Some(lexer.quoted('`')?);
                } else {
                    let name_start = lexer.position;
                    if !lexer.peek().is_some_and(ident_start) {
                        return Err(lexer.error(
                            start,
                            ErrorKind::Unsupported,
                            "expected named parameter",
                        ));
                    }
                    lexer.identifier()?;
                    let name = source.get(name_start..lexer.position).ok_or_else(|| {
                        lexer.error(start, ErrorKind::Syntax, "invalid parameter span")
                    })?;
                    decoded = Some(crate::resources::copy_string(
                        name,
                        lexer.resources,
                        Span {
                            start,
                            end: lexer.position,
                        },
                    )?);
                }
                Kind::Parameter
            }
            c if c.is_ascii_digit()
                || (c == '.' && lexer.peek_next().is_some_and(|c| c.is_ascii_digit())) =>
            {
                lexer.number()?
            }
            '.' => Kind::Dot,
            c if ident_start(c) => {
                lexer.identifier()?;
                Kind::Ident
            }
            _ => {
                lexer.advance()?;
                return Err(lexer.error(
                    start,
                    ErrorKind::Unsupported,
                    "character outside lexical profile",
                ));
            }
        };
        // Compound/word/quoted scanners already consumed their complete token.
        if !matches!(
            kind,
            Kind::Ident
                | Kind::QuotedName
                | Kind::String
                | Kind::Parameter
                | Kind::Integer
                | Kind::Float
        ) {
            lexer.advance()?;
        }
        let span = Span {
            start,
            end: lexer.position,
        };
        if tokens.len() == limits.tokens {
            return Err(ParseError::new(
                ErrorKind::Limit(LimitKind::Tokens),
                span,
                "token limit",
            ));
        }
        push(
            &mut tokens,
            Token {
                kind,
                span,
                decoded,
            },
            lexer.resources,
            span,
        )?;
    }
    Ok(tokens)
}

struct Lexer<'a, 'r> {
    source: &'a str,
    position: usize,
    resources: &'r mut dyn Resources,
}
impl Lexer<'_, '_> {
    fn peek(&self) -> Option<char> {
        self.source.get(self.position..)?.chars().next()
    }
    fn peek_next(&self) -> Option<char> {
        self.source.get(self.position..)?.chars().nth(1)
    }
    fn starts(&self, text: &str) -> bool {
        self.source
            .get(self.position..)
            .is_some_and(|tail| tail.starts_with(text))
    }
    fn advance(&mut self) -> Result<Option<char>, ParseError> {
        let next = self.peek();
        poll(
            self.resources,
            Span {
                start: self.position,
                end: self.position,
            },
        )?;
        if let Some(c) = next {
            self.position += c.len_utf8();
        }
        Ok(next)
    }
    fn error(&self, start: usize, kind: ErrorKind, message: &'static str) -> ParseError {
        ParseError::new(
            kind,
            Span {
                start,
                end: self.position,
            },
            message,
        )
    }
    fn identifier(&mut self) -> Result<(), ParseError> {
        while self
            .peek()
            .is_some_and(|c| ident_start(c) || c.is_ascii_digit())
        {
            self.advance()?;
        }
        Ok(())
    }
    fn number(&mut self) -> Result<Kind, ParseError> {
        let start = self.position;
        let mut float = false;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.advance()?;
        }
        if self.peek() == Some('.') && self.peek_next().is_some_and(|c| c.is_ascii_digit()) {
            float = true;
            self.advance()?;
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.advance()?;
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            float = true;
            self.advance()?;
            if self.peek() == Some('-') {
                self.advance()?;
            }
            if !self.peek().is_some_and(|c| c.is_ascii_digit()) {
                return Err(self.error(start, ErrorKind::InvalidLiteral, "invalid exponent"));
            }
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.advance()?;
            }
        }
        if self.peek().is_some_and(|c| ident_start(c) || !c.is_ascii()) {
            return Err(self.error(start, ErrorKind::Unsupported, "nondecimal numeric literal"));
        }
        if !float
            && self
                .source
                .get(start..self.position)
                .is_some_and(|s| s.len() > 1 && s.starts_with('0'))
        {
            return Err(self.error(start, ErrorKind::Unsupported, "octal numeric literal"));
        }
        Ok(if float { Kind::Float } else { Kind::Integer })
    }
    fn quoted(&mut self, quote: char) -> Result<String, ParseError> {
        let start = self.position;
        self.advance()?;
        let mut result = String::new();
        loop {
            let Some(mut c) = self.advance()? else {
                return Err(self.error(start, ErrorKind::Syntax, "unterminated quoted token"));
            };
            if c == quote {
                if quote == '`' && self.peek() == Some('`') {
                    self.advance()?;
                } else {
                    return Ok(result);
                }
            } else if c == '\\' && quote != '`' {
                let escape = self.position - 1;
                c = match self.advance()? {
                    Some('n') => '\n',
                    Some('r') => '\r',
                    Some('t') => '\t',
                    Some('b') => '\u{8}',
                    Some('f') => '\u{c}',
                    Some('\\') => '\\',
                    Some('\'') => '\'',
                    Some('"') => '"',
                    Some('u') => self.unicode(4, escape)?,
                    Some('U') => self.unicode(8, escape)?,
                    _ => {
                        return Err(self.error(
                            escape,
                            ErrorKind::InvalidLiteral,
                            "invalid string escape",
                        ));
                    }
                };
            }
            if quote == '`' && c == '\0' {
                return Err(self.error(start, ErrorKind::InvalidLiteral, "NUL in name"));
            }
            let spare = result.capacity() - result.len();
            if spare < c.len_utf8() {
                let additional = result.capacity().max(4);
                charge(
                    self.resources,
                    additional,
                    Span {
                        start,
                        end: self.position,
                    },
                )?;
                result.try_reserve_exact(spare + additional).map_err(|_| {
                    self.error(
                        start,
                        ErrorKind::Resource(ResourceError::Allocation),
                        "allocation failed",
                    )
                })?;
            }
            result.push(c);
        }
    }
    fn unicode(&mut self, digits: usize, start: usize) -> Result<char, ParseError> {
        let mut scalar = self.hex(digits, start)?;
        if digits == 4 && (0xd800..=0xdbff).contains(&scalar) {
            if !self.starts("\\u") {
                return Err(self.error(start, ErrorKind::InvalidLiteral, "missing low surrogate"));
            }
            self.advance()?;
            self.advance()?;
            let low = self.hex(4, start)?;
            if !(0xdc00..=0xdfff).contains(&low) {
                return Err(self.error(start, ErrorKind::InvalidLiteral, "invalid low surrogate"));
            }
            scalar = 0x10000 + ((scalar - 0xd800) << 10) + low - 0xdc00;
        }
        char::from_u32(scalar)
            .ok_or_else(|| self.error(start, ErrorKind::InvalidLiteral, "invalid Unicode scalar"))
    }
    fn hex(&mut self, digits: usize, start: usize) -> Result<u32, ParseError> {
        let mut value = 0u32;
        for _ in 0..digits {
            let digit = self
                .advance()?
                .and_then(|c| c.to_digit(16))
                .ok_or_else(|| {
                    self.error(start, ErrorKind::InvalidLiteral, "invalid Unicode escape")
                })?;
            value = value * 16 + digit;
        }
        Ok(value)
    }
}
fn ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}
