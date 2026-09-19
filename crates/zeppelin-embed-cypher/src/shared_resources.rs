//! One scoped frontend reservation inside the runtime's actual query owner.
use crate::*;
use zeppelin_embed::lifecycle::{QueryControl, QueryError as ControlError};
use zeppelin_embed::property_graph::query::{
    plan::ParameterBinding,
    resources::{QueryExternalReservation, QueryMemory},
};
/// Reserved stack/control envelope for the depth-bounded parser/binder.
pub const COMPILER_SCRATCH_BYTES: usize = 65536;
struct SharedResources<'m, 'g, 'c> {
    reservation: QueryExternalReservation<'m, 'g>,
    control: &'c dyn Fn() -> Result<(), ResourceError>,
}
impl Resources for SharedResources<'_, '_, '_> {
    fn charge(&mut self, bytes: usize) -> Result<(), ResourceError> {
        self.checkpoint()?;
        self.reservation
            .reserve_additional(bytes)
            .map_err(|_| ResourceError::Memory)
    }
    fn checkpoint(&mut self) -> Result<(), ResourceError> {
        (self.control)()
    }
}
/// Complete private compiler invocation under one existing query/store budget.
/// The callback prepares lowering; it is not writer admission or execution.
/// Caller source/parameters are borrowed synchronously; their capacities are
/// not inferred or credited. Later execution requires actual retained owners.
/// All frontend backing drops before its grow-only reservation, even on error.
/// This guard is not a runtime prepayment capability: retained plan payloads
/// require separate real copies/owners under the same QueryMemory.
pub fn compile_in<T>(
    text: &str,
    parameters: &[ParameterBinding<'_>],
    limits: CompileLimits,
    memory: &QueryMemory<'_>,
    control: &QueryControl,
    consume: impl for<'query> FnOnce(BoundQuery<'query>) -> Result<T, ParseError>,
) -> Result<T, ParseError> {
    compile_checked_in(
        text,
        parameters,
        limits,
        memory,
        &|| {
            control.checkpoint().map_err(|error| match error {
                ControlError::Cancelled { .. } => ResourceError::Cancelled,
                ControlError::ReadCancelled { .. } => ResourceError::ReadCancelled,
                ControlError::Timeout { .. } => ResourceError::Timeout,
                _ => ResourceError::Control,
            })
        },
        consume,
    )
}

/// Same compiler reservation, with the originating context's close-first check.
/// Internal only: external callers retain the established compile_in interface.
pub(crate) fn compile_checked_in<T>(
    text: &str,
    parameters: &[ParameterBinding<'_>],
    limits: CompileLimits,
    memory: &QueryMemory<'_>,
    control: &dyn Fn() -> Result<(), ResourceError>,
    consume: impl for<'query> FnOnce(BoundQuery<'query>) -> Result<T, ParseError>,
) -> Result<T, ParseError> {
    limits.validate()?;
    let mut resources = SharedResources {
        reservation: memory.reserve_external_capacity().map_err(|_| {
            ParseError::new(
                ErrorKind::Resource(ResourceError::Memory),
                Span::default(),
                "shared compiler reservation unavailable",
            )
        })?,
        control,
    };
    resources
        .charge(
            COMPILER_SCRATCH_BYTES + std::mem::size_of::<SharedResources<'_, '_, '_>>()
                - std::mem::size_of::<QueryExternalReservation<'_, '_>>(),
        )
        .map_err(|kind| {
            ParseError::new(
                ErrorKind::Resource(kind),
                Span::default(),
                "compiler control/scratch reservation",
            )
        })?;
    let result = compile_with(text, parameters, limits, &mut resources, consume)?;
    resources.checkpoint().map_err(|kind| {
        ParseError::new(
            ErrorKind::Resource(kind),
            Span {
                start: 0,
                end: text.len(),
            },
            "compiler cancelled before handoff",
        )
    })?;
    Ok(result)
}
