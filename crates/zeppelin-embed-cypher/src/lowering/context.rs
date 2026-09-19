use super::*;
use zeppelin_embed::property_graph::query::{
    QueryError,
    runtime::{RetainedView, RuntimeContext},
};
mod sealed {
    pub trait Sealed {}
    impl Sealed for super::ValueContext<'_> {}
    impl Sealed for super::RuntimeContext<'_, '_, '_> {}
}
/// The two actual context owners, sealed against arbitrary accounting adapters.
/// ValueContext is symbolic pre-admission preparation. RuntimeContext retains
/// its supplied view checks and all existing value/runtime counters through the
/// callback; neither implementation creates an account, budget, view or lease.
pub trait ReadContext<'v>: sealed::Sealed {
    #[doc(hidden)]
    fn value_context(&mut self) -> &mut ValueContext<'v>;
    #[doc(hidden)]
    fn preparation_control(&mut self) -> PreparationControl<'v> {
        let values = self.value_context();
        PreparationControl {
            caller: values.control(),
            retained: values.retained_view(),
        }
    }
    #[doc(hidden)]
    fn matches_memory(&self, _memory: &QueryMemory<'_>) -> bool {
        true
    }
}
impl<'v> ReadContext<'v> for ValueContext<'v> {
    fn value_context(&mut self) -> &mut ValueContext<'v> {
        self
    }
}
impl<'v> ReadContext<'v> for RuntimeContext<'v, '_, '_> {
    fn value_context(&mut self) -> &mut ValueContext<'v> {
        self.values()
    }
    fn matches_memory(&self, memory: &QueryMemory<'_>) -> bool {
        std::ptr::eq(self.memory(), memory)
    }
}
/// Non-owning check descriptor derived only from a sealed original context.
/// Fields are private; there is no caller constructor or independent policy.
#[doc(hidden)]
pub struct PreparationControl<'v> {
    caller: &'v QueryControl,
    retained: Option<&'v dyn RetainedView>,
}
impl PreparationControl<'_> {
    pub(super) fn checkpoint(&self) -> Result<(), ResourceError> {
        if let Some(retained) = self.retained {
            retained.check_active().map_err(resource_error)?;
        }
        self.caller.checkpoint().map_err(|e| match e {
            ControlError::Cancelled { .. } => ResourceError::Cancelled,
            ControlError::ReadCancelled { .. } => ResourceError::ReadCancelled,
            ControlError::Timeout { .. } => ResourceError::Timeout,
            _ => ResourceError::Control,
        })
    }
}
pub(super) fn resource_error(error: QueryError) -> ResourceError {
    match error {
        QueryError::Cancelled => ResourceError::Cancelled,
        QueryError::ReadCancelled => ResourceError::ReadCancelled,
        QueryError::Timeout => ResourceError::Timeout,
        QueryError::WorkLimit => ResourceError::WorkLimit,
        _ => ResourceError::Control,
    }
}
