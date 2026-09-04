use crate::tower::TokenBatch;

/// ML runtime identity included in evidence and diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeIdentity {
    /// Stable runtime name.
    pub name: &'static str,
    /// Whether inference uses the GPU stream.
    pub gpu: bool,
}

/// One contiguous evaluated batch returned by a runtime.
#[derive(Debug)]
pub struct EmbeddingBatch {
    values: Vec<f32>,
    rows: usize,
    dims: usize,
}

impl EmbeddingBatch {
    /// Constructs a shape-checked contiguous batch.
    pub fn new(values: Vec<f32>, rows: usize, dims: usize) -> Result<Self, RuntimeError> {
        if rows == 0 || dims == 0 || rows.checked_mul(dims) != Some(values.len()) {
            return Err(RuntimeError::Shape(
                "embedding batch shape mismatch".to_owned(),
            ));
        }
        Ok(Self { values, rows, dims })
    }

    /// Returns the full contiguous batch view.
    #[must_use]
    pub fn values(&self) -> &[f32] {
        &self.values
    }

    /// Returns mutable contiguous values for normalization.
    #[must_use]
    pub(crate) fn values_mut(&mut self) -> &mut [f32] {
        &mut self.values
    }

    /// Returns the number of rows.
    #[must_use]
    pub const fn rows(&self) -> usize {
        self.rows
    }

    /// Returns the coordinates in each row.
    #[must_use]
    pub const fn dims(&self) -> usize {
        self.dims
    }

    /// Moves the complete contiguous allocation to the receiver.
    #[must_use]
    pub fn into_values(self) -> Vec<f32> {
        self.values
    }
}

/// Typed model-runtime failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeError {
    /// Model tokenizer rejected input or bundle tables.
    Tokenization(String),
    /// An MLX operation failed.
    Mlx(String),
    /// A required tensor was absent.
    MissingTensor(String),
    /// Tensor bytes or array dimensions disagreed with metadata.
    Shape(String),
    /// Persisted dtype is not implemented by the commit-1 runtime.
    UnsupportedDtype(String),
    /// The embed worker panicked while evaluating a submitted batch.
    WorkerPanicked,
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tokenization(detail) => write!(formatter, "tokenization failed: {detail}"),
            Self::Mlx(detail) => write!(formatter, "MLX failed: {detail}"),
            Self::MissingTensor(name) => write!(formatter, "required tensor {name} is absent"),
            Self::Shape(detail) => write!(formatter, "model shape mismatch: {detail}"),
            Self::UnsupportedDtype(name) => {
                write!(formatter, "tensor {name} has an unsupported dtype")
            }
            Self::WorkerPanicked => formatter.write_str("embed worker panicked"),
        }
    }
}

impl std::error::Error for RuntimeError {}

/// Swappable single-tower model runtime.
pub trait ModelRuntime {
    /// Evaluates one rectangular token batch.
    fn embed_batch(&mut self, tokens: &TokenBatch) -> Result<EmbeddingBatch, RuntimeError>;

    /// Evaluates a minimal warmup graph.
    fn warm(&mut self) -> Result<(), RuntimeError>;

    /// Returns the runtime identity used by the epoch declaration.
    fn identity(&self) -> RuntimeIdentity;
}

/// MLX GPU runtime.
pub mod mlx;

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn embedding_batches_and_runtime_errors_preserve_their_typed_contracts() {
        assert!(EmbeddingBatch::new(Vec::new(), 0, 1).is_err());
        assert!(EmbeddingBatch::new(vec![1.0], 1, 0).is_err());
        assert!(EmbeddingBatch::new(vec![1.0], 2, 1).is_err());
        let batch = EmbeddingBatch::new(vec![1.0, 2.0], 1, 2).expect("valid batch");
        assert_eq!(batch.values(), [1.0, 2.0]);
        assert_eq!(batch.rows(), 1);
        assert_eq!(batch.dims(), 2);
        for error in [
            RuntimeError::Tokenization("tokens".to_owned()),
            RuntimeError::Mlx("mlx".to_owned()),
            RuntimeError::MissingTensor("weight".to_owned()),
            RuntimeError::Shape("shape".to_owned()),
            RuntimeError::UnsupportedDtype("weight".to_owned()),
            RuntimeError::WorkerPanicked,
        ] {
            assert!(!error.to_string().is_empty());
        }
    }
}
