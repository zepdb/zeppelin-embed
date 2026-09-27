//! Lossless bounded streams, independent of record interpretation and publication.
//! The required source owns cancellable artifact admission/cache and its lease;
//! polling stream chunks alone does not make an uncached artifact scan cancellable.

use super::artifact::{self, ArtifactId, BlockKind, FramedBlock, PhysicalRef};
use super::tree::directory::{BlockSink, BlockSource, TreeError, TreeResources};
use crate::property_graph::{GraphGeneration, MAX_GRAPH_INPUT_BYTES, StoreInstanceId};

/// Maximum payload bytes in each independently framed raw chunk.
pub const CHUNK_BYTES: usize = 64 * 1024;
/// Derived record indexes may expand beyond canonical input; this remains inside
/// the same 32 MiB total private preparation allowance, not an extra budget.
pub const MAX_RECORD_BYTES: usize = 32 * 1024 * 1024;
/// Maximum for derived records only; other logical roles retain 8 MiB/128 chunks.
pub const MAX_CHUNKS: usize = MAX_RECORD_BYTES / CHUNK_BYTES;
/// Complete maximum nonrecursive descriptor payload, excluding outer framing.
pub const MAX_DESCRIPTOR_BYTES: usize = 32 + MAX_CHUNKS * 32;

/// Typed logical bytes, direct or indexed. Successful construction validates
/// descriptor shape only; validate_all proves required stream availability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PayloadRef {
    role: BlockKind,
    length: u64,
    reference: PhysicalRef,
}
impl PayloadRef {
    /// Validate role, complete logical bound and root-reference shape.
    pub fn new(role: BlockKind, length: u64, reference: PhysicalRef) -> Result<Self, TreeError> {
        if !matches!(
            role,
            BlockKind::NodeRecord
                | BlockKind::RelRecord
                | BlockKind::CanonicalImage
                | BlockKind::StoredText
                | BlockKind::StoredVector
                | BlockKind::OverflowKey
                | BlockKind::OperationProvenance
                | BlockKind::RetrievalRows
                | BlockKind::RetrievalLexical
                | BlockKind::RetrievalLiveRows
                | BlockKind::RetrievalVectorIndex
        ) || length > role_limit(role) as u64
            || (role == BlockKind::OverflowKey
                && (length < 9 || reference.kind != BlockKind::OverflowKey))
            || (reference.kind != role && reference.kind != BlockKind::ExtentList)
        {
            return Err(TreeError::Invalid("logical payload role or length"));
        }
        check_reference(reference)?;
        Ok(Self {
            role,
            length,
            reference,
        })
    }
    /// Encode the fixed 48-byte typed record/payload descriptor.
    pub fn encode_into(self, output: &mut [u8]) -> Result<(), TreeError> {
        if output.len() != 48 {
            return Err(TreeError::Invalid("typed payload descriptor width"));
        }
        put(output, 0, &(self.role as u16).to_le_bytes())?;
        put(output, 2, &1u16.to_le_bytes())?;
        put(output, 4, &0u32.to_le_bytes())?;
        put(output, 8, &self.length.to_le_bytes())?;
        artifact::encode_reference(
            self.reference,
            output
                .get_mut(16..48)
                .ok_or(TreeError::Invalid("payload reference extent"))?,
        )?;
        Ok(())
    }
    /// Decode an exact typed descriptor; role-specific bounds cannot be widened
    /// by the physical ExtentList kind or by a forged total/chunk count.
    pub fn decode(bytes: &[u8]) -> Result<Self, TreeError> {
        if bytes.len() != 48
            || u16::from_le_bytes(read(bytes, 2)?) != 1
            || u32::from_le_bytes(read(bytes, 4)?) != 0
        {
            return Err(TreeError::Invalid(
                "typed payload descriptor width/version/reserved",
            ));
        }
        let role = match u16::from_le_bytes(read(bytes, 0)?) {
            2 => BlockKind::NodeRecord,
            3 => BlockKind::RelRecord,
            4 => BlockKind::CanonicalImage,
            5 => BlockKind::StoredText,
            6 => BlockKind::StoredVector,
            7 => BlockKind::OverflowKey,
            12 => BlockKind::OperationProvenance,
            15 => BlockKind::RetrievalRows,
            16 => BlockKind::RetrievalLexical,
            17 => BlockKind::RetrievalLiveRows,
            18 => BlockKind::RetrievalVectorIndex,
            _ => return Err(TreeError::Invalid("logical payload role")),
        };
        let reference = artifact::decode_reference(
            bytes
                .get(16..48)
                .ok_or(TreeError::Invalid("descriptor reference extent"))?,
        )?;
        Self::new(role, u64::from_le_bytes(read(bytes, 8)?), reference)
    }
    /// Complete logical stream length, independent of physical packing.
    pub const fn len(self) -> u64 {
        self.length
    }
    /// Whether this is a present empty logical payload.
    pub const fn is_empty(self) -> bool {
        self.length == 0
    }
    /// Required logical interpretation, not the descriptor's physical block kind.
    pub const fn role(self) -> BlockKind {
        self.role
    }
    /// Exact immutable root block.
    pub const fn reference(self) -> PhysicalRef {
        self.reference
    }
    pub(super) fn creation_generation(
        self,
        source: &impl BlockSource,
        store: StoreInstanceId,
        generation: GraphGeneration,
        resources: &mut TreeResources<'_>,
    ) -> Result<GraphGeneration, TreeError> {
        if source.scoped_blocks() {
            return source.with_block(self.reference, resources, |block, resources| {
                let block = checked_resolved_block(block, self.reference, store, generation)?;
                Ok(self.check_root(block, resources)?.identity().generation)
            });
        }
        Ok(self
            .root(source, store, generation, resources)?
            .identity()
            .generation)
    }
    fn indirect(self) -> bool {
        self.reference.kind == BlockKind::ExtentList || self.role == BlockKind::OverflowKey
    }
    fn root<'a>(
        self,
        source: &'a impl BlockSource,
        store: StoreInstanceId,
        generation: GraphGeneration,
        resources: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        let block = resolve(source, self.reference, store, generation, resources)?;
        self.check_root(block, resources)
    }
    fn check_root<'a>(
        self,
        block: FramedBlock<'a>,
        resources: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        let bytes = block.payload();
        if self.indirect() {
            if self.length == 0
                || bytes.len() < 32
                || bytes.get(..4) != Some(b"ZGEX".as_slice())
                || u16::from_le_bytes(read(bytes, 4)?) != 1
                || u16::from_le_bytes(read(bytes, 6)?) != self.role as u16
                || u64::from_le_bytes(read(bytes, 8)?) != self.length
                || u32::from_le_bytes(read(bytes, 20)?) as usize != CHUNK_BYTES
                || u64::from_le_bytes(read(bytes, 24)?) != 0
            {
                return Err(TreeError::Invalid("extent header/role/length/version"));
            }
            let count = u32::from_le_bytes(read(bytes, 16)?) as usize;
            let expected = (self.length as usize).div_ceil(CHUNK_BYTES);
            if count != expected || count > MAX_CHUNKS || bytes.len() != 32 + count * 32 {
                return Err(TreeError::Invalid("extent count or descriptor length"));
            }
            // Every reference is structurally checked, even when a point read
            // visits only one chunk. Existence/content validation is validate_all.
            for index in 0..count {
                resources.step(1)?;
                chunk_reference(bytes, index)?;
            }
        } else if bytes.len() as u64 != self.length {
            return Err(TreeError::Invalid("direct logical payload length"));
        }
        Ok(block)
    }

    /// Run one bounded span read without allowing the source mapping lifetime
    /// to escape. Ordinary sources delegate to their retained borrowed resolve;
    /// captured trace sources release backing before this method returns.
    #[allow(
        clippy::too_many_arguments,
        reason = "independent resource owners and lifetimes are explicit at this private seam"
    )]
    pub(super) fn with_span_at<R>(
        self,
        source: &impl BlockSource,
        store: StoreInstanceId,
        generation: GraphGeneration,
        offset: u64,
        maximum: usize,
        resources: &mut TreeResources<'_>,
        callback: impl for<'a, 'r> FnOnce(&'a [u8], &'r mut TreeResources<'_>) -> Result<R, TreeError>,
    ) -> Result<R, TreeError> {
        if offset > self.length {
            return Err(TreeError::Invalid("read beyond logical end"));
        }
        source.with_block(self.reference, resources, |root, resources| {
            let root = checked_resolved_block(root, self.reference, store, generation)?;
            let root = self.check_root(root, resources)?;
            if offset == self.length {
                return callback(
                    root.payload()
                        .get(..0)
                        .ok_or(TreeError::Invalid("empty span"))?,
                    resources,
                );
            }
            if self.indirect() {
                let index = offset as usize / CHUNK_BYTES;
                let reference = chunk_reference(root.payload(), index)?;
                let root_generation = root.identity().generation;
                source.with_block(reference, resources, |chunk, resources| {
                    let chunk = checked_resolved_block(chunk, reference, store, root_generation)?;
                    self.check_chunk(index, chunk.payload())?;
                    let start = offset as usize % CHUNK_BYTES;
                    let length = chunk
                        .payload()
                        .len()
                        .saturating_sub(start)
                        .min(CHUNK_BYTES)
                        .min(maximum);
                    resources.step(length as u64)?;
                    callback(
                        chunk
                            .payload()
                            .get(start..start + length)
                            .ok_or(TreeError::Invalid("payload span extent"))?,
                        resources,
                    )
                })
            } else {
                let start = offset as usize;
                let length = root
                    .payload()
                    .len()
                    .saturating_sub(start)
                    .min(CHUNK_BYTES)
                    .min(maximum);
                resources.step(length as u64)?;
                callback(
                    root.payload()
                        .get(start..start + length)
                        .ok_or(TreeError::Invalid("payload span extent"))?,
                    resources,
                )
            }
        })
    }
    /// Validate every required chunk and its exact geometry. Repeated exact chunk
    /// refs are legal; they repeat those bytes in the logical stream.
    pub fn validate_all(
        self,
        source: &impl BlockSource,
        store: StoreInstanceId,
        generation: GraphGeneration,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        if source.scoped_blocks() {
            let mut descriptor = [0_u8; MAX_DESCRIPTOR_BYTES];
            let (root_generation, descriptor_length) =
                source.with_block(self.reference, resources, |root, resources| {
                    let root = checked_resolved_block(root, self.reference, store, generation)?;
                    let root = self.check_root(root, resources)?;
                    if !self.indirect() {
                        for chunk in root.payload().chunks(CHUNK_BYTES) {
                            resources.step(chunk.len() as u64)?;
                        }
                        return Ok((root.identity().generation, 0));
                    }
                    let length = root.payload().len();
                    descriptor
                        .get_mut(..length)
                        .ok_or(TreeError::Invalid("extent descriptor copy bound"))?
                        .copy_from_slice(root.payload());
                    Ok((root.identity().generation, length))
                })?;
            if !self.indirect() {
                return Ok(());
            }
            let bytes = descriptor
                .get(..descriptor_length)
                .ok_or(TreeError::Invalid("extent descriptor copy extent"))?;
            let count = (self.length as usize).div_ceil(CHUNK_BYTES);
            for index in 0..count {
                let reference = chunk_reference(bytes, index)?;
                source.with_block(reference, resources, |chunk, resources| {
                    let chunk = checked_resolved_block(chunk, reference, store, root_generation)?;
                    self.check_chunk(index, chunk.payload())?;
                    resources.step(chunk.payload().len() as u64)
                })?;
            }
            return Ok(());
        }
        let root = self.root(source, store, generation, resources)?;
        if self.indirect() {
            let count = (self.length as usize).div_ceil(CHUNK_BYTES);
            for index in 0..count {
                let reference = chunk_reference(root.payload(), index)?;
                let chunk = resolve(
                    source,
                    reference,
                    store,
                    root.identity().generation,
                    resources,
                )?;
                self.check_chunk(index, chunk.payload())?;
                resources.step(chunk.payload().len() as u64)?;
            }
        } else {
            for chunk in root.payload().chunks(CHUNK_BYTES) {
                resources.step(chunk.len() as u64)?;
            }
        }
        Ok(())
    }
    /// Return one checked physical descendant at a time. Index zero is the
    /// direct payload or extent root; later indexes are exact validated chunks.
    pub(crate) fn physical_reference_at(
        self,
        source: &impl BlockSource,
        store: StoreInstanceId,
        generation: GraphGeneration,
        index: usize,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<PhysicalRef>, TreeError> {
        let root = self.root(source, store, generation, resources)?;
        if index == 0 {
            return Ok(Some(self.reference));
        }
        if !self.indirect() {
            return Ok(None);
        }
        let chunk_index = index - 1;
        let count = (self.length as usize).div_ceil(CHUNK_BYTES);
        if chunk_index == count {
            return Ok(None);
        }
        if chunk_index > count {
            return Err(TreeError::Invalid("payload trace beyond exact end"));
        }
        let reference = chunk_reference(root.payload(), chunk_index)?;
        let chunk = resolve(
            source,
            reference,
            store,
            root.identity().generation,
            resources,
        )?;
        self.check_chunk(chunk_index, chunk.payload())?;
        Ok(Some(reference))
    }
    /// Scoped counterpart used by resumable reclamation tracing. It returns
    /// only the copied physical descriptor after all mapped backing is gone.
    pub(crate) fn physical_reference_at_scoped(
        self,
        source: &impl BlockSource,
        store: StoreInstanceId,
        generation: GraphGeneration,
        index: usize,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<PhysicalRef>, TreeError> {
        source.with_block(self.reference, resources, |root, resources| {
            let root = checked_resolved_block(root, self.reference, store, generation)?;
            let root = self.check_root(root, resources)?;
            if index == 0 {
                return Ok(Some(self.reference));
            }
            if !self.indirect() {
                return Ok(None);
            }
            let chunk_index = index - 1;
            let count = (self.length as usize).div_ceil(CHUNK_BYTES);
            if chunk_index == count {
                return Ok(None);
            }
            if chunk_index > count {
                return Err(TreeError::Invalid("payload trace beyond exact end"));
            }
            let reference = chunk_reference(root.payload(), chunk_index)?;
            let root_generation = root.identity().generation;
            source.with_block(reference, resources, |chunk, _resources| {
                let chunk = checked_resolved_block(chunk, reference, store, root_generation)?;
                self.check_chunk(chunk_index, chunk.payload())?;
                Ok(Some(reference))
            })
        })
    }
    fn check_chunk(self, index: usize, bytes: &[u8]) -> Result<(), TreeError> {
        let start = index
            .checked_mul(CHUNK_BYTES)
            .ok_or(TreeError::Invalid("chunk offset overflow"))?;
        let remaining = (self.length as usize)
            .checked_sub(start)
            .ok_or(TreeError::Invalid("chunk beyond logical end"))?;
        if remaining == 0 || bytes.len() != remaining.min(CHUNK_BYTES) {
            return Err(TreeError::Invalid("chunk payload length"));
        }
        Ok(())
    }
    /// Borrow at most one 64 KiB span under the caller's retained source lease.
    /// No backing allocation or whole-stream copy is created. The caller must not
    /// treat point access as proof that every other chunk exists.
    pub fn span_at<'a>(
        self,
        source: &'a impl BlockSource,
        store: StoreInstanceId,
        generation: GraphGeneration,
        offset: u64,
        resources: &mut TreeResources<'_>,
    ) -> Result<&'a [u8], TreeError> {
        self.span_at_bounded(source, store, generation, offset, CHUNK_BYTES, resources)
    }
    /// Internal window access charges only the bytes made available to that
    /// window. Exact source/frame/extent checks remain identical to full spans.
    pub(super) fn span_at_bounded<'a>(
        self,
        source: &'a impl BlockSource,
        store: StoreInstanceId,
        generation: GraphGeneration,
        offset: u64,
        maximum: usize,
        resources: &mut TreeResources<'_>,
    ) -> Result<&'a [u8], TreeError> {
        if offset > self.length {
            return Err(TreeError::Invalid("read beyond logical end"));
        }
        let root = self.root(source, store, generation, resources)?;
        if offset == self.length {
            return root
                .payload()
                .get(..0)
                .ok_or(TreeError::Invalid("empty span"));
        }
        let (bytes, start) = if self.indirect() {
            let index = offset as usize / CHUNK_BYTES;
            let reference = chunk_reference(root.payload(), index)?;
            let block = resolve(
                source,
                reference,
                store,
                root.identity().generation,
                resources,
            )?;
            self.check_chunk(index, block.payload())?;
            (block.payload(), offset as usize % CHUNK_BYTES)
        } else {
            (root.payload(), offset as usize)
        };
        let length = (bytes.len() - start).min(CHUNK_BYTES).min(maximum);
        resources.step(length as u64)?;
        bytes
            .get(start..start + length)
            .ok_or(TreeError::Invalid("payload span extent"))
    }
    /// Read at most the destination length, checking and charging every <=64 KiB
    /// copy. Returns zero exactly at end; offset beyond end is a malformed request.
    pub fn read_at(
        self,
        source: &impl BlockSource,
        store: StoreInstanceId,
        generation: GraphGeneration,
        offset: u64,
        output: &mut [u8],
        resources: &mut TreeResources<'_>,
    ) -> Result<usize, TreeError> {
        if offset > self.length {
            return Err(TreeError::Invalid("read beyond logical end"));
        }
        let root = self.root(source, store, generation, resources)?;
        let wanted = output.len().min((self.length - offset) as usize);
        let mut copied = 0usize;
        while copied < wanted {
            let position = offset as usize + copied;
            let (bytes, start) = if self.indirect() {
                let index = position / CHUNK_BYTES;
                let reference = chunk_reference(root.payload(), index)?;
                let block = resolve(
                    source,
                    reference,
                    store,
                    root.identity().generation,
                    resources,
                )?;
                self.check_chunk(index, block.payload())?;
                (block.payload(), position % CHUNK_BYTES)
            } else {
                (root.payload(), position)
            };
            let length = (bytes.len() - start).min(wanted - copied).min(CHUNK_BYTES);
            resources.step(length as u64)?;
            let from = bytes
                .get(start..start + length)
                .ok_or(TreeError::Invalid("chunk read extent"))?;
            output
                .get_mut(copied..copied + length)
                .ok_or(TreeError::Memory)?
                .copy_from_slice(from);
            copied += length;
        }
        Ok(copied)
    }
}

fn role_limit(role: BlockKind) -> usize {
    if matches!(
        role,
        BlockKind::NodeRecord
            | BlockKind::RelRecord
            | BlockKind::RetrievalRows
            | BlockKind::RetrievalLexical
            | BlockKind::RetrievalLiveRows
            | BlockKind::RetrievalVectorIndex
    ) {
        MAX_RECORD_BYTES
    } else {
        MAX_GRAPH_INPUT_BYTES
    }
}
fn validate_prepared(role: BlockKind, length: usize) -> Result<(), TreeError> {
    if length > role_limit(role)
        || (role == BlockKind::OverflowKey && length < 9)
        || !matches!(
            role,
            BlockKind::NodeRecord
                | BlockKind::RelRecord
                | BlockKind::CanonicalImage
                | BlockKind::StoredText
                | BlockKind::StoredVector
                | BlockKind::OverflowKey
                | BlockKind::OperationProvenance
                | BlockKind::RetrievalRows
                | BlockKind::RetrievalLexical
                | BlockKind::RetrievalLiveRows
                | BlockKind::RetrievalVectorIndex
        )
    {
        return Err(TreeError::Invalid(
            "prepared payload role or complete length",
        ));
    }
    Ok(())
}
/// Stage lossless borrowed bytes without an additional whole-stream allocation.
/// The sink charges all retained capacity and retains its explicit abort inventory.
pub fn prepare_payload(
    store: &mut impl BlockSink,
    store_id: StoreInstanceId,
    generation: GraphGeneration,
    role: BlockKind,
    bytes: &[u8],
    resources: &mut TreeResources<'_>,
) -> Result<PayloadRef, TreeError> {
    prepare_stream(
        store,
        store_id,
        generation,
        role,
        bytes.len(),
        &mut |offset, output, resources| {
            resources.step(output.len() as u64)?;
            let start = usize::try_from(offset).map_err(|_| TreeError::Memory)?;
            let end = start.checked_add(output.len()).ok_or(TreeError::Memory)?;
            output.copy_from_slice(
                bytes
                    .get(start..end)
                    .ok_or(TreeError::Invalid("input stream extent"))?,
            );
            Ok(())
        },
        resources,
    )
}
/// Stage a bounded generated record/byte stream through one fixed 64 KiB buffer.
/// The internal producer fills each exact span; it may fail/cancel but cannot
/// publish a prefix. This is an engine component seam, not an application callback.
/// All sink inventories/capacities remain part of the one preparation budget.
pub fn prepare_stream(
    store: &mut impl BlockSink,
    store_id: StoreInstanceId,
    generation: GraphGeneration,
    role: BlockKind,
    length: usize,
    producer: &mut impl FnMut(u64, &mut [u8], &mut TreeResources<'_>) -> Result<(), TreeError>,
    resources: &mut TreeResources<'_>,
) -> Result<PayloadRef, TreeError> {
    resources.step(1)?;
    validate_prepared(role, length)?;
    let mut buffer = [0; CHUNK_BYTES];
    if length <= CHUNK_BYTES && role != BlockKind::OverflowKey {
        let bytes = buffer.get_mut(..length).ok_or(TreeError::Memory)?;
        producer(0, bytes, resources)?;
        resources.step(0)?;
        let reference = append_verified(store, store_id, generation, role, bytes, resources)?;
        return PayloadRef::new(role, length as u64, reference);
    }
    let count = length.div_ceil(CHUNK_BYTES);
    let mut descriptor = [0; MAX_DESCRIPTOR_BYTES];
    put(&mut descriptor, 0, b"ZGEX")?;
    put(&mut descriptor, 4, &1u16.to_le_bytes())?;
    put(&mut descriptor, 6, &(role as u16).to_le_bytes())?;
    put(&mut descriptor, 8, &(length as u64).to_le_bytes())?;
    put(&mut descriptor, 16, &(count as u32).to_le_bytes())?;
    put(&mut descriptor, 20, &(CHUNK_BYTES as u32).to_le_bytes())?;
    for index in 0..count {
        let offset = index.checked_mul(CHUNK_BYTES).ok_or(TreeError::Memory)?;
        let width = (length - offset).min(CHUNK_BYTES);
        let bytes = buffer.get_mut(..width).ok_or(TreeError::Memory)?;
        resources.step(1)?;
        producer(offset as u64, bytes, resources)?;
        resources.step(0)?;
        let reference = append_verified(
            store,
            store_id,
            generation,
            BlockKind::PayloadChunk,
            bytes,
            resources,
        )?;
        artifact::encode_reference(
            reference,
            descriptor
                .get_mut(32 + index * 32..64 + index * 32)
                .ok_or(TreeError::Invalid("descriptor extent"))?,
        )?;
    }
    let kind = if role == BlockKind::OverflowKey {
        BlockKind::OverflowKey
    } else {
        BlockKind::ExtentList
    };
    let reference = append_verified(
        store,
        store_id,
        generation,
        kind,
        descriptor
            .get(..32 + count * 32)
            .ok_or(TreeError::Invalid("descriptor bound"))?,
        resources,
    )?;
    resources.step(0)?;
    PayloadRef::new(role, length as u64, reference)
}
fn append_verified(
    store: &mut impl BlockSink,
    store_id: StoreInstanceId,
    generation: GraphGeneration,
    kind: BlockKind,
    bytes: &[u8],
    resources: &mut TreeResources<'_>,
) -> Result<PhysicalRef, TreeError> {
    for chunk in bytes.chunks(CHUNK_BYTES) {
        resources.step(chunk.len() as u64)?;
    }
    let reference = store.append(kind, generation, bytes, resources)?;
    let block = resolve(store, reference, store_id, generation, resources)?;
    if reference.kind != kind
        || block.identity().generation != generation
        || block.payload().len() != bytes.len()
    {
        return Err(TreeError::Invalid("sink substituted payload"));
    }
    for (left, right) in block
        .payload()
        .chunks(CHUNK_BYTES)
        .zip(bytes.chunks(CHUNK_BYTES))
    {
        resources.step(left.len() as u64)?;
        if left != right {
            return Err(TreeError::Invalid("sink changed payload bytes"));
        }
    }
    Ok(reference)
}
fn resolve<'a>(
    source: &'a impl BlockSource,
    reference: PhysicalRef,
    store: StoreInstanceId,
    generation: GraphGeneration,
    resources: &mut TreeResources<'_>,
) -> Result<FramedBlock<'a>, TreeError> {
    resources.step(1)?;
    check_reference(reference)?;
    let block = source.resolve(reference, resources)?;
    resources.step(0)?;
    checked_resolved_block(block, reference, store, generation)
}
fn checked_resolved_block<'a>(
    block: FramedBlock<'a>,
    reference: PhysicalRef,
    store: StoreInstanceId,
    generation: GraphGeneration,
) -> Result<FramedBlock<'a>, TreeError> {
    check_reference(reference)?;
    if block.reference() != reference
        || block.identity().artifact != reference.artifact
        || block.identity().store != store
        || block.identity().generation.get() > generation.get()
    {
        return Err(TreeError::Invalid("substituted/wrong-store/future payload"));
    }
    Ok(block)
}
fn check_reference(reference: PhysicalRef) -> Result<(), TreeError> {
    if reference.version != 1
        || reference.offset < artifact::HEADER_BYTES as u64
        || reference.length < 24
        || reference
            .offset
            .checked_add(reference.length as u64)
            .is_none_or(|end| end > artifact::MAX_ARTIFACT_BYTES as u64)
    {
        return Err(TreeError::Invalid("payload physical reference"));
    }
    Ok(())
}
fn chunk_reference(bytes: &[u8], index: usize) -> Result<PhysicalRef, TreeError> {
    let start = index
        .checked_mul(32)
        .and_then(|n| n.checked_add(32))
        .ok_or(TreeError::Invalid("extent index overflow"))?;
    let id = u128::from_le_bytes(read(bytes, start)?);
    if id == 0
        || u16::from_le_bytes(read(bytes, start + 28)?) != BlockKind::PayloadChunk as u16
        || u16::from_le_bytes(read(bytes, start + 30)?) != 1
    {
        return Err(TreeError::Invalid("extent chunk identity/kind/version"));
    }
    let reference = PhysicalRef {
        artifact: ArtifactId::new(id)?,
        offset: u64::from_le_bytes(read(bytes, start + 16)?),
        length: u32::from_le_bytes(read(bytes, start + 24)?),
        kind: BlockKind::PayloadChunk,
        version: 1,
    };
    check_reference(reference)?;
    Ok(reference)
}
fn read<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], TreeError> {
    let end = offset
        .checked_add(N)
        .ok_or(TreeError::Invalid("field offset overflow"))?;
    bytes
        .get(offset..end)
        .and_then(|part| part.try_into().ok())
        .ok_or(TreeError::Invalid("truncated field"))
}
fn put(bytes: &mut [u8], offset: usize, value: &[u8]) -> Result<(), TreeError> {
    let end = offset
        .checked_add(value.len())
        .ok_or(TreeError::Invalid("field offset overflow"))?;
    bytes
        .get_mut(offset..end)
        .ok_or(TreeError::Invalid("field extent"))?
        .copy_from_slice(value);
    Ok(())
}

pub(super) mod writer;
