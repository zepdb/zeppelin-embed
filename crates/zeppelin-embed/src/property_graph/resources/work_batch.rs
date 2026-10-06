//! Caller-thread request deltas. Nested participants fold into their request;
//! only the outer owner acquires the diagnostic mutex, including on errors.
use crate::lifecycle::stats::{Accounting, GraphWorkKind, GraphWorkLedger};
use std::{cell::Cell, marker::PhantomData, rc::Rc, sync::Arc};

#[derive(Clone, Copy)]
struct Pending {
    owner: usize,
    delta: GraphWorkLedger,
}
thread_local! { static PENDING: Cell<Option<Pending>> = const { Cell::new(None) }; }

pub(crate) struct WorkBatch {
    accounting: Arc<Accounting>,
    previous: Option<Pending>,
    _caller_thread: PhantomData<Rc<()>>,
}
impl WorkBatch {
    pub(crate) fn new(accounting: &Arc<Accounting>) -> Self {
        let current = Pending {
            owner: Arc::as_ptr(accounting) as usize,
            delta: GraphWorkLedger::default(),
        };
        Self {
            accounting: Arc::clone(accounting),
            previous: PENDING.with(|pending| pending.replace(Some(current))),
            _caller_thread: PhantomData,
        }
    }
}
impl Drop for WorkBatch {
    fn drop(&mut self) {
        PENDING.with(|pending| {
            let current = pending.replace(self.previous);
            if let Some(current) = current {
                if let Some(mut parent) = self.previous
                    && parent.owner == current.owner
                {
                    parent.delta.merge(current.delta);
                    pending.set(Some(parent));
                } else {
                    self.accounting.merge_graph_work(current.delta);
                }
            }
        });
    }
}
pub(super) fn record(accounting: &Arc<Accounting>, kind: GraphWorkKind, units: u64) {
    let owner = Arc::as_ptr(accounting) as usize;
    let captured = PENDING.with(|pending| {
        if let Some(mut local) = pending.get()
            && local.owner == owner
        {
            local.delta.add(kind, units);
            pending.set(Some(local));
            true
        } else {
            false
        }
    });
    if !captured {
        let mut delta = GraphWorkLedger::default();
        delta.add(kind, units);
        accounting.merge_graph_work(delta);
    }
}
pub(super) fn record_delta(accounting: &Arc<Accounting>, delta: GraphWorkLedger) {
    let owner = Arc::as_ptr(accounting) as usize;
    let captured = PENDING.with(|pending| {
        if let Some(mut local) = pending.get()
            && local.owner == owner
        {
            local.delta.merge(delta);
            pending.set(Some(local));
            true
        } else {
            false
        }
    });
    if !captured {
        accounting.merge_graph_work(delta);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    #[test]
    fn ze76_request_work_flushes_once_including_failure_prefix() {
        let accounting = Arc::new(Accounting::new(1024, 1024));
        let before = accounting.ze76_work_merges();
        let failure: Result<(), ()> = (|| {
            let _request = WorkBatch::new(&accounting);
            record(&accounting, GraphWorkKind::StoragePagesDecoded, 1);
            {
                let _participant = WorkBatch::new(&accounting);
                record(&accounting, GraphWorkKind::StoragePagesDecoded, 1);
                record(&accounting, GraphWorkKind::CanonicalComparisonBytes, 7);
            }
            assert_eq!(accounting.ze76_work_merges(), before);
            assert_eq!(accounting.graph_work(), GraphWorkLedger::default());
            Err(())
        })();
        assert!(failure.is_err());
        assert_eq!(accounting.ze76_work_merges() - before, 1);
        let work = accounting.graph_work();
        assert_eq!(work.storage_pages_decoded, 2);
        assert_eq!(work.canonical_comparison_bytes, 7);
    }
}
