//! Test-support-only observation of the table used by a real Store score.

use std::sync::{Arc, Mutex, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::scan::vector_fault::{
    VectorFaultEffect, VectorFaultKind, VectorFaultReceipt, VectorFaultSite, VectorOperation,
};

use super::{KernelBackendId, KernelBackendTag};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelOperationId {
    DotI8,
    HammingU1,
    DotF32,
    DotF16,
    DotI8Batch,
    HammingU1Batch,
    DotBit4,
    DotBit4Prepared,
    DotBit4Batch,
    ScoreBit4PreparedBatch,
    ScoreBit4Ptrs,
}

/// Exact result bits produced by the selected Store kernel invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KernelScoreValue {
    I32(i32),
    U32(u32),
    F32(u32),
    I32s(Vec<i32>),
    U32s(Vec<u32>),
    F32s(Vec<u32>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingScore {
    selected: KernelBackendId,
    kernel: KernelOperationId,
    work_items: u64,
    value: Option<KernelScoreValue>,
}

/// Fact-only observation emitted after a real public Store scoring call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KernelScoreObservation {
    backend: KernelBackendId,
    kernel: KernelOperationId,
    work_items: u64,
    value: KernelScoreValue,
    result_published: bool,
}

impl KernelScoreObservation {
    #[must_use]
    pub const fn backend(&self) -> KernelBackendId {
        self.backend
    }

    #[must_use]
    pub const fn kernel(&self) -> KernelOperationId {
        self.kernel
    }

    #[must_use]
    pub const fn work_items(&self) -> u64 {
        self.work_items
    }

    #[must_use]
    pub const fn value(&self) -> &KernelScoreValue {
        &self.value
    }

    #[must_use]
    pub const fn result_published(&self) -> bool {
        self.result_published
    }
}

#[derive(Debug)]
struct State {
    requested: Option<KernelBackendId>,
    forced_selected: Option<KernelBackendId>,
    case_id: u64,
    scope_open: bool,
    pending: Option<PendingScore>,
    observations: Vec<KernelScoreObservation>,
    receipts: Vec<VectorFaultReceipt>,
}

#[derive(Clone, Debug)]
pub struct KernelFaultController {
    state: Arc<Mutex<State>>,
}

impl KernelFaultController {
    #[must_use]
    pub fn forced_backend(requested: KernelBackendId, case_id: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                requested: Some(requested),
                forced_selected: None,
                case_id,
                scope_open: false,
                pending: None,
                observations: Vec::new(),
                receipts: Vec::new(),
            })),
        }
    }

    /// Observes the default table selected by a real public Store score.
    #[must_use]
    pub fn observing_store(case_id: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                requested: None,
                forced_selected: None,
                case_id,
                scope_open: false,
                pending: None,
                observations: Vec::new(),
                receipts: Vec::new(),
            })),
        }
    }

    pub(crate) fn initialize_for_store(&self) -> Result<KernelBackendId, super::KernelInitError> {
        let requested = self.state.lock().ok().and_then(|state| state.requested);
        let selected_backend = match requested {
            Some(requested) => super::dispatch::initialize_forced(requested)?,
            None => {
                super::initialize()?;
                super::KernelVariant::selected().backend_id()
            }
        };
        let selected_table = super::dispatch::active_table();
        if requested.is_some_and(|requested| selected_backend != requested)
            || KernelBackendId::from_tag(selected_table.backend) != selected_backend
        {
            return Err(super::KernelInitError::AlreadyInitialized {
                selected: selected_table.arm,
                requested: requested.map_or(selected_table.arm, KernelBackendId::arm),
            });
        }
        if let Ok(mut state) = self.state.lock() {
            state.forced_selected = Some(selected_backend);
        }
        if let Ok(mut installed) = installed().lock() {
            *installed = Some(self.clone());
        }
        Ok(selected_backend)
    }

    pub(crate) fn begin_store_scoring(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.scope_open = true;
            state.pending = None;
        }
    }

    pub(crate) fn finish_store_scoring(&self, result_published: bool) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.scope_open = false;
        let Some(pending) = state.pending.take() else {
            return;
        };
        let Some(requested_backend) = state.forced_selected else {
            return;
        };
        if pending.selected != requested_backend {
            return;
        }
        if let Some(value) = pending.value {
            state.observations.push(KernelScoreObservation {
                backend: pending.selected,
                kernel: pending.kernel,
                work_items: pending.work_items,
                value,
                result_published,
            });
        }
        let Some(requested) = state.requested else {
            return;
        };
        state.forced_selected = None;
        let case_id = state.case_id;
        state.receipts.push(VectorFaultReceipt::new(
            VectorOperation::KernelParity,
            VectorFaultKind::ForcedDispatchBackend,
            VectorFaultSite::KernelDispatchSelectedScoringTable,
            case_id,
            VectorFaultEffect::ForcedBackend {
                requested,
                selected: pending.selected,
                kernel: pending.kernel,
                work_items: pending.work_items,
            },
            result_published,
        ));
    }

    #[must_use]
    pub fn take_typed_receipts(&self) -> Vec<VectorFaultReceipt> {
        self.state
            .lock()
            .map(|mut state| std::mem::take(&mut state.receipts))
            .unwrap_or_default()
    }

    /// Drains Store scoring facts without creating a feature-fault receipt.
    #[must_use]
    pub fn take_observations(&self) -> Vec<KernelScoreObservation> {
        self.state
            .lock()
            .map(|mut state| std::mem::take(&mut state.observations))
            .unwrap_or_default()
    }
}

fn scoring_isolation() -> &'static RwLock<()> {
    static ISOLATION: OnceLock<RwLock<()>> = OnceLock::new();
    ISOLATION.get_or_init(|| RwLock::new(()))
}

fn scoring_reader() -> RwLockReadGuard<'static, ()> {
    match scoring_isolation().read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn scoring_writer() -> RwLockWriteGuard<'static, ()> {
    match scoring_isolation().write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

pub(crate) fn run_store_scoring<T, E>(
    controller: Option<&KernelFaultController>,
    score: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    if let Some(controller) = controller {
        let _guard = scoring_writer();
        controller.begin_store_scoring();
        let result = score();
        controller.finish_store_scoring(result.is_ok());
        result
    } else {
        let _guard = scoring_reader();
        score()
    }
}

fn installed() -> &'static Mutex<Option<KernelFaultController>> {
    static INSTALLED: OnceLock<Mutex<Option<KernelFaultController>>> = OnceLock::new();
    INSTALLED.get_or_init(|| Mutex::new(None))
}

pub(super) fn observe_selected_score(
    backend: KernelBackendTag,
    kernel: KernelOperationId,
    work_items: usize,
) {
    if kernel != KernelOperationId::ScoreBit4PreparedBatch {
        return;
    }
    let controller = installed()
        .lock()
        .ok()
        .and_then(|installed| installed.clone());
    let Some(controller) = controller else {
        return;
    };
    let Ok(mut state) = controller.state.lock() else {
        return;
    };
    if !state.scope_open || state.pending.is_some() {
        return;
    }
    let selected = KernelBackendId::from_tag(backend);
    if state.forced_selected != Some(selected) {
        return;
    }
    state.pending = Some(PendingScore {
        selected,
        kernel,
        work_items: work_items as u64,
        value: None,
    });
}

pub(super) fn observe_selected_result(value: KernelScoreValue) {
    if !matches!(value, KernelScoreValue::F32s(_)) {
        return;
    }
    let controller = installed()
        .lock()
        .ok()
        .and_then(|installed| installed.clone());
    let Some(controller) = controller else {
        return;
    };
    let Ok(mut state) = controller.state.lock() else {
        return;
    };
    let Some(pending) = state.pending.as_mut() else {
        return;
    };
    if pending.value.is_none() {
        pending.value = Some(value);
    }
}
