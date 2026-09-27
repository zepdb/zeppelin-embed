//! Bounded physical-reclamation proof types and durable sorted mark streams.
//! Only the native lifecycle coordinator may publish an intent or unlink.

mod codec;
mod trace;

use codec::ProofRole;
pub(crate) use codec::{ProtectedClass, ProtectedRecord, ProtectedValue};
pub(crate) use trace::{
    TraceEntrySource, TraceReferenceVisitor, trace_fence_entry_inner, trace_graph_bundle,
    trace_graph_state, trace_payload_references, trace_record_entry_inner,
};

use super::artifact::{ArtifactId, BlockKind};
use super::memory::{StorageMemory, StorageReservation};
use super::tree::directory::{TreeError, TreeResources};
use crate::property_graph::wal::{ArtifactDescriptor, RequiredRef};
use crate::property_graph::{GraphGeneration, StoreInstanceId};
use xxhash_rust::xxh3::Xxh3;

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static OMIT_MARK_ARTIFACT: std::cell::Cell<Option<ArtifactId>> = const {
        std::cell::Cell::new(None)
    };
    static OMITTED_MARK_EMISSIONS: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
    /// Mark pages a `DurableRunReader` read on this thread. ZE-163 gates it
    /// so membership stays a bounded descent instead of a run-long scan.
    static MARK_PAGE_READS: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn take_mark_page_reads_for_test() -> u64 {
    MARK_PAGE_READS.with(|count| count.replace(0))
}

#[cfg(any(test, feature = "test-support"))]
fn charge_mark_page_read() {
    MARK_PAGE_READS.with(|count| count.set(count.get().saturating_add(1)));
}

#[cfg(not(any(test, feature = "test-support")))]
const fn charge_mark_page_read() {}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn omit_mark_artifact_for_test(artifact: ArtifactId) {
    OMIT_MARK_ARTIFACT.with(|target| {
        assert!(target.replace(Some(artifact)).is_none());
    });
    OMITTED_MARK_EMISSIONS.with(|count| count.set(0));
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn take_omitted_mark_emissions_for_test() -> u64 {
    OMIT_MARK_ARTIFACT.with(|target| target.set(None));
    OMITTED_MARK_EMISSIONS.with(|count| count.replace(0))
}

pub(crate) const TRACE_OUTPUT_LIMIT: usize = 256;

/// The protected-root mark reads every live page, so its work grows with the
/// store and a fixed limit would cap the store size instead. Owner decision
/// 2026-09-20: the mark, its recovery retrace and the unlink resume carry no
/// work limit. Memory stays bounded, and every step still honors cancellation
/// and deadlines.
pub(crate) const MARK_WORK_LIMIT: u64 = u64::MAX;
pub(crate) const SPILL_CHUNK_LIMIT: usize = 256;
pub(crate) const MERGE_FAN_IN: usize = 8;
pub(crate) const MAX_RUN_LEVELS: usize = 64;
pub(crate) const MAX_CANDIDATES: usize = 256;
pub(crate) const PROTECTED_CAPTURE_PAGE_RECORDS: usize = codec::PROTECTED_STREAM_PAGE_RECORDS;
const PAGE_HEADER_BYTES: usize = 144;
const CHILD_BYTES: usize = 144;
const PAGE_BUFFER_BYTES: usize = PAGE_HEADER_BYTES + SPILL_CHUNK_LIMIT * 16;
const PROTECTED_PAGE_BYTES: usize = codec::PROTECTED_PAGE_HEADER_BYTES
    + codec::PROTECTED_STREAM_PAGE_RECORDS * codec::PROTECTED_STREAM_RECORD_BYTES;

pub(crate) fn encode_pending_intent(
    binding: SpillBinding,
    protected: DurableProtectedStream,
    mark: DurableRun,
    candidates: &[ArtifactDescriptor],
    partials: &[PartialTarget],
    output: &mut [u8],
) -> Result<(usize, u64), TreeError> {
    codec::encode_pending_intent(binding, protected, mark, candidates, partials, output)
}

pub(crate) fn validate_pending_intent(
    bytes: &[u8],
    binding: SpillBinding,
    protected: DurableProtectedStream,
    mark: DurableRun,
    candidates: &[ArtifactDescriptor],
    partials: &[PartialTarget],
    expected_digest: u64,
) -> Result<(), TreeError> {
    codec::validate_pending_intent(
        bytes,
        binding,
        protected,
        mark,
        candidates,
        partials,
        expected_digest,
    )
}

/// The encoded length of one pending intent, including its tagged partial
/// partition. Callers size their payload buffer with this.
pub(crate) fn pending_intent_bytes(candidates: usize, partials: usize) -> Result<usize, TreeError> {
    codec::pending_intent_bytes(candidates, partials)
}

/// The encoded length of one reclaim completion, including its tagged partial
/// partition.
pub(crate) fn reclaim_completion_bytes(
    targets: usize,
    partials: usize,
) -> Result<usize, TreeError> {
    codec::reclaim_completion_bytes(targets, partials)
}

/// Writes-owned exact I/O for one durable reclaim-stream page at a time.
pub(crate) trait SpillIo {
    fn append_page(
        &mut self,
        payload: &[u8],
        resources: &mut TreeResources<'_>,
    ) -> Result<RequiredRef, TreeError>;

    fn read_page(
        &self,
        reference: RequiredRef,
        output: &mut [u8],
        resources: &mut TreeResources<'_>,
    ) -> Result<usize, TreeError>;
}

/// Exact persisted association shared by every page in one completed mark.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SpillBinding {
    pub(crate) store: StoreInstanceId,
    pub(crate) session: ArtifactId,
    pub(crate) capture_generation: GraphGeneration,
    pub(crate) target_generation: GraphGeneration,
    pub(crate) sequence: u64,
    pub(crate) serial_fence: u64,
}

struct ChargedVec<'m, T> {
    values: Vec<T>,
    _reservation: StorageReservation<'m>,
    limit: usize,
}

impl<'m, T> ChargedVec<'m, T> {
    fn new(memory: &'m StorageMemory<'m>, limit: usize) -> Result<Self, TreeError> {
        let bytes = limit
            .checked_mul(std::mem::size_of::<T>())
            .ok_or(TreeError::Memory)?;
        let mut reservation = memory.reserve(bytes)?;
        let mut values = Vec::new();
        #[cfg(feature = "allocation-audit")]
        let allocated = crate::allocation_audit::attributed(|| values.try_reserve_exact(limit));
        #[cfg(not(feature = "allocation-audit"))]
        let allocated = values.try_reserve_exact(limit);
        allocated.map_err(|_| TreeError::Memory)?;
        reservation.resize(
            values
                .capacity()
                .checked_mul(std::mem::size_of::<T>())
                .ok_or(TreeError::Memory)?,
        )?;
        Ok(Self {
            values,
            _reservation: reservation,
            limit,
        })
    }

    fn push(&mut self, value: T) -> Result<(), TreeError> {
        if self.values.len() == self.limit {
            return Err(TreeError::Memory);
        }
        self.values.push(value);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DurableRun {
    pub(crate) root: RequiredRef,
    pub(crate) count: u64,
    pub(crate) digest: u64,
    pub(crate) first: ArtifactId,
    pub(crate) last: ArtifactId,
    pub(crate) binding: SpillBinding,
    height: u16,
}

impl DurableRun {
    /// Levels above the leaves. A membership descent reads at most one page
    /// per level plus the leaf, which is the bound ZE-163 gates.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) const fn height(&self) -> u16 {
        self.height
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DurableProtectedStream {
    pub(crate) head: RequiredRef,
    pub(crate) count: u64,
    pub(crate) digest: u64,
    pub(crate) pages: u64,
    pub(crate) binding: SpillBinding,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PendingIntentManifest {
    pub(crate) binding: SpillBinding,
    pub(crate) protected: DurableProtectedStream,
    pub(crate) mark: DurableRun,
    pub(crate) candidate_count: usize,
    pub(crate) partial_count: usize,
    pub(crate) digest: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CompletedIntentManifest {
    pub(crate) binding: SpillBinding,
    pub(crate) intent: RequiredRef,
    pub(crate) completed_count: usize,
    pub(crate) remaining_count: usize,
    pub(crate) partial_count: usize,
    pub(crate) digest: u64,
}

/// An interrupted native object creation that keeps an intact 96-byte header.
///
/// ZE-46 adopts a *complete* unregistered object by reading an
/// [`ArtifactDescriptor`] out of it and validating the whole file against that
/// descriptor's checksum. A partial create has no whole-file checksum, so it
/// can never become a descriptor and can never enter the inventory, the WAL
/// candidate lists, or the completed mark. ZE-165 therefore names it in its
/// own tagged partition of the role-5 pending and completion records: the
/// bytes that were actually observed, digested exactly as observed, plus the
/// same-store identity facts its own header declared.
///
/// `declared` is the length the header claims and is always strictly greater
/// than `observed`; that inequality *is* the interrupted-prefix
/// classification. A file whose header agrees with its length is a complete
/// object and belongs to ZE-46's adoption path instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PartialTarget {
    pub(crate) store: StoreInstanceId,
    pub(crate) artifact: ArtifactId,
    pub(crate) generation: GraphGeneration,
    pub(crate) serial: u64,
    /// Bytes the file actually held when it was observed.
    pub(crate) observed: u64,
    /// Bytes the intact header declared. Always greater than `observed`.
    pub(crate) declared: u64,
    /// xxh3-64 over exactly the `observed` bytes.
    pub(crate) digest: u64,
    pub(crate) family: u16,
    pub(crate) version: u16,
}

pub(crate) fn pending_intent_partial_at(
    bytes: &[u8],
    index: usize,
) -> Result<PartialTarget, TreeError> {
    codec::pending_intent_partial_at(bytes, index)
}

pub(crate) fn completed_intent_partial_at(
    bytes: &[u8],
    index: usize,
) -> Result<PartialTarget, TreeError> {
    codec::completed_intent_partial_at(bytes, index)
}

pub(crate) fn decode_pending_intent_manifest(
    bytes: &[u8],
) -> Result<PendingIntentManifest, TreeError> {
    codec::decode_pending_intent_manifest(bytes)
}

pub(crate) fn pending_intent_candidate_at(
    bytes: &[u8],
    index: usize,
) -> Result<ArtifactDescriptor, TreeError> {
    codec::pending_intent_candidate_at(bytes, index)
}

pub(crate) fn decode_completed_intent_manifest(
    bytes: &[u8],
) -> Result<CompletedIntentManifest, TreeError> {
    codec::decode_completed_intent_manifest(bytes)
}

pub(crate) fn completed_intent_candidate_at(
    bytes: &[u8],
    index: usize,
) -> Result<ArtifactDescriptor, TreeError> {
    codec::completed_intent_candidate_at(bytes, index)
}

pub(crate) fn encode_reclaim_completion(
    binding: SpillBinding,
    intent: RequiredRef,
    completed: &[ArtifactDescriptor],
    remaining: &[ArtifactDescriptor],
    partials: &[PartialTarget],
    output: &mut [u8],
) -> Result<(usize, u64), TreeError> {
    codec::encode_reclaim_completion(binding, intent, completed, remaining, partials, output)
}

/// Fixed-page typed capture stream. It retains at most32 records and one page
/// scratch while each completed page is flushed through the durable spill I/O.
pub(crate) struct ProtectedStreamBuilder<'m> {
    page: ChargedVec<'m, u8>,
    records: [Option<ProtectedRecord>; codec::PROTECTED_STREAM_PAGE_RECORDS],
    len: usize,
    head: Option<RequiredRef>,
    count: u64,
    digest: u64,
    pages: u64,
    binding: SpillBinding,
    complete: bool,
}

impl<'m> ProtectedStreamBuilder<'m> {
    pub(crate) fn new(
        memory: &'m StorageMemory<'m>,
        binding: SpillBinding,
    ) -> Result<Self, TreeError> {
        let mut page = ChargedVec::new(memory, PROTECTED_PAGE_BYTES)?;
        for _ in 0..PROTECTED_PAGE_BYTES {
            page.push(0)?;
        }
        Ok(Self {
            page,
            records: [None; codec::PROTECTED_STREAM_PAGE_RECORDS],
            len: 0,
            head: None,
            count: 0,
            digest: 0,
            pages: 0,
            binding,
            complete: false,
        })
    }

    pub(crate) fn emit<T: SpillIo>(
        &mut self,
        record: ProtectedRecord,
        sink: &mut T,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        if self.complete {
            return Err(TreeError::Invalid(
                "completed protected stream received another record",
            ));
        }
        *self.records.get_mut(self.len).ok_or(TreeError::Memory)? = Some(record);
        self.len += 1;
        if self.len == self.records.len() {
            self.flush(sink, resources)?;
        }
        Ok(())
    }

    pub(crate) fn finish<T: SpillIo>(
        &mut self,
        sink: &mut T,
        resources: &mut TreeResources<'_>,
    ) -> Result<DurableProtectedStream, TreeError> {
        if self.complete {
            return Err(TreeError::Invalid("protected stream completed twice"));
        }
        if self.len != 0 {
            self.flush(sink, resources)?;
        }
        self.complete = true;
        Ok(DurableProtectedStream {
            head: self
                .head
                .ok_or(TreeError::Invalid("empty protected stream"))?,
            count: self.count,
            digest: self.digest,
            pages: self.pages,
            binding: self.binding,
        })
    }

    fn flush<T: SpillIo>(
        &mut self,
        sink: &mut T,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        let records = self.records.get(..self.len).ok_or(TreeError::Memory)?;
        let mut compact = [ProtectedRecord {
            class: ProtectedClass::Proof,
            value: ProtectedValue::WalAuthority {
                identity: 1,
                first_sequence: 0,
                bytes: 1,
            },
        }; codec::PROTECTED_STREAM_PAGE_RECORDS];
        for (index, record) in records.iter().enumerate() {
            *compact.get_mut(index).ok_or(TreeError::Memory)? =
                record.ok_or(TreeError::Invalid("protected stream record hole"))?;
        }
        let (length, digest) = codec::encode_protected_page(
            self.binding,
            self.head,
            self.pages,
            self.count,
            self.digest,
            compact.get(..self.len).ok_or(TreeError::Memory)?,
            self.page.values.as_mut_slice(),
        )?;
        let head = sink.append_page(
            self.page.values.get(..length).ok_or(TreeError::Memory)?,
            resources,
        )?;
        self.count = self
            .count
            .checked_add(u64::try_from(self.len).map_err(|_| TreeError::Memory)?)
            .ok_or(TreeError::Memory)?;
        self.digest = digest;
        self.pages = self.pages.checked_add(1).ok_or(TreeError::Memory)?;
        self.head = Some(head);
        self.records.fill(None);
        self.len = 0;
        Ok(())
    }
}

pub(crate) fn validate_protected_stream<T: SpillIo>(
    stream: DurableProtectedStream,
    sink: &T,
    memory: &StorageMemory<'_>,
    resources: &mut TreeResources<'_>,
    mut visit: impl FnMut(ProtectedRecord, &mut TreeResources<'_>) -> Result<(), TreeError>,
) -> Result<(), TreeError> {
    let mut scratch = ChargedVec::new(memory, PROTECTED_PAGE_BYTES)?;
    for _ in 0..PROTECTED_PAGE_BYTES {
        scratch.push(0)?;
    }
    let mut next = Some(stream.head);
    let mut ordinal = stream.pages;
    let mut count = stream.count;
    let mut digest = stream.digest;
    while let Some(reference) = next {
        ordinal = ordinal
            .checked_sub(1)
            .ok_or(TreeError::Invalid("protected stream page count underflow"))?;
        let length = sink.read_page(reference, scratch.values.as_mut_slice(), resources)?;
        let bytes = scratch.values.get(..length).ok_or(TreeError::Memory)?;
        let page = codec::decode_protected_page(bytes, stream.binding)?;
        if page.ordinal != ordinal || page.cumulative_count != count || page.digest != digest {
            return Err(TreeError::Invalid("protected stream chain"));
        }
        for index in 0..page.count {
            visit(
                codec::protected_record_at(bytes, index, stream.binding)?,
                resources,
            )?;
        }
        count = count
            .checked_sub(u64::try_from(page.count).map_err(|_| TreeError::Memory)?)
            .ok_or(TreeError::Invalid("protected stream count underflow"))?;
        digest = page.prior_digest;
        next = page.previous;
    }
    if ordinal != 0 || count != 0 || digest != 0 {
        return Err(TreeError::Invalid("protected stream incomplete"));
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct IndexLevel {
    children: [Option<DurableRun>; MERGE_FAN_IN],
    len: usize,
}

impl IndexLevel {
    const EMPTY: Self = Self {
        children: [None; MERGE_FAN_IN],
        len: 0,
    };
}

/// Real durable fixed-chunk spill/merge builder. At most256 identities are held
/// in the sort chunk, at most8 child heads are retained at each of64 charged
/// levels, and every run page is an immutable participant block in the caller's
/// authentic prepared-object sink.
pub(crate) struct SpillMark<'m> {
    memory: &'m StorageMemory<'m>,
    chunk: ChargedVec<'m, ArtifactId>,
    page: ChargedVec<'m, u8>,
    levels: ChargedVec<'m, IndexLevel>,
    merge_levels: ChargedVec<'m, IndexLevel>,
    binding: SpillBinding,
    requested_chunk: usize,
    spill_runs: usize,
    merges: usize,
    max_batch: usize,
    complete: bool,
}

impl<'m> SpillMark<'m> {
    pub(crate) fn new(
        memory: &'m StorageMemory<'m>,
        binding: SpillBinding,
        requested_chunk: usize,
    ) -> Result<Self, TreeError> {
        if requested_chunk == 0 || requested_chunk > SPILL_CHUNK_LIMIT {
            return Err(TreeError::Memory);
        }
        let mut page = ChargedVec::new(memory, PAGE_BUFFER_BYTES)?;
        for _ in 0..PAGE_BUFFER_BYTES {
            page.push(0)?;
        }
        let mut levels = ChargedVec::new(memory, MAX_RUN_LEVELS)?;
        for _ in 0..MAX_RUN_LEVELS {
            levels.push(IndexLevel::EMPTY)?;
        }
        let mut merge_levels = ChargedVec::new(memory, MAX_RUN_LEVELS)?;
        for _ in 0..MAX_RUN_LEVELS {
            merge_levels.push(IndexLevel::EMPTY)?;
        }
        Ok(Self {
            memory,
            chunk: ChargedVec::new(memory, SPILL_CHUNK_LIMIT)?,
            page,
            levels,
            merge_levels,
            binding,
            requested_chunk,
            spill_runs: 0,
            merges: 0,
            max_batch: 0,
            complete: false,
        })
    }

    pub(crate) fn emit<T: SpillIo>(
        &mut self,
        artifact: ArtifactId,
        sink: &mut T,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        if self.complete {
            return Err(TreeError::Invalid(
                "completed mark received another reference",
            ));
        }
        #[cfg(any(test, feature = "test-support"))]
        if OMIT_MARK_ARTIFACT.with(|target| target.get() == Some(artifact)) {
            OMITTED_MARK_EMISSIONS.with(|count| count.set(count.get().saturating_add(1)));
            return Ok(());
        }
        self.chunk.push(artifact)?;
        self.max_batch = self.max_batch.max(self.chunk.values.len());
        if self.chunk.values.len() == self.requested_chunk {
            self.flush_chunk(sink, resources)?;
        }
        Ok(())
    }

    pub(crate) fn finish<T: SpillIo>(
        &mut self,
        sink: &mut T,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<DurableRun>, TreeError> {
        if self.complete {
            return Err(TreeError::Invalid("mark completed twice"));
        }
        if !self.chunk.values.is_empty() {
            self.flush_chunk(sink, resources)?;
        }
        let mut output = None;
        for level in 0..MAX_RUN_LEVELS {
            let Some(run) = self.take_level(level)? else {
                continue;
            };
            output = Some(match output {
                None => run,
                Some(previous) => self.merge_runs(previous, run, sink, resources)?,
            });
        }
        self.complete = true;
        Ok(output)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) const fn spill_runs(&self) -> usize {
        self.spill_runs
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) const fn merges(&self) -> usize {
        self.merges
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) const fn max_batch(&self) -> usize {
        self.max_batch
    }

    fn flush_chunk<T: SpillIo>(
        &mut self,
        sink: &mut T,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        self.chunk.values.sort_unstable();
        self.chunk.values.dedup();
        let run = write_single_run(
            self.binding,
            &mut self.page.values,
            &self.chunk.values,
            sink,
            resources,
        )?;
        self.chunk.values.clear();
        self.spill_runs = self.spill_runs.checked_add(1).ok_or(TreeError::Work)?;
        self.carry(0, run, sink, resources)
    }

    fn carry<T: SpillIo>(
        &mut self,
        level: usize,
        run: DurableRun,
        sink: &mut T,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        if level >= MAX_RUN_LEVELS {
            return Err(TreeError::Work);
        }
        let slot = self.levels.values.get_mut(level).ok_or(TreeError::Work)?;
        let Some(previous) = slot.children[0].take() else {
            slot.children[0] = Some(run);
            slot.len = 1;
            return Ok(());
        };
        slot.len = 0;
        let merged = self.merge_runs(previous, run, sink, resources)?;
        self.carry(level + 1, merged, sink, resources)
    }

    fn take_level(&mut self, level: usize) -> Result<Option<DurableRun>, TreeError> {
        let slot = self.levels.values.get_mut(level).ok_or(TreeError::Work)?;
        slot.len = 0;
        Ok(slot.children[0].take())
    }

    fn merge_runs<T: SpillIo>(
        &mut self,
        left: DurableRun,
        right: DurableRun,
        sink: &mut T,
        resources: &mut TreeResources<'_>,
    ) -> Result<DurableRun, TreeError> {
        self.merges = self.merges.checked_add(1).ok_or(TreeError::Work)?;
        let (count, digest, first, last) = {
            let mut reader = MergeReader::new(left, right, self.memory)?;
            let mut count = 0_u64;
            let mut hash = Xxh3::new();
            let mut first = None;
            let mut last = None;
            while let Some(id) = reader.next(sink, resources)? {
                resources.step(1)?;
                hash.update(&id.get().to_le_bytes());
                first.get_or_insert(id);
                last = Some(id);
                count = count.checked_add(1).ok_or(TreeError::Work)?;
            }
            (
                count,
                hash.digest(),
                first.ok_or(TreeError::Invalid("empty merged run"))?,
                last.ok_or(TreeError::Invalid("empty merged run"))?,
            )
        };
        let binding = self.binding;
        let levels = &mut self.merge_levels.values;
        levels.fill(IndexLevel::EMPTY);
        let mut builder = RunTreeBuilder::new(levels, binding, count, digest);
        let mut reader = MergeReader::new(left, right, self.memory)?;
        let mut ordinal = 0_u64;
        loop {
            while self.chunk.values.len() < SPILL_CHUNK_LIMIT {
                let Some(id) = reader.next(sink, resources)? else {
                    break;
                };
                self.chunk.values.push(id);
            }
            if self.chunk.values.is_empty() {
                break;
            }
            let payload = encode_leaf(
                &mut self.page.values,
                binding,
                &self.chunk.values,
                ordinal,
                count,
                digest,
            )?;
            let reference = sink.append_page(payload, resources)?;
            validate_page_reference(reference, binding.target_generation)?;
            let child = child_for_leaf(reference, binding, &self.chunk.values)?;
            self.chunk.values.clear();
            builder.push(child, sink, &mut self.page.values, resources)?;
            ordinal = ordinal.checked_add(1).ok_or(TreeError::Work)?;
        }
        let mut run = builder.finish(sink, &mut self.page.values, resources)?;
        run.count = count;
        run.digest = digest;
        run.first = first;
        run.last = last;
        Ok(run)
    }
}

fn write_single_run<T: SpillIo>(
    binding: SpillBinding,
    page: &mut [u8],
    ids: &[ArtifactId],
    sink: &mut T,
    resources: &mut TreeResources<'_>,
) -> Result<DurableRun, TreeError> {
    let first = *ids.first().ok_or(TreeError::Invalid("empty spill run"))?;
    let last = *ids.last().ok_or(TreeError::Invalid("empty spill run"))?;
    let digest = digest_ids(ids.iter().copied());
    let count = u64::try_from(ids.len()).map_err(|_| TreeError::Memory)?;
    let payload = encode_leaf(page, binding, ids, 0, count, digest)?;
    let root = sink.append_page(payload, resources)?;
    validate_page_reference(root, binding.target_generation)?;
    Ok(DurableRun {
        root,
        count,
        digest,
        first,
        last,
        binding,
        height: 0,
    })
}

fn digest_ids(ids: impl Iterator<Item = ArtifactId>) -> u64 {
    let mut hash = Xxh3::new();
    for id in ids {
        hash.update(&id.get().to_le_bytes());
    }
    hash.digest()
}

fn child_for_leaf(
    reference: RequiredRef,
    binding: SpillBinding,
    ids: &[ArtifactId],
) -> Result<DurableRun, TreeError> {
    Ok(DurableRun {
        root: reference,
        count: u64::try_from(ids.len()).map_err(|_| TreeError::Memory)?,
        digest: digest_ids(ids.iter().copied()),
        first: *ids.first().ok_or(TreeError::Invalid("empty mark leaf"))?,
        last: *ids.last().ok_or(TreeError::Invalid("empty mark leaf"))?,
        binding,
        height: 0,
    })
}

struct RunTreeBuilder<'a> {
    levels: &'a mut [IndexLevel],
    binding: SpillBinding,
    stream_total: u64,
    stream_digest: u64,
}

impl<'a> RunTreeBuilder<'a> {
    fn new(
        levels: &'a mut [IndexLevel],
        binding: SpillBinding,
        stream_total: u64,
        stream_digest: u64,
    ) -> Self {
        Self {
            levels,
            binding,
            stream_total,
            stream_digest,
        }
    }

    fn push<T: SpillIo>(
        &mut self,
        child: DurableRun,
        sink: &mut T,
        page: &mut [u8],
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        self.push_at(0, child, sink, page, resources)
    }

    fn push_at<T: SpillIo>(
        &mut self,
        level: usize,
        child: DurableRun,
        sink: &mut T,
        page: &mut [u8],
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        let slot = self.levels.get_mut(level).ok_or(TreeError::Work)?;
        *slot.children.get_mut(slot.len).ok_or(TreeError::Work)? = Some(child);
        slot.len += 1;
        if slot.len < MERGE_FAN_IN {
            return Ok(());
        }
        let parent = write_index(
            sink,
            self.binding,
            &slot.children,
            slot.len,
            page,
            resources,
        )?;
        *slot = IndexLevel::EMPTY;
        self.push_at(level + 1, parent, sink, page, resources)
    }

    fn finish<T: SpillIo>(
        &mut self,
        sink: &mut T,
        page: &mut [u8],
        resources: &mut TreeResources<'_>,
    ) -> Result<DurableRun, TreeError> {
        let top = self.levels.len().checked_sub(1).ok_or(TreeError::Work)?;
        for level in 0..top {
            let len = self.levels.get(level).ok_or(TreeError::Work)?.len;
            if len == 0 {
                continue;
            }
            let upper_empty = self
                .levels
                .get(level + 1..)
                .ok_or(TreeError::Work)?
                .iter()
                .all(|entry| entry.len == 0);
            let slot = self.levels.get_mut(level).ok_or(TreeError::Work)?;
            if len == 1 && upper_empty {
                let mut run = slot
                    .children
                    .first_mut()
                    .and_then(Option::take)
                    .ok_or(TreeError::Invalid("missing mark root"))?;
                run.count = self.stream_total;
                run.digest = self.stream_digest;
                return Ok(run);
            }
            let parent = write_index(sink, self.binding, &slot.children, len, page, resources)?;
            *slot = IndexLevel::EMPTY;
            self.push_at(level + 1, parent, sink, page, resources)?;
        }
        let slot = self.levels.get_mut(top).ok_or(TreeError::Work)?;
        if slot.len != 1 {
            return Err(TreeError::Work);
        }
        let mut root = slot
            .children
            .first_mut()
            .and_then(Option::take)
            .ok_or(TreeError::Invalid("missing mark root"))?;
        root.count = self.stream_total;
        root.digest = self.stream_digest;
        Ok(root)
    }
}

/// Copy `bytes` to `output[offset..]`; a short buffer is an error, never a panic.
fn put(output: &mut [u8], offset: usize, bytes: &[u8]) -> Result<(), TreeError> {
    let end = offset.checked_add(bytes.len()).ok_or(TreeError::Memory)?;
    output
        .get_mut(offset..end)
        .ok_or(TreeError::Memory)?
        .copy_from_slice(bytes);
    Ok(())
}

fn write_index<T: SpillIo>(
    sink: &mut T,
    binding: SpillBinding,
    children: &[Option<DurableRun>; MERGE_FAN_IN],
    len: usize,
    page: &mut [u8],
    resources: &mut TreeResources<'_>,
) -> Result<DurableRun, TreeError> {
    if len == 0 || len > MERGE_FAN_IN {
        return Err(TreeError::Invalid("mark index fan-in"));
    }
    let children = children
        .get(..len)
        .ok_or(TreeError::Invalid("mark index fan-in"))?;
    let first_child = children
        .first()
        .copied()
        .flatten()
        .ok_or(TreeError::Invalid("mark index child"))?;
    let last_child = children
        .last()
        .copied()
        .flatten()
        .ok_or(TreeError::Invalid("mark index child"))?;
    let height = first_child.height.checked_add(1).ok_or(TreeError::Work)?;
    let mut count = 0_u64;
    let mut hash = Xxh3::new();
    let mut previous = None;
    for child in children.iter().copied() {
        let child = child.ok_or(TreeError::Invalid("mark index child"))?;
        if child.binding != binding
            || child.height.checked_add(1) != Some(height)
            || previous.is_some_and(|last| last >= child.first)
        {
            return Err(TreeError::Invalid("mark index child order or height"));
        }
        previous = Some(child.last);
        count = count.checked_add(child.count).ok_or(TreeError::Work)?;
        hash.update(&child.digest.to_le_bytes());
        hash.update(&child.count.to_le_bytes());
    }
    let digest = hash.digest();
    let length = PAGE_HEADER_BYTES
        .checked_add(len.checked_mul(CHILD_BYTES).ok_or(TreeError::Memory)?)
        .ok_or(TreeError::Memory)?;
    let output = page.get_mut(..length).ok_or(TreeError::Memory)?;
    output.fill(0);
    encode_page_header(output, binding, 2, len, height, count, digest, 0)?;
    put(
        output,
        12,
        &u32::try_from(length)
            .map_err(|_| TreeError::Memory)?
            .to_le_bytes(),
    )?;
    put(output, 112, &first_child.first.get().to_le_bytes())?;
    put(output, 128, &last_child.last.get().to_le_bytes())?;
    for (index, child) in children.iter().copied().enumerate() {
        let child = child.ok_or(TreeError::Invalid("mark index child"))?;
        let start = PAGE_HEADER_BYTES + index * CHILD_BYTES;
        encode_required(
            child.root,
            output.get_mut(start..start + 96).ok_or(TreeError::Memory)?,
        )?;
        put(output, start + 96, &child.count.to_le_bytes())?;
        put(output, start + 104, &child.digest.to_le_bytes())?;
        put(output, start + 112, &child.first.get().to_le_bytes())?;
        put(output, start + 128, &child.last.get().to_le_bytes())?;
    }
    let root = sink.append_page(output, resources)?;
    validate_page_reference(root, binding.target_generation)?;
    Ok(DurableRun {
        root,
        count,
        digest,
        first: first_child.first,
        last: last_child.last,
        binding,
        height,
    })
}

fn encode_leaf<'a>(
    page: &'a mut [u8],
    binding: SpillBinding,
    ids: &[ArtifactId],
    ordinal: u64,
    total: u64,
    digest: u64,
) -> Result<&'a [u8], TreeError> {
    if ids.is_empty()
        || ids.len() > SPILL_CHUNK_LIMIT
        || !ids.is_sorted_by(|left, right| left < right)
    {
        return Err(TreeError::Invalid("mark leaf order or count"));
    }
    let length = PAGE_HEADER_BYTES
        .checked_add(ids.len().checked_mul(16).ok_or(TreeError::Memory)?)
        .ok_or(TreeError::Memory)?;
    let output = page.get_mut(..length).ok_or(TreeError::Memory)?;
    output.fill(0);
    encode_page_header(output, binding, 1, ids.len(), 0, total, digest, ordinal)?;
    put(
        output,
        12,
        &u32::try_from(length)
            .map_err(|_| TreeError::Memory)?
            .to_le_bytes(),
    )?;
    put(
        output,
        112,
        &ids.first()
            .ok_or(TreeError::Invalid("empty mark leaf"))?
            .get()
            .to_le_bytes(),
    )?;
    put(
        output,
        128,
        &ids.last()
            .ok_or(TreeError::Invalid("empty mark leaf"))?
            .get()
            .to_le_bytes(),
    )?;
    for (index, id) in ids.iter().enumerate() {
        let start = PAGE_HEADER_BYTES + index * 16;
        put(output, start, &id.get().to_le_bytes())?;
    }
    Ok(output)
}

#[allow(
    clippy::too_many_arguments,
    reason = "independent resource owners and lifetimes are explicit at this private seam"
)]
fn encode_page_header(
    output: &mut [u8],
    binding: SpillBinding,
    subtype: u16,
    entries: usize,
    height: u16,
    total: u64,
    digest: u64,
    ordinal: u64,
) -> Result<(), TreeError> {
    if output.len() < PAGE_HEADER_BYTES || !matches!(subtype, 1 | 2) {
        return Err(TreeError::Invalid("mark stream page header"));
    }
    put(output, 0, b"ZGCP")?;
    put(output, 4, &(ProofRole::CompletedMark as u16).to_le_bytes())?;
    put(output, 6, &1_u16.to_le_bytes())?;
    put(output, 8, &subtype.to_le_bytes())?;
    put(output, 10, &0_u16.to_le_bytes())?;
    put(output, 16, &binding.store.get().to_le_bytes())?;
    put(output, 32, &binding.session.get().to_le_bytes())?;
    put(output, 48, &binding.capture_generation.get().to_le_bytes())?;
    put(output, 56, &binding.target_generation.get().to_le_bytes())?;
    put(output, 64, &binding.sequence.to_le_bytes())?;
    put(output, 72, &binding.serial_fence.to_le_bytes())?;
    put(
        output,
        80,
        &u16::try_from(entries)
            .map_err(|_| TreeError::Memory)?
            .to_le_bytes(),
    )?;
    put(output, 82, &height.to_le_bytes())?;
    put(output, 84, &0_u32.to_le_bytes())?;
    put(output, 88, &total.to_le_bytes())?;
    put(output, 96, &digest.to_le_bytes())?;
    put(output, 104, &ordinal.to_le_bytes())?;
    Ok(())
}

#[derive(Clone, Copy)]
struct CursorFrame {
    reference: RequiredRef,
    next_child: usize,
    height: u16,
}

pub(crate) struct DurableRunReader<'m> {
    run: DurableRun,
    page: ChargedVec<'m, u8>,
    page_length: usize,
    frames: [Option<CursorFrame>; MAX_RUN_LEVELS],
    depth: usize,
    leaf: Option<RequiredRef>,
    leaf_index: usize,
    leaf_count: usize,
    expected_ordinal: u64,
    emitted: u64,
    previous: Option<ArtifactId>,
    hash: Xxh3,
    done: bool,
}

impl<'m> DurableRunReader<'m> {
    pub(crate) fn new(run: DurableRun, memory: &'m StorageMemory<'m>) -> Result<Self, TreeError> {
        let mut frames = [None; MAX_RUN_LEVELS];
        frames[0] = Some(CursorFrame {
            reference: run.root,
            next_child: 0,
            height: run.height,
        });
        let mut page = ChargedVec::new(memory, PAGE_BUFFER_BYTES)?;
        for _ in 0..PAGE_BUFFER_BYTES {
            page.push(0)?;
        }
        Ok(Self {
            run,
            page,
            page_length: 0,
            frames,
            depth: 1,
            leaf: None,
            leaf_index: 0,
            leaf_count: 0,
            expected_ordinal: 0,
            emitted: 0,
            previous: None,
            hash: Xxh3::new(),
            done: false,
        })
    }

    pub(crate) fn next(
        &mut self,
        source: &impl SpillIo,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<ArtifactId>, TreeError> {
        if self.done {
            return Ok(None);
        }
        loop {
            if self.leaf.is_some() {
                if self.leaf_index < self.leaf_count {
                    let payload = self
                        .page
                        .values
                        .get(..self.page_length)
                        .ok_or(TreeError::Invalid("mark page extent"))?;
                    validate_leaf_header(
                        payload,
                        self.expected_ordinal - 1,
                        self.run.count,
                        self.run.digest,
                        self.run.binding,
                    )?;
                    let id = ArtifactId::new(read_u128(
                        payload,
                        PAGE_HEADER_BYTES + self.leaf_index * 16,
                    )?)?;
                    if self.previous.is_some_and(|previous| previous >= id) {
                        return Err(TreeError::Invalid("mark run is not sorted unique"));
                    }
                    self.previous = Some(id);
                    self.hash.update(&id.get().to_le_bytes());
                    self.emitted = self.emitted.checked_add(1).ok_or(TreeError::Work)?;
                    self.leaf_index += 1;
                    return Ok(Some(id));
                }
                self.leaf = None;
            }
            if self.descend_next(source, resources)? {
                continue;
            }
            self.done = true;
            if self.emitted != self.run.count || self.hash.digest() != self.run.digest {
                return Err(TreeError::Invalid("mark run count or digest"));
            }
            return Ok(None);
        }
    }

    /// Exact membership of one id, by descending the child ranges of the run
    /// tree: one page read per level. Independent of the sequential cursor.
    pub(crate) fn contains(
        &mut self,
        artifact: ArtifactId,
        source: &impl SpillIo,
        resources: &mut TreeResources<'_>,
    ) -> Result<bool, TreeError> {
        let mut reference = self.run.root;
        let mut height = self.run.height;
        if artifact < self.run.first || artifact > self.run.last {
            return Ok(false);
        }
        for _ in 0..=MAX_RUN_LEVELS {
            charge_mark_page_read();
            let length = source.read_page(reference, &mut self.page.values, resources)?;
            let payload = self
                .page
                .values
                .get(..length)
                .ok_or(TreeError::Invalid("mark page extent"))?;
            let count = usize::from(read_u16(payload, 80)?);
            if validate_page_prefix(payload, self.run.binding)? == 1 {
                if height != 0
                    || count == 0
                    || count > SPILL_CHUNK_LIMIT
                    || payload.len()
                        != PAGE_HEADER_BYTES + count.checked_mul(16).ok_or(TreeError::Memory)?
                {
                    return Err(TreeError::Invalid("mark stream leaf geometry"));
                }
                let (mut low, mut high) = (0_usize, count);
                while low < high {
                    resources.step(1)?;
                    let middle = low + (high - low) / 2;
                    let id = ArtifactId::new(read_u128(payload, PAGE_HEADER_BYTES + middle * 16)?)?;
                    match id.cmp(&artifact) {
                        std::cmp::Ordering::Equal => return Ok(true),
                        std::cmp::Ordering::Less => low = middle + 1,
                        std::cmp::Ordering::Greater => high = middle,
                    }
                }
                return Ok(false);
            }
            if read_u16(payload, 82)? != height || height == 0 {
                return Err(TreeError::Invalid("mark index cursor geometry"));
            }
            let mut next = None;
            for index in 0..count {
                let child = decode_child(payload, index, self.run.binding)?;
                if child.height.checked_add(1) != Some(height) {
                    return Err(TreeError::Invalid("mark child height or depth"));
                }
                if child.first <= artifact && artifact <= child.last {
                    next = Some(child);
                    break;
                }
            }
            let Some(child) = next else {
                return Ok(false);
            };
            reference = child.root;
            height = child.height;
        }
        Err(TreeError::Invalid("mark child height or depth"))
    }

    fn descend_next(
        &mut self,
        source: &impl SpillIo,
        resources: &mut TreeResources<'_>,
    ) -> Result<bool, TreeError> {
        while self.depth > 0 {
            let frame_index = self.depth - 1;
            let mut frame = self
                .frames
                .get(frame_index)
                .copied()
                .flatten()
                .ok_or(TreeError::Invalid("mark cursor frame"))?;
            charge_mark_page_read();
            self.page_length =
                source.read_page(frame.reference, &mut self.page.values, resources)?;
            let payload = self
                .page
                .values
                .get(..self.page_length)
                .ok_or(TreeError::Invalid("mark page extent"))?;
            let subtype = validate_page_prefix(payload, self.run.binding)?;
            if subtype == 1 {
                if frame.height != 0 {
                    return Err(TreeError::Invalid("mark leaf height"));
                }
                *self
                    .frames
                    .get_mut(frame_index)
                    .ok_or(TreeError::Invalid("mark cursor frame"))? = None;
                self.depth -= 1;
                validate_leaf_header(
                    payload,
                    self.expected_ordinal,
                    self.run.count,
                    self.run.digest,
                    self.run.binding,
                )?;
                self.leaf = Some(frame.reference);
                self.leaf_index = 0;
                self.leaf_count = usize::from(read_u16(payload, 80)?);
                self.expected_ordinal = self
                    .expected_ordinal
                    .checked_add(1)
                    .ok_or(TreeError::Work)?;
                return Ok(true);
            }
            let count = usize::from(read_u16(payload, 80)?);
            let height = read_u16(payload, 82)?;
            if count == 0
                || count > MERGE_FAN_IN
                || height != frame.height
                || frame.next_child > count
            {
                return Err(TreeError::Invalid("mark index cursor geometry"));
            }
            if frame.next_child == count {
                *self
                    .frames
                    .get_mut(frame_index)
                    .ok_or(TreeError::Invalid("mark cursor frame"))? = None;
                self.depth -= 1;
                continue;
            }
            let child = decode_child(payload, frame.next_child, self.run.binding)?;
            frame.next_child += 1;
            *self
                .frames
                .get_mut(frame_index)
                .ok_or(TreeError::Invalid("mark cursor frame"))? = Some(frame);
            if child.height.checked_add(1) != Some(height) || self.depth == MAX_RUN_LEVELS {
                return Err(TreeError::Invalid("mark child height or depth"));
            }
            if self
                .frames
                .get(..self.depth)
                .ok_or(TreeError::Invalid("mark cursor depth"))?
                .iter()
                .flatten()
                .any(|ancestor| ancestor.reference == child.root)
            {
                return Err(TreeError::Invalid("mark run cycle"));
            }
            *self
                .frames
                .get_mut(self.depth)
                .ok_or(TreeError::Invalid("mark cursor depth"))? = Some(CursorFrame {
                reference: child.root,
                next_child: 0,
                height: child.height,
            });
            self.depth += 1;
        }
        Ok(false)
    }
}

struct MergeReader<'m> {
    left: DurableRunReader<'m>,
    right: DurableRunReader<'m>,
    left_next: Option<ArtifactId>,
    right_next: Option<ArtifactId>,
}

impl<'m> MergeReader<'m> {
    fn new(
        left: DurableRun,
        right: DurableRun,
        memory: &'m StorageMemory<'m>,
    ) -> Result<Self, TreeError> {
        Ok(Self {
            left: DurableRunReader::new(left, memory)?,
            right: DurableRunReader::new(right, memory)?,
            left_next: None,
            right_next: None,
        })
    }

    fn next(
        &mut self,
        source: &impl SpillIo,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<ArtifactId>, TreeError> {
        if self.left_next.is_none() {
            self.left_next = self.left.next(source, resources)?;
        }
        if self.right_next.is_none() {
            self.right_next = self.right.next(source, resources)?;
        }
        match (self.left_next, self.right_next) {
            (None, None) => Ok(None),
            (Some(left), None) => {
                self.left_next = None;
                Ok(Some(left))
            }
            (None, Some(right)) => {
                self.right_next = None;
                Ok(Some(right))
            }
            (Some(left), Some(right)) => match left.cmp(&right) {
                std::cmp::Ordering::Less => {
                    self.left_next = None;
                    Ok(Some(left))
                }
                std::cmp::Ordering::Greater => {
                    self.right_next = None;
                    Ok(Some(right))
                }
                std::cmp::Ordering::Equal => {
                    self.left_next = None;
                    self.right_next = None;
                    Ok(Some(left))
                }
            },
        }
    }
}

fn validate_page_prefix(payload: &[u8], binding: SpillBinding) -> Result<u16, TreeError> {
    if payload.len() < PAGE_HEADER_BYTES
        || payload.get(..4) != Some(b"ZGCP".as_slice())
        || read_u16(payload, 4)? != ProofRole::CompletedMark as u16
        || read_u16(payload, 6)? != 1
        || read_u16(payload, 10)? != 0
        || usize::try_from(read_u32(payload, 12)?).map_err(|_| TreeError::Memory)? != payload.len()
        || StoreInstanceId::new(read_u128(payload, 16)?)
            .map_err(|_| TreeError::Invalid("zero mark stream store"))?
            != binding.store
        || ArtifactId::new(read_u128(payload, 32)?)? != binding.session
        || GraphGeneration::new(read_u64(payload, 48)?) != binding.capture_generation
        || GraphGeneration::new(read_u64(payload, 56)?) != binding.target_generation
        || read_u64(payload, 64)? != binding.sequence
        || read_u64(payload, 72)? != binding.serial_fence
        || read_u32(payload, 84)? != 0
    {
        return Err(TreeError::Invalid("mark stream page header"));
    }
    match read_u16(payload, 8)? {
        value @ (1 | 2) => Ok(value),
        _ => Err(TreeError::Invalid("mark stream page subtype")),
    }
}

fn validate_leaf_header(
    payload: &[u8],
    ordinal: u64,
    total: u64,
    digest: u64,
    binding: SpillBinding,
) -> Result<(), TreeError> {
    let count = usize::from(read_u16(payload, 80)?);
    if validate_page_prefix(payload, binding)? != 1
        || count == 0
        || count > SPILL_CHUNK_LIMIT
        || read_u16(payload, 82)? != 0
        || read_u64(payload, 88)? != total
        || read_u64(payload, 96)? != digest
        || read_u64(payload, 104)? != ordinal
        || payload.len() != PAGE_HEADER_BYTES + count.checked_mul(16).ok_or(TreeError::Memory)?
    {
        return Err(TreeError::Invalid("mark stream leaf geometry"));
    }
    Ok(())
}

fn decode_child(
    payload: &[u8],
    index: usize,
    binding: SpillBinding,
) -> Result<DurableRun, TreeError> {
    let count = usize::from(read_u16(payload, 80)?);
    if validate_page_prefix(payload, binding)? != 2
        || count == 0
        || count > MERGE_FAN_IN
        || index >= count
        || payload.len() != PAGE_HEADER_BYTES + count * CHILD_BYTES
    {
        return Err(TreeError::Invalid("mark index page geometry"));
    }
    let start = PAGE_HEADER_BYTES + index * CHILD_BYTES;
    let root = decode_required(
        payload
            .get(start..start + 96)
            .ok_or(TreeError::Invalid("mark child reference"))?,
    )?;
    Ok(DurableRun {
        root,
        count: read_u64(payload, start + 96)?,
        digest: read_u64(payload, start + 104)?,
        first: ArtifactId::new(read_u128(payload, start + 112)?)?,
        last: ArtifactId::new(read_u128(payload, start + 128)?)?,
        binding,
        height: read_u16(payload, 82)?
            .checked_sub(1)
            .ok_or(TreeError::Invalid("mark child height"))?,
    })
}

fn validate_page_reference(
    reference: RequiredRef,
    generation: GraphGeneration,
) -> Result<(), TreeError> {
    if reference.object.generation != generation
        || reference.object.family != crate::format::FormatFamily::NativeGraphObject.id()
        || reference.object.version != 1
        || reference.object.artifact != reference.block.artifact
        || reference.block.kind != BlockKind::CommitParticipant
        || reference.block.version != 1
    {
        return Err(TreeError::Invalid("mark page required reference"));
    }
    Ok(())
}

fn encode_required(required: RequiredRef, output: &mut [u8]) -> Result<(), TreeError> {
    if output.len() != 96 {
        return Err(TreeError::Invalid("mark required width"));
    }
    let object = required.object;
    put(output, 0, &object.store.get().to_le_bytes())?;
    put(output, 16, &object.artifact.get().to_le_bytes())?;
    put(output, 32, &object.generation.get().to_le_bytes())?;
    put(output, 40, &object.serial.to_le_bytes())?;
    put(output, 48, &object.bytes.to_le_bytes())?;
    put(output, 52, &object.family.to_le_bytes())?;
    put(output, 54, &object.version.to_le_bytes())?;
    put(output, 56, &object.checksum.to_le_bytes())?;
    super::artifact::encode_reference(
        required.block,
        output.get_mut(64..96).ok_or(TreeError::Memory)?,
    )?;
    Ok(())
}

fn decode_required(bytes: &[u8]) -> Result<RequiredRef, TreeError> {
    if bytes.len() != 96 {
        return Err(TreeError::Invalid("mark required width"));
    }
    let object = ArtifactDescriptor {
        store: StoreInstanceId::new(read_u128(bytes, 0)?)
            .map_err(|_| TreeError::Invalid("zero mark page store"))?,
        artifact: ArtifactId::new(read_u128(bytes, 16)?)?,
        generation: GraphGeneration::new(read_u64(bytes, 32)?),
        serial: read_u64(bytes, 40)?,
        bytes: read_u32(bytes, 48)?,
        family: read_u16(bytes, 52)?,
        version: read_u16(bytes, 54)?,
        checksum: read_u64(bytes, 56)?,
    };
    let block = super::artifact::decode_reference(
        bytes
            .get(64..96)
            .ok_or(TreeError::Invalid("mark required block"))?,
    )?;
    let required = RequiredRef { object, block };
    validate_page_reference(required, object.generation)?;
    Ok(required)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, TreeError> {
    Ok(u16::from_le_bytes(read(bytes, offset)?))
}
fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, TreeError> {
    Ok(u32::from_le_bytes(read(bytes, offset)?))
}
fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, TreeError> {
    Ok(u64::from_le_bytes(read(bytes, offset)?))
}
fn read_u128(bytes: &[u8], offset: usize) -> Result<u128, TreeError> {
    Ok(u128::from_le_bytes(read(bytes, offset)?))
}
fn read<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], TreeError> {
    bytes
        .get(offset..offset.checked_add(N).ok_or(TreeError::Memory)?)
        .and_then(|value| value.try_into().ok())
        .ok_or(TreeError::Invalid("mark stream field extent"))
}
