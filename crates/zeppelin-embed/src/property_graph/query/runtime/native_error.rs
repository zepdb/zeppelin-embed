use super::RuntimeError;
use crate::property_graph::query::expression::{ExpressionError, ExpressionFailure};
use crate::property_graph::query::plan::PlanError;
use crate::property_graph::storage::tree::directory::TreeError;

/// Complete native execution cause retained inside the graph engine.
#[derive(Debug)]
pub(crate) enum NativeExecutionError {
    Runtime(RuntimeError),
    Expression(ExpressionError),
    Plan(PlanError),
    Tree(TreeError),
}

impl From<RuntimeError> for NativeExecutionError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

impl From<ExpressionFailure> for NativeExecutionError {
    fn from(error: ExpressionFailure) -> Self {
        match error {
            ExpressionFailure::Runtime(error) => Self::Runtime(error),
            ExpressionFailure::Plan(error) => Self::Plan(error),
            ExpressionFailure::Tree(error) => Self::Tree(error),
        }
    }
}

impl From<ExpressionError> for NativeExecutionError {
    fn from(error: ExpressionError) -> Self {
        Self::Expression(error)
    }
}

impl From<PlanError> for NativeExecutionError {
    fn from(error: PlanError) -> Self {
        Self::Plan(error)
    }
}

impl From<TreeError> for NativeExecutionError {
    fn from(error: TreeError) -> Self {
        Self::Tree(error)
    }
}

impl std::fmt::Display for NativeExecutionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Runtime(error) => error.fmt(formatter),
            Self::Expression(error) => error.fmt(formatter),
            Self::Plan(error) => error.fmt(formatter),
            Self::Tree(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for NativeExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Runtime(error) => Some(error),
            Self::Expression(error) => Some(error),
            Self::Plan(error) => Some(error),
            Self::Tree(error) => Some(error),
        }
    }
}
