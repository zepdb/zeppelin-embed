//! Test-support-only vector fault controller and production receipts.

use std::sync::{Arc, Mutex};

use crate::kernels::KernelBackendId;
use crate::kernels::vector_fault::KernelOperationId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorRowSource {
    Active,
    Sealed([u8; 16]),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorSearchTier {
    Exact,
    Scan,
    Graph,
    Auto,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MissingRescoreSite {
    ExactRescoreRows,
    QueryRescoreRows,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorCampaign {
    VectorExecution,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorOperation {
    KernelParity,
    Quantization,
    Rescore,
    RowIdentity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorFaultKind {
    ForcedDispatchBackend,
    CorruptCodesFactors,
    MissingRescoreRows,
    RowCountCancellation,
    AllocationDenial,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorFaultSite {
    KernelDispatchSelectedScoringTable,
    ScanBit4CodeView,
    ScanBit4FactorView,
    ScanInt8FactorView,
    ExactRescoreRows,
    QueryRescoreRows,
    ScoredVectorRow,
    SearchGlobalCandidates,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorQuantScheme {
    Bit4,
    Int8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorQuantField {
    OddPadding,
    Correction,
    Scale,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorAllocationSite {
    SearchGlobalCandidates,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VectorFaultEffect {
    ForcedBackend {
        requested: KernelBackendId,
        selected: KernelBackendId,
        kernel: KernelOperationId,
        work_items: u64,
    },
    CorruptedPayload {
        scheme: VectorQuantScheme,
        source: VectorRowSource,
        tier: VectorSearchTier,
        local_row: u32,
        field: VectorQuantField,
        byte_offset: u64,
        before_bits: u64,
        after_bits: u64,
    },
    MissingRescoreRows {
        segment: [u8; 16],
        expected_rows: u32,
        available_rows: u32,
        requested_tier: VectorSearchTier,
    },
    CancelledAfterRows {
        source: VectorRowSource,
        requested_tier: VectorSearchTier,
        local_row: u32,
        requested_rows: u32,
        observed_rows: u32,
    },
    AllocationDenied {
        component: VectorAllocationSite,
        requested_tier: VectorSearchTier,
        items: u64,
        bytes: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VectorFaultReceipt {
    campaign: VectorCampaign,
    operation: VectorOperation,
    fault: VectorFaultKind,
    site: VectorFaultSite,
    cardinality: u8,
    seed_case_id: u64,
    effect: VectorFaultEffect,
    result_published: bool,
}

/// One closed field in the fixed-width test-evidence receipt codec.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorReceiptEvidenceField {
    /// Campaign discriminator.
    Campaign,
    /// Operation discriminator.
    Operation,
    /// Fault discriminator.
    Fault,
    /// Production-site discriminator.
    Site,
    /// Required one-shot cardinality.
    Cardinality,
    /// Effect discriminator.
    Effect,
    /// Requested backend discriminator.
    RequestedBackend,
    /// Selected backend discriminator.
    SelectedBackend,
    /// Kernel discriminator.
    Kernel,
    /// Publication boolean.
    ResultPublished,
}

/// Typed refusal from the fixed-width test-evidence receipt codec.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorReceiptEvidenceError {
    /// The exact fixed byte width was not present.
    Length {
        /// Required byte count.
        expected: usize,
        /// Supplied byte count.
        actual: usize,
    },
    /// The receipt evidence magic or version is wrong.
    Magic,
    /// A closed field carried an unknown or inapplicable tag.
    InvalidTag {
        /// Field whose tag was rejected.
        field: VectorReceiptEvidenceField,
        /// Rejected byte value.
        actual: u8,
    },
    /// The receipt is not the forced-backend variant owned by this child codec.
    UnsupportedReceipt,
}

const FORCED_BACKEND_EVIDENCE_MAGIC: [u8; 8] = *b"ZEVREC01";
const FORCED_BACKEND_EVIDENCE_BYTES: usize = 34;

fn evidence_backend_tag(backend: KernelBackendId) -> u8 {
    match backend {
        KernelBackendId::Scalar => 0,
        KernelBackendId::NeonWiden => 1,
        KernelBackendId::NeonDotprodU4 => 2,
        KernelBackendId::NeonI8mm => 3,
        KernelBackendId::NeonDotprodU2 => 4,
        KernelBackendId::NeonDotprodU6 => 5,
        KernelBackendId::NeonDotprodU8 => 6,
        KernelBackendId::NeonDotprodU4Prefetch => 7,
        KernelBackendId::Avx2 => 8,
    }
}

fn evidence_backend_from_tag(
    tag: u8,
    field: VectorReceiptEvidenceField,
) -> Result<KernelBackendId, VectorReceiptEvidenceError> {
    match tag {
        0 => Ok(KernelBackendId::Scalar),
        1 => Ok(KernelBackendId::NeonWiden),
        2 => Ok(KernelBackendId::NeonDotprodU4),
        3 => Ok(KernelBackendId::NeonI8mm),
        4 => Ok(KernelBackendId::NeonDotprodU2),
        5 => Ok(KernelBackendId::NeonDotprodU6),
        6 => Ok(KernelBackendId::NeonDotprodU8),
        7 => Ok(KernelBackendId::NeonDotprodU4Prefetch),
        8 => Ok(KernelBackendId::Avx2),
        actual => Err(VectorReceiptEvidenceError::InvalidTag { field, actual }),
    }
}

struct EvidenceCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> EvidenceCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], VectorReceiptEvidenceError> {
        let end = self
            .offset
            .checked_add(N)
            .ok_or(VectorReceiptEvidenceError::Length {
                expected: FORCED_BACKEND_EVIDENCE_BYTES,
                actual: self.bytes.len(),
            })?;
        let source =
            self.bytes
                .get(self.offset..end)
                .ok_or(VectorReceiptEvidenceError::Length {
                    expected: FORCED_BACKEND_EVIDENCE_BYTES,
                    actual: self.bytes.len(),
                })?;
        let mut value = [0_u8; N];
        value.copy_from_slice(source);
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, VectorReceiptEvidenceError> {
        self.take::<1>().map(|value| value[0])
    }

    fn u64(&mut self) -> Result<u64, VectorReceiptEvidenceError> {
        self.take::<8>().map(u64::from_le_bytes)
    }
}

impl VectorFaultReceipt {
    pub(crate) const fn new(
        operation: VectorOperation,
        fault: VectorFaultKind,
        site: VectorFaultSite,
        seed_case_id: u64,
        effect: VectorFaultEffect,
        result_published: bool,
    ) -> Self {
        Self {
            campaign: VectorCampaign::VectorExecution,
            operation,
            fault,
            site,
            cardinality: 1,
            seed_case_id,
            effect,
            result_published,
        }
    }

    #[must_use]
    pub const fn campaign(&self) -> VectorCampaign {
        self.campaign
    }

    #[must_use]
    pub const fn operation(&self) -> VectorOperation {
        self.operation
    }

    #[must_use]
    pub const fn fault(&self) -> VectorFaultKind {
        self.fault
    }

    #[must_use]
    pub const fn site(&self) -> VectorFaultSite {
        self.site
    }

    #[must_use]
    pub const fn cardinality(&self) -> u8 {
        self.cardinality
    }

    #[must_use]
    pub const fn seed_case_id(&self) -> u64 {
        self.seed_case_id
    }

    #[must_use]
    pub const fn effect(&self) -> &VectorFaultEffect {
        &self.effect
    }

    #[must_use]
    pub const fn result_published(&self) -> bool {
        self.result_published
    }

    /// Serializes the real forced-backend receipt for one fresh child handoff.
    pub fn encode_forced_backend_test_evidence(
        &self,
    ) -> Result<Vec<u8>, VectorReceiptEvidenceError> {
        let VectorFaultEffect::ForcedBackend {
            requested,
            selected,
            kernel: KernelOperationId::ScoreBit4PreparedBatch,
            work_items,
        } = self.effect
        else {
            return Err(VectorReceiptEvidenceError::UnsupportedReceipt);
        };
        if self.campaign != VectorCampaign::VectorExecution
            || self.operation != VectorOperation::KernelParity
            || self.fault != VectorFaultKind::ForcedDispatchBackend
            || self.site != VectorFaultSite::KernelDispatchSelectedScoringTable
            || self.cardinality != 1
        {
            return Err(VectorReceiptEvidenceError::UnsupportedReceipt);
        }
        let mut bytes = Vec::with_capacity(FORCED_BACKEND_EVIDENCE_BYTES);
        bytes.extend_from_slice(&FORCED_BACKEND_EVIDENCE_MAGIC);
        bytes.extend_from_slice(&[1, 1, 1, 1, 1]);
        bytes.extend_from_slice(&self.seed_case_id.to_le_bytes());
        bytes.push(1);
        bytes.push(evidence_backend_tag(requested));
        bytes.push(evidence_backend_tag(selected));
        bytes.push(1);
        bytes.extend_from_slice(&work_items.to_le_bytes());
        bytes.push(u8::from(self.result_published));
        if bytes.len() != FORCED_BACKEND_EVIDENCE_BYTES {
            return Err(VectorReceiptEvidenceError::Length {
                expected: FORCED_BACKEND_EVIDENCE_BYTES,
                actual: bytes.len(),
            });
        }
        Ok(bytes)
    }

    /// Decodes one fixed-width fresh-child handoff into the actual receipt type.
    pub fn decode_forced_backend_test_evidence(
        bytes: &[u8],
    ) -> Result<Self, VectorReceiptEvidenceError> {
        if bytes.len() != FORCED_BACKEND_EVIDENCE_BYTES {
            return Err(VectorReceiptEvidenceError::Length {
                expected: FORCED_BACKEND_EVIDENCE_BYTES,
                actual: bytes.len(),
            });
        }
        let mut cursor = EvidenceCursor::new(bytes);
        if cursor.take::<8>()? != FORCED_BACKEND_EVIDENCE_MAGIC {
            return Err(VectorReceiptEvidenceError::Magic);
        }
        let campaign = cursor.u8()?;
        if campaign != 1 {
            return Err(VectorReceiptEvidenceError::InvalidTag {
                field: VectorReceiptEvidenceField::Campaign,
                actual: campaign,
            });
        }
        let operation = cursor.u8()?;
        if operation != 1 {
            return Err(VectorReceiptEvidenceError::InvalidTag {
                field: VectorReceiptEvidenceField::Operation,
                actual: operation,
            });
        }
        let fault = cursor.u8()?;
        if fault != 1 {
            return Err(VectorReceiptEvidenceError::InvalidTag {
                field: VectorReceiptEvidenceField::Fault,
                actual: fault,
            });
        }
        let site = cursor.u8()?;
        if site != 1 {
            return Err(VectorReceiptEvidenceError::InvalidTag {
                field: VectorReceiptEvidenceField::Site,
                actual: site,
            });
        }
        let cardinality = cursor.u8()?;
        if cardinality != 1 {
            return Err(VectorReceiptEvidenceError::InvalidTag {
                field: VectorReceiptEvidenceField::Cardinality,
                actual: cardinality,
            });
        }
        let seed_case_id = cursor.u64()?;
        let effect = cursor.u8()?;
        if effect != 1 {
            return Err(VectorReceiptEvidenceError::InvalidTag {
                field: VectorReceiptEvidenceField::Effect,
                actual: effect,
            });
        }
        let requested =
            evidence_backend_from_tag(cursor.u8()?, VectorReceiptEvidenceField::RequestedBackend)?;
        let selected =
            evidence_backend_from_tag(cursor.u8()?, VectorReceiptEvidenceField::SelectedBackend)?;
        let kernel = cursor.u8()?;
        if kernel != 1 {
            return Err(VectorReceiptEvidenceError::InvalidTag {
                field: VectorReceiptEvidenceField::Kernel,
                actual: kernel,
            });
        }
        let work_items = cursor.u64()?;
        let result_published = match cursor.u8()? {
            0 => false,
            1 => true,
            actual => {
                return Err(VectorReceiptEvidenceError::InvalidTag {
                    field: VectorReceiptEvidenceField::ResultPublished,
                    actual,
                });
            }
        };
        Ok(Self::new(
            VectorOperation::KernelParity,
            VectorFaultKind::ForcedDispatchBackend,
            VectorFaultSite::KernelDispatchSelectedScoringTable,
            seed_case_id,
            VectorFaultEffect::ForcedBackend {
                requested,
                selected,
                kernel: KernelOperationId::ScoreBit4PreparedBatch,
                work_items,
            },
            result_published,
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorFault {
    CorruptBit4OddPadding {
        source: VectorRowSource,
        local_row: u32,
    },
    CorruptBit4CorrectionNaN {
        source: VectorRowSource,
        local_row: u32,
    },
    CorruptInt8ScaleNaN {
        source: VectorRowSource,
        local_row: u32,
    },
    MissingRescoreRows {
        source: VectorRowSource,
        site: MissingRescoreSite,
        expected_rows: u32,
        available_rows: u32,
        tier: VectorSearchTier,
    },
    CancelAfterRows {
        source: VectorRowSource,
        requested_rows: u32,
        tier: VectorSearchTier,
    },
    DenyGlobalCandidateAllocation {
        items: u64,
        bytes: u64,
    },
}

/// Query-local scratch observations; separate from the frozen fault receipt codec.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ExactScanWork {
    pub row_indices_capacity: usize,
    pub coarse_scores_capacity: usize,
    pub exact_scores_capacity: usize,
    pub converted_candidates_capacity: usize,
    pub sorted_items: usize,
    pub collector_capacity: usize,
    pub scored_rows: usize,
    pub worst_score: Option<f32>,
}

#[derive(Clone, Copy, Debug)]
pub enum ExactWorkerFault {
    None,
    Panic,
    NonFinite { row_id: usize },
}

struct ExactWorkerHook(Arc<dyn Fn(usize) -> ExactWorkerFault + Send + Sync>);
impl std::fmt::Debug for ExactWorkerHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ExactWorkerHook")
    }
}

#[derive(Clone, Debug)]
pub struct ExactWorkerTiming {
    pub slot: usize,
    pub range: std::ops::Range<usize>,
    pub queue_wait: std::time::Duration,
    pub execution: std::time::Duration,
}

/// Explicit test-only row tracing; ordinary observation never stores row IDs.
#[derive(Clone, Debug)]
pub struct ExactPartitionReceipt {
    pub range: std::ops::Range<usize>,
    pub source: VectorRowSource,
    pub tier: VectorSearchTier,
    pub thread_id: std::thread::ThreadId,
    pub checked_rows: Vec<usize>,
}

pub(crate) struct ExactRowTrace {
    controller: VectorFaultController,
    receipt: ExactPartitionReceipt,
}

impl ExactRowTrace {
    pub(crate) fn checked(&mut self, row: usize) {
        self.receipt.checked_rows.push(row);
    }
}

impl Drop for ExactRowTrace {
    fn drop(&mut self) {
        if let Ok(mut state) = self.controller.state.lock() {
            state.exact_partitions.push(ExactPartitionReceipt {
                range: self.receipt.range.clone(),
                source: self.receipt.source,
                tier: self.receipt.tier,
                thread_id: self.receipt.thread_id,
                checked_rows: std::mem::take(&mut self.receipt.checked_rows),
            });
        }
    }
}

#[derive(Debug)]
struct State {
    fault: Option<VectorFault>,
    seed_case_id: u64,
    eligible_rows: u32,
    pending: Option<PendingReceipt>,
    receipts: Vec<VectorFaultReceipt>,
    exact_scans: Vec<ExactScanWork>,
    trace_exact: bool,
    exact_partitions: Vec<ExactPartitionReceipt>,
    exact_hook: Option<ExactWorkerHook>,
    exact_completion: Option<std::sync::mpsc::Sender<usize>>,
    deny_exact_reservation: bool,
    exact_reservation_denials: usize,
    panic_exact_caller: bool,
    exact_worker_timings: Vec<ExactWorkerTiming>,
}

#[derive(Debug)]
struct PendingReceipt {
    operation: VectorOperation,
    fault: VectorFaultKind,
    site: VectorFaultSite,
    effect: VectorFaultEffect,
}

#[derive(Clone, Debug)]
pub struct VectorFaultController {
    state: Arc<Mutex<State>>,
    cancel_after_rows: bool,
}

impl VectorFaultController {
    #[must_use]
    pub fn observe_only(seed_case_id: u64) -> Self {
        Self {
            cancel_after_rows: false,
            state: Arc::new(Mutex::new(State {
                fault: None,
                seed_case_id,
                eligible_rows: 0,
                pending: None,
                receipts: Vec::new(),
                exact_scans: Vec::new(),
                trace_exact: false,
                exact_partitions: Vec::new(),
                exact_hook: None,
                exact_completion: None,
                deny_exact_reservation: false,
                exact_reservation_denials: 0,
                panic_exact_caller: false,
                exact_worker_timings: Vec::new(),
            })),
        }
    }

    pub fn set_exact_worker_hook(
        &self,
        hook: impl Fn(usize) -> ExactWorkerFault + Send + Sync + 'static,
    ) {
        if let Ok(mut state) = self.state.lock() {
            state.exact_hook = Some(ExactWorkerHook(Arc::new(hook)));
        }
    }
    pub fn clear_exact_worker_hook(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.exact_hook = None;
            state.exact_completion = None;
        }
    }
    pub fn notify_exact_completions(&self, sender: std::sync::mpsc::Sender<usize>) {
        if let Ok(mut state) = self.state.lock() {
            state.exact_completion = Some(sender);
        }
    }
    pub fn deny_next_exact_reservation(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.deny_exact_reservation = true;
        }
    }
    pub fn panic_after_exact_submission(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.panic_exact_caller = true;
        }
    }
    #[must_use]
    pub fn take_exact_reservation_denials(&self) -> usize {
        self.state
            .lock()
            .map(|mut state| std::mem::take(&mut state.exact_reservation_denials))
            .unwrap_or(0)
    }
    #[must_use]
    pub fn take_exact_worker_timings(&self) -> Vec<ExactWorkerTiming> {
        self.state
            .lock()
            .map(|mut state| std::mem::take(&mut state.exact_worker_timings))
            .unwrap_or_default()
    }
    pub(crate) fn exact_worker_fault(&self, slot: usize) -> ExactWorkerFault {
        let hook = self
            .state
            .lock()
            .ok()
            .and_then(|state| state.exact_hook.as_ref().map(|hook| Arc::clone(&hook.0)));
        hook.map_or(ExactWorkerFault::None, |hook| hook(slot))
    }
    pub(crate) fn exact_completion_notifier(&self) -> Option<std::sync::mpsc::Sender<usize>> {
        self.state.lock().ok()?.exact_completion.clone()
    }
    pub(crate) fn deny_exact_reservation(&self) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        let deny = std::mem::take(&mut state.deny_exact_reservation);
        state.exact_reservation_denials += usize::from(deny);
        deny
    }
    pub(crate) fn panic_exact_caller(&self) -> bool {
        self.state
            .lock()
            .map(|mut state| std::mem::take(&mut state.panic_exact_caller))
            .unwrap_or(false)
    }
    pub(crate) fn record_exact_worker_timing(&self, timing: ExactWorkerTiming) {
        if let Ok(mut state) = self.state.lock() {
            state.exact_worker_timings.push(timing);
        }
    }

    #[must_use]
    pub fn trace_exact_partitions(seed_case_id: u64) -> Self {
        let controller = Self::observe_only(seed_case_id);
        if let Ok(mut state) = controller.state.lock() {
            state.trace_exact = true;
        }
        controller
    }

    pub(crate) fn has_exact_row_hooks(&self) -> bool {
        self.cancel_after_rows || self.state.lock().is_ok_and(|state| state.trace_exact)
    }

    #[must_use]
    pub fn take_exact_partitions(&self) -> Vec<ExactPartitionReceipt> {
        self.state
            .lock()
            .map(|mut state| std::mem::take(&mut state.exact_partitions))
            .unwrap_or_default()
    }

    pub(crate) fn trace_rows(
        &self,
        source: VectorRowSource,
        tier: VectorSearchTier,
        range: std::ops::Range<usize>,
    ) -> Option<ExactRowTrace> {
        if !self.state.lock().ok()?.trace_exact {
            return None;
        }
        Some(ExactRowTrace {
            controller: self.clone(),
            receipt: ExactPartitionReceipt {
                range,
                source,
                tier,
                thread_id: std::thread::current().id(),
                checked_rows: Vec::new(),
            },
        })
    }

    #[must_use]
    pub fn take_exact_scan_work(&self) -> Vec<ExactScanWork> {
        self.state
            .lock()
            .map(|mut state| std::mem::take(&mut state.exact_scans))
            .unwrap_or_default()
    }

    pub(crate) fn record_exact_scan(&self, work: ExactScanWork) {
        if let Ok(mut state) = self.state.lock() {
            state.exact_scans.push(work);
        }
    }

    #[must_use]
    pub fn armed(fault: VectorFault, seed_case_id: u64) -> Self {
        Self {
            cancel_after_rows: matches!(fault, VectorFault::CancelAfterRows { .. }),
            state: Arc::new(Mutex::new(State {
                fault: Some(fault),
                seed_case_id,
                eligible_rows: 0,
                pending: None,
                receipts: Vec::new(),
                exact_scans: Vec::new(),
                trace_exact: false,
                exact_partitions: Vec::new(),
                exact_hook: None,
                exact_completion: None,
                deny_exact_reservation: false,
                exact_reservation_denials: 0,
                panic_exact_caller: false,
                exact_worker_timings: Vec::new(),
            })),
        }
    }

    #[must_use]
    pub fn take_typed_receipts(&self) -> Vec<VectorFaultReceipt> {
        self.state
            .lock()
            .map(|mut state| std::mem::take(&mut state.receipts))
            .unwrap_or_default()
    }

    pub(crate) fn corrupt_bit4(
        &self,
        actual_source: VectorRowSource,
        actual_tier: VectorSearchTier,
        local_row: usize,
        codes: &mut [u8],
        factors: &mut crate::quant::Bit4Factors,
    ) {
        let Ok(local_row_u32) = u32::try_from(local_row) else {
            return;
        };
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let Some(fault) = state.fault else {
            return;
        };
        let (site, effect) = match fault {
            VectorFault::CorruptBit4OddPadding {
                source: planned_source,
                local_row,
            } if planned_source == actual_source && local_row == local_row_u32 => {
                let byte_offset = codes.len().saturating_sub(1);
                let Some(last) = codes.last_mut() else {
                    return;
                };
                let before = *last;
                *last |= 0x0f;
                (
                    VectorFaultSite::ScanBit4CodeView,
                    VectorFaultEffect::CorruptedPayload {
                        scheme: VectorQuantScheme::Bit4,
                        source: actual_source,
                        tier: actual_tier,
                        local_row,
                        field: VectorQuantField::OddPadding,
                        byte_offset: byte_offset as u64,
                        before_bits: u64::from(before),
                        after_bits: u64::from(*last),
                    },
                )
            }
            VectorFault::CorruptBit4CorrectionNaN {
                source: planned_source,
                local_row,
            } if planned_source == actual_source && local_row == local_row_u32 => {
                let mut fields = factors.persisted_fields();
                let before = fields[2].to_bits();
                fields[2] = f32::NAN;
                *factors =
                    crate::quant::Bit4Factors::from_persisted(fields[0], fields[1], fields[2]);
                (
                    VectorFaultSite::ScanBit4FactorView,
                    VectorFaultEffect::CorruptedPayload {
                        scheme: VectorQuantScheme::Bit4,
                        source: actual_source,
                        tier: actual_tier,
                        local_row,
                        field: VectorQuantField::Correction,
                        byte_offset: 8,
                        before_bits: u64::from(before),
                        after_bits: u64::from(f32::NAN.to_bits()),
                    },
                )
            }
            _ => return,
        };
        state.fault = None;
        state.pending = Some(PendingReceipt {
            operation: VectorOperation::Quantization,
            fault: VectorFaultKind::CorruptCodesFactors,
            site,
            effect,
        });
    }

    pub(crate) fn corrupt_int8(
        &self,
        actual_source: VectorRowSource,
        actual_tier: VectorSearchTier,
        local_row: usize,
        scale: &mut f32,
        _offset: &mut f32,
    ) {
        let Ok(local_row_u32) = u32::try_from(local_row) else {
            return;
        };
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let Some(VectorFault::CorruptInt8ScaleNaN {
            source: planned_source,
            local_row,
        }) = state.fault
        else {
            return;
        };
        if planned_source != actual_source || local_row != local_row_u32 {
            return;
        }
        let before = scale.to_bits();
        *scale = f32::NAN;
        state.fault = None;
        state.pending = Some(PendingReceipt {
            operation: VectorOperation::Quantization,
            fault: VectorFaultKind::CorruptCodesFactors,
            site: VectorFaultSite::ScanInt8FactorView,
            effect: VectorFaultEffect::CorruptedPayload {
                scheme: VectorQuantScheme::Int8,
                source: actual_source,
                tier: actual_tier,
                local_row,
                field: VectorQuantField::Scale,
                byte_offset: 0,
                before_bits: u64::from(before),
                after_bits: u64::from(f32::NAN.to_bits()),
            },
        });
    }

    pub(crate) fn after_eligible_row(
        &self,
        actual_source: VectorRowSource,
        actual_tier: VectorSearchTier,
        local_row: usize,
    ) -> bool {
        // Observation-only controllers must not serialize every scored row.
        // Fault kind is fixed at construction; consuming it only disarms it.
        if !self.cancel_after_rows {
            return false;
        }
        let Ok(local_row_u32) = u32::try_from(local_row) else {
            return false;
        };
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        let Some(VectorFault::CancelAfterRows {
            source: planned_source,
            requested_rows,
            tier: planned_tier,
        }) = state.fault
        else {
            return false;
        };
        if planned_source != actual_source || planned_tier != actual_tier {
            return false;
        }
        state.eligible_rows = state.eligible_rows.saturating_add(1);
        if state.eligible_rows != requested_rows {
            return false;
        }
        let observed = state.eligible_rows;
        state.fault = None;
        state.pending = Some(PendingReceipt {
            operation: VectorOperation::RowIdentity,
            fault: VectorFaultKind::RowCountCancellation,
            site: VectorFaultSite::ScoredVectorRow,
            effect: VectorFaultEffect::CancelledAfterRows {
                source: actual_source,
                requested_tier: actual_tier,
                local_row: local_row_u32,
                requested_rows,
                observed_rows: observed,
            },
        });
        true
    }

    /// Consumes the exact/graph missing-row fault at the owning Store view.
    #[must_use]
    pub(crate) fn missing_rescore_rows(
        &self,
        actual_source: VectorRowSource,
        actual_tier: VectorSearchTier,
        site: MissingRescoreSite,
        expected_rows: u32,
    ) -> Option<u32> {
        let Ok(mut state) = self.state.lock() else {
            return None;
        };
        let Some(VectorFault::MissingRescoreRows {
            source: planned_source,
            site: planned_site,
            expected_rows: planned_expected,
            available_rows,
            tier: planned_tier,
        }) = state.fault
        else {
            return None;
        };
        if actual_source != planned_source
            || actual_tier != planned_tier
            || site != planned_site
            || expected_rows != planned_expected
        {
            return None;
        }
        let VectorRowSource::Sealed(segment) = actual_source else {
            return None;
        };
        state.fault = None;
        state.pending = Some(PendingReceipt {
            operation: VectorOperation::Rescore,
            fault: VectorFaultKind::MissingRescoreRows,
            site: match site {
                MissingRescoreSite::ExactRescoreRows => VectorFaultSite::ExactRescoreRows,
                MissingRescoreSite::QueryRescoreRows => VectorFaultSite::QueryRescoreRows,
            },
            effect: VectorFaultEffect::MissingRescoreRows {
                segment,
                expected_rows,
                available_rows,
                requested_tier: actual_tier,
            },
        });
        Some(available_rows)
    }

    /// Consumes the named global candidate-reserve denial.
    #[must_use]
    pub(crate) fn deny_global_candidate_allocation(
        &self,
        actual_tier: VectorSearchTier,
        items: u64,
        bytes: u64,
    ) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        let Some(VectorFault::DenyGlobalCandidateAllocation {
            items: planned_items,
            bytes: planned_bytes,
        }) = state.fault
        else {
            return false;
        };
        if items != planned_items || bytes != planned_bytes {
            return false;
        }
        state.fault = None;
        state.pending = Some(PendingReceipt {
            operation: VectorOperation::RowIdentity,
            fault: VectorFaultKind::AllocationDenial,
            site: VectorFaultSite::SearchGlobalCandidates,
            effect: VectorFaultEffect::AllocationDenied {
                component: VectorAllocationSite::SearchGlobalCandidates,
                requested_tier: actual_tier,
                items,
                bytes,
            },
        });
        true
    }

    /// Finalizes the one consumed action at the owning public Store operation.
    pub(crate) fn finalize_search(&self, result_published: bool) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let Some(pending) = state.pending.take() else {
            return;
        };
        let seed_case_id = state.seed_case_id;
        state.receipts.push(VectorFaultReceipt::new(
            pending.operation,
            pending.fault,
            pending.site,
            seed_case_id,
            pending.effect,
            result_published,
        ));
    }
}

#[cfg(test)]
mod receipt_evidence_tests {
    use super::*;

    #[test]
    fn forced_backend_receipt_evidence_rejects_mutated_campaign_tag() {
        let receipt = VectorFaultReceipt::new(
            VectorOperation::KernelParity,
            VectorFaultKind::ForcedDispatchBackend,
            VectorFaultSite::KernelDispatchSelectedScoringTable,
            0x2401,
            VectorFaultEffect::ForcedBackend {
                requested: KernelBackendId::Scalar,
                selected: KernelBackendId::Scalar,
                kernel: KernelOperationId::ScoreBit4PreparedBatch,
                work_items: 9,
            },
            true,
        );
        let encoded = receipt.encode_forced_backend_test_evidence();
        assert!(
            encoded.is_ok(),
            "encode forced-backend receipt: {encoded:?}"
        );
        let mut bytes = encoded.unwrap_or_default();
        assert_eq!(
            VectorFaultReceipt::decode_forced_backend_test_evidence(&bytes),
            Ok(receipt)
        );
        assert!(bytes.len() > FORCED_BACKEND_EVIDENCE_MAGIC.len());
        if let Some(campaign_tag) = bytes.get_mut(FORCED_BACKEND_EVIDENCE_MAGIC.len()) {
            *campaign_tag = 0xff;
        }
        assert_eq!(
            VectorFaultReceipt::decode_forced_backend_test_evidence(&bytes),
            Err(VectorReceiptEvidenceError::InvalidTag {
                field: VectorReceiptEvidenceField::Campaign,
                actual: 0xff,
            })
        );
    }
}
