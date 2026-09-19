use crate::{ErrorKind, ParseError, Span};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceError {
    Cancelled,
    Memory,
    Allocation,
}

/// Adapter seam for the execution layer's shared query account. Implementations
/// must fail explicitly; `checkpoint` is called within scanning and AST walks.
pub trait Resources {
    fn charge(&mut self, bytes: usize) -> Result<(), ResourceError>;
    fn checkpoint(&mut self) -> Result<(), ResourceError>;
}

/// Conservative allocation account, optionally using a caller-owned cancel flag.
pub struct Budget<'a> {
    maximum: usize,
    charged: usize,
    cancellation: Option<&'a AtomicBool>,
}
impl Default for Budget<'_> {
    fn default() -> Self {
        Self::new(24 * 1024 * 1024, None)
    }
}
impl<'a> Budget<'a> {
    pub fn new(maximum: usize, cancellation: Option<&'a AtomicBool>) -> Self {
        Self {
            maximum,
            charged: 0,
            cancellation,
        }
    }
    pub fn charged_bytes(&self) -> usize {
        self.charged
    }
}
impl Resources for Budget<'_> {
    fn charge(&mut self, bytes: usize) -> Result<(), ResourceError> {
        self.charged = self
            .charged
            .checked_add(bytes)
            .filter(|n| *n <= self.maximum)
            .ok_or(ResourceError::Memory)?;
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ResourceError> {
        if self
            .cancellation
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            Err(ResourceError::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompileLimits {
    pub text_bytes: usize,
    pub tokens: usize,
    pub ast_nodes: usize,
    pub depth: usize,
    pub parameters: usize,
    pub columns: usize,
    pub list_depth: usize,
    pub path_hops: u32,
}
impl Default for CompileLimits {
    fn default() -> Self {
        Self {
            text_bytes: 65536,
            tokens: 8192,
            ast_nodes: 4096,
            depth: 64,
            parameters: 256,
            columns: 256,
            list_depth: 16,
            path_hops: 16,
        }
    }
}
impl CompileLimits {
    pub(crate) fn validate(self) -> Result<(), ParseError> {
        let max = Self::default();
        if self.text_bytes > max.text_bytes
            || self.tokens > max.tokens
            || self.ast_nodes > max.ast_nodes
            || self.depth > max.depth
            || self.parameters > max.parameters
            || self.columns > max.columns
            || self.list_depth > max.list_depth
            || self.path_hops > max.path_hops
        {
            Err(ParseError::new(
                ErrorKind::InvalidLimits,
                Span::default(),
                "compile limits may only be tightened",
            ))
        } else {
            Ok(())
        }
    }
}

pub(crate) fn poll(resources: &mut dyn Resources, span: Span) -> Result<(), ParseError> {
    resources
        .checkpoint()
        .map_err(|error| ParseError::new(ErrorKind::Resource(error), span, "compiler interrupted"))
}
pub(crate) fn charge(
    resources: &mut dyn Resources,
    bytes: usize,
    span: Span,
) -> Result<(), ParseError> {
    resources.charge(bytes).map_err(|error| {
        ParseError::new(
            ErrorKind::Resource(error),
            span,
            "compiler allocation budget",
        )
    })
}
pub(crate) fn push<T>(
    values: &mut Vec<T>,
    value: T,
    resources: &mut dyn Resources,
    span: Span,
) -> Result<(), ParseError> {
    if values.len() == values.capacity() {
        let additional = values.capacity().max(1);
        let bytes = additional
            .checked_mul(std::mem::size_of::<T>())
            .ok_or_else(|| {
                ParseError::new(
                    ErrorKind::Resource(ResourceError::Memory),
                    span,
                    "allocation size overflow",
                )
            })?;
        charge(resources, bytes, span)?;
        values.try_reserve_exact(additional).map_err(|_| {
            ParseError::new(
                ErrorKind::Resource(ResourceError::Allocation),
                span,
                "allocation failed",
            )
        })?;
    }
    values.push(value);
    Ok(())
}
pub(crate) fn copy_string(
    text: &str,
    resources: &mut dyn Resources,
    span: Span,
) -> Result<String, ParseError> {
    charge(resources, text.len(), span)?;
    let mut result = String::new();
    result.try_reserve_exact(text.len()).map_err(|_| {
        ParseError::new(
            ErrorKind::Resource(ResourceError::Allocation),
            span,
            "allocation failed",
        )
    })?;
    result.push_str(text);
    Ok(result)
}
