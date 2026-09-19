//! Bounded, store-independent syntax frontend for Zeppelin's Cypher profile.
//!
//! This internal workspace crate does not execute queries. Parsing is not binding,
//! validation of graph types, or an openCypher conformance claim. Its syntax API is
//! an implementation seam, not the release's reusable prepared-query API.
#![deny(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented,
    clippy::unwrap_used,
    unsafe_code
)]

mod ast;
mod binding;
mod lexer;
mod lowering;
mod parser;
mod resources;
mod shared_resources;

pub use ast::*;
pub use binding::*;
pub use lowering::{LoweredRead, PreparationControl, ReadColumn, ReadContext, compile_read_in};
pub use resources::*;
pub use shared_resources::{COMPILER_SCRATCH_BYTES, compile_in};

/// A half-open UTF-8 byte range in the original query.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// Parser failures; semantic binding errors belong to the binder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Syntax,
    Unsupported,
    InvalidLiteral,
    InvalidRange,
    DuplicateProperty,
    InvalidLimits,
    UnknownVariable,
    DuplicateVariable,
    Parameter,
    Type,
    RelationshipUniqueness,
    DeletedEntity,
    SearchContext,
    BindingInvariant,
    Plan(zeppelin_embed::property_graph::query::plan::PlanError),
    Limit(LimitKind),
    Resource(ResourceError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitKind {
    TextBytes,
    Tokens,
    AstNodes,
    Depth,
    Parameters,
    Columns,
    ListDepth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub kind: ErrorKind,
    pub span: Span,
    pub message: &'static str,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} at bytes {}..{}",
            self.message, self.span.start, self.span.end
        )
    }
}
impl std::error::Error for ParseError {}

impl ParseError {
    pub(crate) fn new(kind: ErrorKind, span: Span, message: &'static str) -> Self {
        Self {
            kind,
            span,
            message,
        }
    }
    /// Render a position lazily, with bounded work and no auxiliary allocation.
    /// Lines and Unicode scalar columns are one-based. Invalid foreign spans fail.
    pub fn location(
        &self,
        text: &str,
        resources: &mut dyn Resources,
    ) -> Result<(usize, usize), Self> {
        let prefix = text
            .get(..self.span.start)
            .ok_or_else(|| Self::new(ErrorKind::Syntax, self.span, "invalid source span"))?;
        let (mut line, mut column) = (1, 1);
        for c in prefix.chars() {
            poll(resources, self.span)?;
            if c == '\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
        }
        Ok((line, column))
    }
}

/// Parse one complete statement under the first-release limits.
pub fn parse(text: &str) -> Result<Ast, ParseError> {
    parse_with(text, CompileLimits::default(), &mut Budget::default())
}

/// Parse with tighter limits and the caller's shared allocation/cancellation account.
/// All owned allocations, including scratch capacity, are charged before reservation.
/// Charges are conservative cumulative reservations, never hidden extra allowance.
pub fn parse_with(
    text: &str,
    limits: CompileLimits,
    resources: &mut dyn Resources,
) -> Result<Ast, ParseError> {
    limits.validate()?;
    let span = Span {
        start: 0,
        end: text.len(),
    };
    poll(resources, span)?;
    if text.len() > limits.text_bytes {
        return Err(ParseError::new(
            ErrorKind::Limit(LimitKind::TextBytes),
            span,
            "query byte limit",
        ));
    }
    let source = copy_string(text, resources, span)?;
    let tokens = lexer::tokenize(&source, limits, resources)?;
    parser::parse(source, tokens, limits, resources)
}

/// Tooling byte seam: invalid UTF-8 is rejected before text processing.
pub fn parse_bytes(
    text: &[u8],
    limits: CompileLimits,
    resources: &mut dyn Resources,
) -> Result<Ast, ParseError> {
    limits.validate()?;
    poll(
        resources,
        Span {
            start: 0,
            end: text.len(),
        },
    )?;
    if text.len() > limits.text_bytes {
        return Err(ParseError::new(
            ErrorKind::Limit(LimitKind::TextBytes),
            Span {
                start: 0,
                end: text.len(),
            },
            "query byte limit",
        ));
    }
    let text = std::str::from_utf8(text).map_err(|error| {
        ParseError::new(
            ErrorKind::Syntax,
            Span {
                start: error.valid_up_to(),
                end: error.valid_up_to()
                    + error
                        .error_len()
                        .unwrap_or(text.len() - error.valid_up_to()),
            },
            "invalid UTF-8",
        )
    })?;
    parse_with(text, limits, resources)
}
