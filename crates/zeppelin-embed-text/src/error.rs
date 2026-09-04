use crate::bundle::BundleError;
use crate::runtime::RuntimeError;

/// Typed text API failure.
#[derive(Debug)]
pub enum TextError {
    /// The `.zem` artifact is absent, corrupt, or unsupported.
    Bundle(BundleError),
    /// Tokenization or model evaluation failed.
    Runtime(RuntimeError),
    /// A produced vector disagreed with the tower declaration.
    DimsMismatch {
        /// Bundle-declared dimensions.
        declared: u32,
        /// Runtime-produced dimensions.
        actual: usize,
    },
    /// A tower declared L2 normalization but produced a non-normalizable vector.
    NonUnitVector,
    /// A pipeline stage could not start or complete.
    Pipeline {
        /// Stable stage name.
        stage: &'static str,
        /// Failure detail.
        detail: String,
    },
    /// Core ingest rejected a generated mutation.
    Ingest(zeppelin_embed::ingest::IngestError),
    /// Core sealing rejected a generated boundary.
    Seal(zeppelin_embed::lifecycle::StoreError),
    /// Core vector query failed.
    Query(zeppelin_embed::lifecycle::QueryError),
    /// Core lexical query failed.
    Lexical(zeppelin_embed::ingest::StoreLexicalError),
    /// Core hybrid query failed.
    Hybrid(zeppelin_embed::fusion::FusionError),
    /// Store open or close failed.
    Store(zeppelin_embed::lifecycle::StoreError),
    /// The caller supplied an invalid option or document.
    InvalidInput(&'static str),
}

impl std::fmt::Display for TextError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bundle(error) => error.fmt(formatter),
            Self::Runtime(error) => error.fmt(formatter),
            Self::DimsMismatch { declared, actual } => {
                write!(
                    formatter,
                    "tower declares {declared} dimensions but produced {actual}"
                )
            }
            Self::NonUnitVector => write!(formatter, "tower produced a non-unit vector"),
            Self::Pipeline { stage, detail } => {
                write!(formatter, "{stage} pipeline failed: {detail}")
            }
            Self::Ingest(error) => error.fmt(formatter),
            Self::Seal(error) | Self::Store(error) => error.fmt(formatter),
            Self::Query(error) => error.fmt(formatter),
            Self::Lexical(error) => error.fmt(formatter),
            Self::Hybrid(error) => error.fmt(formatter),
            Self::InvalidInput(detail) => write!(formatter, "invalid text input: {detail}"),
        }
    }
}

impl std::error::Error for TextError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Bundle(error) => Some(error),
            Self::Runtime(error) => Some(error),
            Self::Ingest(error) => Some(error),
            Self::Seal(error) | Self::Store(error) => Some(error),
            Self::Query(error) => Some(error),
            Self::Lexical(error) => Some(error),
            Self::Hybrid(error) => Some(error),
            Self::DimsMismatch { .. }
            | Self::NonUnitVector
            | Self::Pipeline { .. }
            | Self::InvalidInput(_) => None,
        }
    }
}

impl From<BundleError> for TextError {
    fn from(error: BundleError) -> Self {
        Self::Bundle(error)
    }
}

impl From<RuntimeError> for TextError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn text_error_preserves_typed_sources_and_stable_context() {
        let bundle = TextError::from(BundleError::BundleDigest);
        assert!(bundle.source().is_some());
        assert!(bundle.to_string().contains("bundle digest"));
        let runtime = TextError::from(RuntimeError::Shape("wrong".to_owned()));
        assert!(runtime.source().is_some());
        assert!(runtime.to_string().contains("wrong"));
        for error in [
            TextError::DimsMismatch {
                declared: 8,
                actual: 4,
            },
            TextError::NonUnitVector,
            TextError::Pipeline {
                stage: "embed",
                detail: "closed".to_owned(),
            },
            TextError::InvalidInput("empty"),
        ] {
            assert!(error.source().is_none());
            assert!(!error.to_string().is_empty());
        }
        let ingest = TextError::Ingest(zeppelin_embed::ingest::IngestError::EmptyBatch);
        assert!(ingest.source().is_some());
        assert!(!ingest.to_string().is_empty());
    }
}
