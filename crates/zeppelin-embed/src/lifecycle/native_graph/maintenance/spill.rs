use super::super::persistence::{encode_framed, next_artifact, write_new, zeroed};
use super::super::{
    NativeGraphError, NativeProtectedRoots, NativeReadLease, NativeSpillRegistration,
};
use crate::lifecycle::{QueryControl, Store};
use crate::property_graph::storage::NativePreparationSource;
use crate::property_graph::storage::artifact::{ArtifactIdentity, Block, BlockKind, ContainerKind};
use crate::property_graph::storage::memory::{StorageBuffer, StorageMemory};
use crate::property_graph::storage::reclaim::{
    DurableProtectedStream, DurableRun, DurableRunReader, SpillBinding, SpillIo,
    validate_protected_stream,
};
use crate::property_graph::storage::tree::directory::{TreeError, TreeResources};
use crate::property_graph::wal::{ArtifactDescriptor, RequiredRef};
use std::cell::{Cell, RefCell};

const ALLOCATION_PAGE_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct NativeSpillStats {
    pub(crate) created_objects: u64,
    pub(crate) disk_bytes: u64,
    pub(crate) maximum_encoded_backing: usize,
    pub(crate) charged_peak_bytes: usize,
    pub(crate) read_windows: u64,
    pub(crate) maximum_mapped_window: u64,
    pub(crate) released_each_read_window: bool,
    pub(crate) allocation_head: Option<RequiredRef>,
}

/// Writes-owned durable sink for one reclaim stream. It retains one ownership
/// head and two precreate descriptors; completed object bytes are released after
/// each Full file/directory sync pair.
pub(super) struct NativeSpillWriter<'a, 'm> {
    store: &'a Store,
    lease: &'a NativeReadLease,
    source: NativePreparationSource<'a, 'm>,
    memory: &'m StorageMemory<'m>,
    control: &'a QueryControl,
    binding: SpillBinding,
    registration: NativeSpillRegistration,
    allocation_head: Option<RequiredRef>,
    allocation_ordinal: u64,
    created_objects: u64,
    disk_bytes: u64,
    disk_limit: u64,
    maximum_encoded_backing: usize,
    read_windows: Cell<u64>,
    maximum_mapped_window: Cell<u64>,
    released_each_read_window: Cell<bool>,
    failure: RefCell<Option<NativeGraphError>>,
}

pub(in crate::lifecycle::native_graph) struct NativeSpillReader<'a, 'm> {
    source: NativePreparationSource<'a, 'm>,
    target_generation: crate::property_graph::GraphGeneration,
}

impl<'a, 'm> NativeSpillReader<'a, 'm> {
    pub(in crate::lifecycle::native_graph) fn new(
        lease: &'a NativeReadLease,
        memory: &'m StorageMemory<'m>,
        target_generation: crate::property_graph::GraphGeneration,
    ) -> Result<Self, NativeGraphError> {
        if target_generation > lease.bundle().base().generation {
            return Err(NativeGraphError::Invalid("spill reader target generation"));
        }
        Ok(Self {
            source: NativePreparationSource::new(
                lease,
                memory,
                crate::property_graph::storage::MAX_NATIVE_ARTIFACTS,
            )?,
            target_generation,
        })
    }

    fn validate_required(
        &self,
        required: RequiredRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        if required.object.family != 17 {
            return Err(TreeError::Invalid("legacy checkpoint reference"));
        }
        if required.object.version != 1 || required.block.artifact != required.object.artifact {
            return Err(TreeError::Invalid("required reference descriptor domain"));
        }
        // Reuse the source's charged immutable mapping slots. Reader closure
        // records must authenticate each pack once, while checking every block.
        use crate::property_graph::storage::tree::directory::BlockSource;
        let block = self.source.resolve(required.block, resources)?;
        let identity = block.identity();
        if identity.store != required.object.store
            || identity.artifact != required.object.artifact
            || identity.generation != required.object.generation
            || identity.creation_serial != required.object.serial
            || block.file_length() != required.object.bytes as usize
            || block.file_checksum() != required.object.checksum
            || block.reference() != required.block
        {
            return Err(TreeError::Invalid("required reference descriptor mismatch"));
        }
        Ok(())
    }

    pub(super) fn validate_record(
        &self,
        record: crate::property_graph::storage::reclaim::ProtectedRecord,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        match record.value {
            crate::property_graph::storage::reclaim::ProtectedValue::Required(required) => {
                self.validate_required(required, resources)
            }
            crate::property_graph::storage::reclaim::ProtectedValue::Descriptor(descriptor) => self
                .source
                .validate_object_descriptor(descriptor, resources),
            crate::property_graph::storage::reclaim::ProtectedValue::FoldAuthority { .. } => Ok(()),
            crate::property_graph::storage::reclaim::ProtectedValue::CapturedBase {
                checkpoint,
                ..
            } => self.validate_required(checkpoint, resources),
        }
    }
}

impl SpillIo for NativeSpillReader<'_, '_> {
    fn append_page(
        &mut self,
        _: &[u8],
        _: &mut TreeResources<'_>,
    ) -> Result<RequiredRef, TreeError> {
        Err(TreeError::Invalid("read-only spill reader append"))
    }

    fn read_page(
        &self,
        reference: RequiredRef,
        output: &mut [u8],
        resources: &mut TreeResources<'_>,
    ) -> Result<usize, TreeError> {
        self.source
            .copy_spill_page(reference, self.target_generation, output, resources)
    }
}

/// Opaque handoff for one fully flushed and locally validated durable proof.
/// The registration remains live until the sole commit path adopts its roots.
pub(in crate::lifecycle::native_graph) struct PreparedDurableSpill {
    admitted: NativeReadLease,
    capture: NativeProtectedRoots,
    binding: SpillBinding,
    protected: DurableProtectedStream,
    mark: DurableRun,
    intent: Option<RequiredRef>,
    intent_digest: u64,
    candidate_count: usize,
    partial_count: usize,
    allocation_head: RequiredRef,
    _registration: NativeSpillRegistration,
}

impl PreparedDurableSpill {
    pub(in crate::lifecycle::native_graph) const fn binding(&self) -> SpillBinding {
        self.binding
    }

    pub(in crate::lifecycle::native_graph) const fn protected(&self) -> DurableProtectedStream {
        self.protected
    }

    pub(in crate::lifecycle::native_graph) const fn mark(&self) -> DurableRun {
        self.mark
    }

    pub(in crate::lifecycle::native_graph) const fn allocation_head(&self) -> RequiredRef {
        self.allocation_head
    }

    pub(in crate::lifecycle::native_graph) const fn intent(&self) -> Option<RequiredRef> {
        self.intent
    }

    pub(in crate::lifecycle::native_graph) const fn intent_digest(&self) -> u64 {
        self.intent_digest
    }

    pub(in crate::lifecycle::native_graph) const fn candidate_count(&self) -> usize {
        self.candidate_count
    }

    pub(in crate::lifecycle::native_graph) const fn partial_count(&self) -> usize {
        self.partial_count
    }

    pub(in crate::lifecycle::native_graph) fn matches_admission(
        &self,
        lease: &NativeReadLease,
    ) -> bool {
        self.admitted.token() == lease.token()
            && std::sync::Arc::ptr_eq(self.admitted.bundle(), lease.bundle())
            && self.capture.contains_lease(lease)
            && self.capture.serial_fence() == self.binding.serial_fence
    }

    pub(in crate::lifecycle::native_graph) fn validate_candidates<'m>(
        &self,
        candidates: &[ArtifactDescriptor],
        partials: &[crate::property_graph::storage::reclaim::PartialTarget],
        memory: &'m StorageMemory<'m>,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), NativeGraphError> {
        if candidates.len() != self.candidate_count || partials.len() != self.partial_count {
            return Err(NativeGraphError::Invalid(
                "native spill candidate count mismatch",
            ));
        }
        let reader = NativeSpillReader {
            source: NativePreparationSource::new(
                &self.admitted,
                memory,
                crate::property_graph::storage::MAX_NATIVE_ARTIFACTS,
            )?,
            target_generation: self.binding.target_generation,
        };
        #[cfg(all(test, feature = "graph-cypher"))]
        let mark_reads_before =
            crate::property_graph::storage::preparation_work_capture::mark_entries();
        // Validate every page, ordering, count and digest once. The mark is
        // immutable; each protected record still requires exact membership.
        let mut mark = DurableRunReader::new(self.mark, memory)?;
        while mark.next(&reader, resources)?.is_some() {}
        validate_protected_stream(
            self.protected,
            &reader,
            memory,
            resources,
            |record, resources| {
                let Some(expected) = record.artifact() else {
                    return Ok(());
                };
                if !mark.contains(expected, &reader, resources)? {
                    return Err(TreeError::Invalid(
                        "protected root is absent from completed mark",
                    ));
                }
                if candidates
                    .iter()
                    .any(|candidate| candidate.artifact == expected)
                    || partials.iter().any(|partial| partial.artifact == expected)
                {
                    return Err(TreeError::Invalid(
                        "reclaim candidate is an authentic protected root",
                    ));
                }
                Ok(())
            },
        )?;
        #[cfg(all(test, feature = "graph-cypher"))]
        crate::property_graph::storage::preparation_work_capture::protected_mark(
            self.mark.count,
            self.protected.count,
            self.mark.height(),
            crate::property_graph::storage::preparation_work_capture::mark_entries()
                - mark_reads_before,
        );
        let Some(intent) = self.intent else {
            return if candidates.is_empty() && partials.is_empty() {
                Ok(())
            } else {
                Err(NativeGraphError::Invalid(
                    "native spill candidate intent is absent",
                ))
            };
        };
        let capacity = intent.block.length as usize;
        let mut bytes = StorageBuffer::new(memory, capacity)?;
        for _ in 0..capacity {
            bytes.push(0)?;
        }
        let length = reader.source.copy_spill_page(
            intent,
            self.binding.target_generation,
            bytes.as_mut_slice(),
            resources,
        )?;
        crate::property_graph::storage::reclaim::validate_pending_intent(
            bytes
                .as_slice()
                .get(..length)
                .ok_or(NativeGraphError::Invalid("native spill intent extent"))?,
            self.binding,
            self.protected,
            self.mark,
            candidates,
            partials,
            self.intent_digest,
        )?;
        Ok(())
    }
}

impl<'a, 'm> NativeSpillWriter<'a, 'm> {
    pub(super) fn new(
        store: &'a Store,
        lease: &'a NativeReadLease,
        memory: &'m StorageMemory<'m>,
        control: &'a QueryControl,
        binding: SpillBinding,
        disk_limit: u64,
    ) -> Result<Self, NativeGraphError> {
        if binding.store != lease.bundle().base().store
            || binding.capture_generation != lease.bundle().base().generation
            || binding.target_generation <= binding.capture_generation
            || binding.sequence != lease.bundle().sequence()
            || disk_limit == 0
        {
            return Err(NativeGraphError::Invalid("native spill binding"));
        }
        Ok(Self {
            store,
            lease,
            source: NativePreparationSource::new(
                lease,
                memory,
                crate::property_graph::storage::MAX_NATIVE_ARTIFACTS,
            )?,
            memory,
            control,
            binding,
            registration: lease.register_spill()?,
            allocation_head: None,
            allocation_ordinal: 0,
            created_objects: 0,
            disk_bytes: 0,
            disk_limit,
            maximum_encoded_backing: 0,
            read_windows: Cell::new(0),
            maximum_mapped_window: Cell::new(0),
            released_each_read_window: Cell::new(true),
            failure: RefCell::new(None),
        })
    }

    pub(in crate::lifecycle::native_graph) const fn binding(&self) -> SpillBinding {
        self.binding
    }

    pub(super) fn stats(&self) -> NativeSpillStats {
        NativeSpillStats {
            created_objects: self.created_objects,
            disk_bytes: self.disk_bytes,
            maximum_encoded_backing: self.maximum_encoded_backing,
            charged_peak_bytes: self.memory.peak_reserved_bytes(),
            read_windows: self.read_windows.get(),
            maximum_mapped_window: self.maximum_mapped_window.get(),
            released_each_read_window: self.released_each_read_window.get(),
            allocation_head: self.allocation_head,
        }
    }

    pub(super) fn take_failure(&self) -> Option<NativeGraphError> {
        self.failure.borrow_mut().take()
    }

    pub(super) fn validate_candidate(
        &self,
        descriptor: ArtifactDescriptor,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        if descriptor.store != self.binding.store
            || descriptor.serial == 0
            || descriptor.serial > self.binding.serial_fence
            || !matches!(descriptor.family, 17..=19)
            || descriptor.version != 1
        {
            return Err(TreeError::Invalid("reclaim candidate descriptor domain"));
        }
        self.source
            .validate_object_descriptor(descriptor, resources)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "independent resource owners and lifetimes are explicit at this private seam"
    )]
    pub(super) fn finish(
        self,
        capture: NativeProtectedRoots,
        protected: DurableProtectedStream,
        mark: DurableRun,
        intent: Option<RequiredRef>,
        intent_digest: u64,
        candidate_count: usize,
        partial_count: usize,
    ) -> Result<PreparedDurableSpill, NativeGraphError> {
        if protected.binding != self.binding || mark.binding != self.binding {
            return Err(NativeGraphError::Invalid("native spill proof binding"));
        }
        if self.failure.borrow().is_some() {
            return Err(NativeGraphError::Invalid("native spill retained failure"));
        }
        let allocation_head = self.allocation_head.ok_or(NativeGraphError::Invalid(
            "native spill allocation stream is empty",
        ))?;
        if self.registration.head()? != Some(allocation_head) {
            return Err(NativeGraphError::Invalid(
                "native spill allocation head mismatch",
            ));
        }
        // Either partition alone is a complete reason for an intent: an
        // interrupted creation is reclaimed even when no registered object is.
        if intent.is_some() != (candidate_count != 0 || partial_count != 0)
            || (intent.is_none() && intent_digest != 0)
            || candidate_count
                .checked_add(partial_count)
                .is_none_or(|rows| rows > crate::property_graph::storage::reclaim::MAX_CANDIDATES)
        {
            return Err(NativeGraphError::Invalid(
                "native spill reclaim intent association",
            ));
        }
        Ok(PreparedDurableSpill {
            admitted: self.lease.clone(),
            capture,
            binding: self.binding,
            protected,
            mark,
            intent,
            intent_digest,
            candidate_count,
            partial_count,
            allocation_head,
            _registration: self.registration,
        })
    }

    fn remember(&self, error: NativeGraphError) -> TreeError {
        let mut failure = self.failure.borrow_mut();
        if failure.is_none() {
            *failure = Some(error);
        }
        TreeError::Invalid("native durable spill I/O")
    }

    fn identity(&self) -> Result<ArtifactIdentity, NativeGraphError> {
        Ok(ArtifactIdentity {
            store: self.binding.store,
            artifact: next_artifact(&mut crate::property_graph::storage::allocation::OsEntropy)?,
            generation: self.binding.target_generation,
            creation_serial: self.store.native_graph.burn_creation_serial()?,
        })
    }

    fn preflight_disk(&self, bytes: usize) -> Result<u64, NativeGraphError> {
        let bytes = u64::try_from(bytes)
            .map_err(|_| NativeGraphError::Invalid("native spill object length"))?;
        let next = self
            .disk_bytes
            .checked_add(bytes)
            .ok_or(NativeGraphError::IdentityExhausted)?;
        if next > self.disk_limit {
            return Err(NativeGraphError::Invalid("native spill disk budget"));
        }
        Ok(next)
    }

    fn write_registered(
        &mut self,
        identity: ArtifactIdentity,
        bytes: StorageBuffer<'m, u8>,
        next_disk_bytes: u64,
    ) -> Result<(), NativeGraphError> {
        let (path, path_charge) = NativePreparationSource::charged_path(
            self.memory,
            self.lease.bundle().directory(),
            identity.artifact,
        )?;
        self.maximum_encoded_backing = self.maximum_encoded_backing.max(bytes.as_slice().len());
        write_new(
            self.lease.bundle().vfs(),
            self.lease.bundle().directory(),
            &path,
            bytes.as_slice(),
            self.store.durability_policy,
        )?;
        self.disk_bytes = next_disk_bytes;
        self.created_objects = self
            .created_objects
            .checked_add(1)
            .ok_or(NativeGraphError::IdentityExhausted)?;
        drop(bytes);
        drop(path);
        drop(path_charge);
        Ok(())
    }

    fn allocation_payload(
        &self,
        data: ArtifactDescriptor,
    ) -> Result<StorageBuffer<'m, u8>, NativeGraphError> {
        let mut output = zeroed(self.memory, self.control, ALLOCATION_PAGE_BYTES)?;
        let bytes = output.as_mut_slice();
        put(bytes, 0, b"ZGCP")?;
        put(bytes, 4, &5_u16.to_le_bytes())?;
        put(bytes, 6, &1_u16.to_le_bytes())?;
        put(bytes, 8, &1_u16.to_le_bytes())?;
        put(
            bytes,
            10,
            &u16::from(self.allocation_head.is_some()).to_le_bytes(),
        )?;
        put(bytes, 12, &(ALLOCATION_PAGE_BYTES as u32).to_le_bytes())?;
        put(bytes, 16, &self.binding.store.get().to_le_bytes())?;
        put(bytes, 32, &self.binding.session.get().to_le_bytes())?;
        put(
            bytes,
            48,
            &self.binding.capture_generation.get().to_le_bytes(),
        )?;
        put(
            bytes,
            56,
            &self.binding.target_generation.get().to_le_bytes(),
        )?;
        put(bytes, 64, &self.binding.sequence.to_le_bytes())?;
        put(bytes, 72, &self.binding.serial_fence.to_le_bytes())?;
        put(bytes, 80, &self.allocation_ordinal.to_le_bytes())?;
        put(bytes, 88, &1_u64.to_le_bytes())?;
        put_descriptor(bytes, 96, data)?;
        if let Some(previous) = self.allocation_head {
            put_required(bytes, 160, previous)?;
        }
        Ok(output)
    }
}

impl SpillIo for NativeSpillWriter<'_, '_> {
    fn append_page(
        &mut self,
        payload: &[u8],
        resources: &mut TreeResources<'_>,
    ) -> Result<RequiredRef, TreeError> {
        resources.step(payload.len() as u64)?;
        self.control.checkpoint().map_err(TreeError::Control)?;
        let old_head = self.allocation_head;
        let data_identity = self.identity().map_err(|error| self.remember(error))?;
        let (data_bytes, data_ref) = encode_framed(
            self.memory,
            self.control,
            ContainerKind::Object,
            data_identity,
            &[Block {
                kind: BlockKind::CommitParticipant,
                payload,
            }],
        )
        .map_err(|error| self.remember(error))?;
        let data_next = self
            .preflight_disk(data_bytes.as_slice().len())
            .map_err(|error| self.remember(error))?;
        self.registration
            .begin_data(data_ref.object, old_head)
            .map_err(|error| self.remember(error))?;
        self.write_registered(data_identity, data_bytes, data_next)
            .map_err(|error| self.remember(error))?;

        let allocation_payload = self
            .allocation_payload(data_ref.object)
            .map_err(|error| self.remember(error))?;
        resources.step(allocation_payload.as_slice().len() as u64)?;
        let allocation_identity = self.identity().map_err(|error| self.remember(error))?;
        let (allocation_bytes, allocation_ref) = encode_framed(
            self.memory,
            self.control,
            ContainerKind::Object,
            allocation_identity,
            &[Block {
                kind: BlockKind::CommitParticipant,
                payload: allocation_payload.as_slice(),
            }],
        )
        .map_err(|error| self.remember(error))?;
        let allocation_next = self
            .preflight_disk(allocation_bytes.as_slice().len())
            .map_err(|error| self.remember(error))?;
        self.registration
            .begin_inventory(data_ref.object, allocation_ref.object, old_head)
            .map_err(|error| self.remember(error))?;
        drop(allocation_payload);
        self.write_registered(allocation_identity, allocation_bytes, allocation_next)
            .map_err(|error| self.remember(error))?;
        self.registration
            .finish_pair(allocation_ref)
            .map_err(|error| self.remember(error))?;
        self.allocation_head = Some(allocation_ref);
        self.allocation_ordinal = self
            .allocation_ordinal
            .checked_add(1)
            .ok_or(TreeError::Work)?;
        Ok(data_ref)
    }

    fn read_page(
        &self,
        reference: RequiredRef,
        output: &mut [u8],
        resources: &mut TreeResources<'_>,
    ) -> Result<usize, TreeError> {
        #[cfg(any(test, feature = "test-seams"))]
        qualification::read(reference);
        let reserved_before = self.memory.reserved_bytes();
        let length = self.source.copy_spill_page(
            reference,
            self.binding.target_generation,
            output,
            resources,
        )?;
        // Per-read path/scratch charges are released; immutable mappings
        // remain in the bounded source table until the attempt ends.
        let released = self.memory.reserved_bytes() == reserved_before;
        self.released_each_read_window
            .set(self.released_each_read_window.get() && released);
        self.maximum_mapped_window.set(
            self.maximum_mapped_window
                .get()
                .max(u64::from(reference.object.bytes)),
        );
        self.read_windows.set(
            self.read_windows
                .get()
                .checked_add(1)
                .ok_or(TreeError::Work)?,
        );
        if !released {
            return Err(TreeError::Invalid(
                "native spill read window retained backing",
            ));
        }
        Ok(length)
    }
}

fn put(output: &mut [u8], offset: usize, input: &[u8]) -> Result<(), NativeGraphError> {
    let end = offset
        .checked_add(input.len())
        .ok_or(NativeGraphError::Invalid("native spill payload overflow"))?;
    output
        .get_mut(offset..end)
        .ok_or(NativeGraphError::Invalid("native spill payload extent"))?
        .copy_from_slice(input);
    Ok(())
}

fn put_descriptor(
    output: &mut [u8],
    offset: usize,
    descriptor: ArtifactDescriptor,
) -> Result<(), NativeGraphError> {
    put(output, offset, &descriptor.store.get().to_le_bytes())?;
    put(
        output,
        offset + 16,
        &descriptor.artifact.get().to_le_bytes(),
    )?;
    put(
        output,
        offset + 32,
        &descriptor.generation.get().to_le_bytes(),
    )?;
    put(output, offset + 40, &descriptor.serial.to_le_bytes())?;
    put(output, offset + 48, &descriptor.bytes.to_le_bytes())?;
    put(output, offset + 52, &descriptor.family.to_le_bytes())?;
    put(output, offset + 54, &descriptor.version.to_le_bytes())?;
    put(output, offset + 56, &descriptor.checksum.to_le_bytes())
}

fn put_required(
    output: &mut [u8],
    offset: usize,
    required: RequiredRef,
) -> Result<(), NativeGraphError> {
    put_descriptor(output, offset, required.object)?;
    let mut reference = [0_u8; 32];
    crate::property_graph::storage::artifact::encode_reference(required.block, &mut reference)
        .map_err(|_| NativeGraphError::Invalid("native spill predecessor reference"))?;
    put(output, offset + 64, &reference)
}

/// Thread-local, one-shot qualification seam at a real merge input read.
#[cfg(any(test, feature = "test-seams"))]
pub(in crate::lifecycle::native_graph) mod qualification {
    use crate::property_graph::wal::RequiredRef;
    use std::cell::{Cell, RefCell};
    type Hook = Box<dyn FnMut(RequiredRef)>;
    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
        static CHUNK: Cell<usize> = const { Cell::new(crate::property_graph::storage::reclaim::SPILL_CHUNK_LIMIT) };
    }
    pub(crate) struct Guard;
    pub(crate) fn start(hook: impl FnMut(RequiredRef) + 'static) -> Guard {
        CHUNK.with(|chunk| chunk.set(2));
        HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        Guard
    }
    pub(crate) fn chunk() -> usize {
        CHUNK.with(Cell::get)
    }
    pub(crate) fn read(reference: RequiredRef) {
        HOOK.with(|hook| {
            if let Some(hook) = hook.borrow_mut().as_mut() {
                hook(reference);
            }
        });
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            CHUNK.with(|chunk| {
                chunk.set(crate::property_graph::storage::reclaim::SPILL_CHUNK_LIMIT)
            });
            HOOK.with(|hook| *hook.borrow_mut() = None);
        }
    }
}
