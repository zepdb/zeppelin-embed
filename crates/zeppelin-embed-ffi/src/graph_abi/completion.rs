//! One synchronous graph-handle admission and precommit ABI completion adapter.
use super::*;
use crate::graph_result::PendingResponse;
use crate::graph_result::conversion::{GlobalWorkSlots, prepare_boundary_native};
use std::cell::{Cell, RefCell};
use zeppelin_embed::property_graph::query::completed::{
    CompletedError, CompletedGraphResult, GraphBoundary, GraphQueryError, ResultSource,
};
use zeppelin_embed::property_graph::query::runtime::{RuntimeContext, WorkCounters};

pub(super) struct Boundary<'a> {
    writer: &'a Mutex<()>,
    writer_guard: RefCell<Option<MutexGuard<'a, ()>>>,
    response_guard: RefCell<Option<std::sync::MutexGuard<'static, ()>>>,
    pending: Cell<Option<PendingResponse>>,
    work: Cell<Option<GlobalWorkSlots>>,
    error: RefCell<Option<FfiError>>,
    out: *mut ZeGraphResponse,
}
impl<'a> Boundary<'a> {
    pub(super) fn new(access: &'a Access<GraphHandleState, ()>, out: *mut ZeGraphResponse) -> Self {
        Self {
            writer: &access.writer,
            writer_guard: RefCell::new(None),
            response_guard: RefCell::new(None),
            pending: Cell::new(None),
            work: Cell::new(None),
            error: RefCell::new(None),
            out,
        }
    }
    pub(super) fn take_error(&self) -> Option<FfiError> {
        self.error.borrow_mut().take()
    }
    pub(super) fn publish(
        &self,
        result: &CompletedGraphResult,
    ) -> Result<ZeGraphResponse, FfiError> {
        let pending = self.pending.take().ok_or_else(|| {
            FfiError::new(
                ZeErrorCode::ZeErrInternal,
                "native completion did not prepare the ABI response",
            )
        })?;
        Ok(pending.publish_completed(result, outcome_of(result.metadata().outcome)))
    }
}
impl GraphBoundary for Boundary<'_> {
    fn classified(&self, writes: bool) -> Result<(), GraphQueryError> {
        if writes && self.writer_guard.borrow().is_none() {
            let guard = match self.writer.try_lock() {
                Ok(guard) => guard,
                Err(TryLockError::WouldBlock) => {
                    *self.error.borrow_mut() = Some(FfiError::new(
                        ZeErrorCode::ZeErrBusy,
                        "another FFI writer call is active on this handle",
                    ));
                    return Err(GraphQueryError::builder_rejected());
                }
                Err(TryLockError::Poisoned(_)) => {
                    *self.error.borrow_mut() = Some(FfiError::new(
                        ZeErrorCode::ZeErrSynchronization,
                        "per-handle writer mutex is poisoned",
                    ));
                    return Err(GraphQueryError::builder_rejected());
                }
            };
            *self.writer_guard.borrow_mut() = Some(guard);
        }
        if self.response_guard.borrow().is_none() {
            *self.response_guard.borrow_mut() = Some(response_gate());
        }
        #[cfg(feature = "abi-panic-probe")]
        crate::graph_deadline_test_support::classified();
        set_disposition(
            self.out,
            if writes {
                ZeGraphDisposition::ZeGraphDispositionIndeterminate
            } else {
                ZeGraphDisposition::ZeGraphDispositionNotApplicable
            },
        );
        Ok(())
    }
    fn prepare(
        &self,
        source: &dyn ResultSource,
        context: &mut RuntimeContext<'_, '_, '_>,
    ) -> Result<usize, CompletedError> {
        match prepare_boundary_native(&RESPONSES, source, context) {
            Ok((pending, work, bytes)) => {
                self.pending.set(Some(pending));
                self.work.set(Some(work));
                Ok(bytes)
            }
            Err(error) => {
                *self.error.borrow_mut() =
                    Some(producer_error(&ProducerError::Conversion(error), true));
                Err(CompletedError::Limit)
            }
        }
    }
    fn completed(&self, counters: WorkCounters, peak_query_bytes: usize) {
        if let Some(work) = self.work.take() {
            work.finalize(counters, peak_query_bytes);
        }
    }
}
impl Drop for Boundary<'_> {
    fn drop(&mut self) {
        drop(self.pending.take());
    }
}
