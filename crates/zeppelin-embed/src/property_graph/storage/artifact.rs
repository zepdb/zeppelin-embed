//! Required graph object/root envelope framing, independent of checkpoint semantics.

use crate::format::{
    FormatFamily,
    frame::{self, FormatCheck, FormatError},
};
use crate::property_graph::{GraphGeneration, StoreInstanceId};
use std::convert::Infallible;
mod private;
pub use private::{OwnedArtifact, PrivateArtifact};
use xxhash_rust::xxh3::Xxh3;

/// Framing failure or the caller's typed cancellation/work rejection.
#[derive(Debug)]
pub enum ArtifactControlError<E> {
    /// Malformed container bytes or encoding input.
    Format(FormatError),
    /// The mandatory caller callback rejected further bounded work.
    Control(E),
}
impl<E> From<FormatError> for ArtifactControlError<E> {
    fn from(error: FormatError) -> Self {
        Self::Format(error)
    }
}

fn uncontrolled<T>(result: Result<T, ArtifactControlError<Infallible>>) -> Result<T, FormatError> {
    match result {
        Ok(value) => Ok(value),
        Err(ArtifactControlError::Format(error)) => Err(error),
        Err(ArtifactControlError::Control(never)) => match never {},
    }
}
fn poll<E>(
    control: &mut impl FnMut(usize) -> Result<(), E>,
    bytes: usize,
) -> Result<(), ArtifactControlError<E>> {
    control(bytes).map_err(ArtifactControlError::Control)
}
fn controlled_checksum<E>(
    bytes: &[u8],
    control: &mut impl FnMut(usize) -> Result<(), E>,
) -> Result<u64, ArtifactControlError<E>> {
    let mut hash = Xxh3::new();
    for chunk in bytes.chunks(65_536) {
        poll(control, chunk.len())?;
        hash.update(chunk);
    }
    Ok(hash.digest())
}

/// Fixed file header plus graph identity/geometry prefix.
pub const HEADER_BYTES: usize = 96;
/// Maximum complete object, including framing, directory and checksum trailer.
pub const MAX_ARTIFACT_BYTES: usize = 4 * 1024 * 1024;
/// Upper body bound before subtracting the directory's 24 bytes per block.
pub const MAX_BODY_BYTES: usize = MAX_ARTIFACT_BYTES - HEADER_BYTES - 8;

/// Physical object nonce, independent of logical node/relationship allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ArtifactId(u128);
impl ArtifactId {
    /// Checks a persisted or entropy-supplied nonce; zero is reserved.
    pub fn new(value: u128) -> Result<Self, FormatError> {
        if value == 0 {
            Err(invalid(
                FormatCheck::ObjectIdentity,
                "zero artifact identity",
            ))
        } else {
            Ok(Self(value))
        }
    }
    /// Returns all nonce bits.
    pub const fn get(self) -> u128 {
        self.0
    }
}

/// Required native container family; neither is a legacy vector segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContainerKind {
    /// Packed immutable blocks.
    Object,
    /// One nonempty opaque checkpoint payload, not a validated checkpoint.
    RootEnvelope,
}

/// Append-only block framing tags. Inner record semantics have separate owners.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum BlockKind {
    /// A fixed-size structurally framed tree page.
    TreePage = 1,
    /// A node record, interpreted by the record codec.
    NodeRecord = 2,
    /// A relationship record, interpreted by the record codec.
    RelRecord = 3,
    /// Lossless logical canonical bytes.
    CanonicalImage = 4,
    /// Original text payload bytes.
    StoredText = 5,
    /// Original vector payload bytes.
    StoredVector = 6,
    /// An overflow-key extent descriptor owned by the tree resolver.
    OverflowKey = 7,
    /// An indexed logical extent descriptor.
    ExtentList = 8,
    /// Opaque checkpoint bytes requiring writes-owned logical validation.
    CheckpointPayload = 9,
    /// Required role/version-tagged graph commit participant.
    CommitParticipant = 10,
    /// One raw fragment of a typed, nonrecursive logical extent stream.
    PayloadChunk = 11,
    /// Complete versioned logical installing-operation evidence.
    OperationProvenance = 12,
    /// Bounded immutable adjacency base (ZE-124).
    AdjacencyBase = 13,
    /// Bounded immutable adjacency delta (ZE-124).
    AdjacencyDelta = 14,
    /// Sparse retrieval source row table.
    RetrievalRows = 15,
    /// Sparse retrieval lexical region.
    RetrievalLexical = 16,
    /// Sparse retrieval source live mask.
    RetrievalLiveRows = 17,
    /// Complete immutable native derived vector-index image.
    RetrievalVectorIndex = 18,
}

/// Identity and monotone creation metadata covered by the file checksum.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactIdentity {
    /// Persisted store incarnation.
    pub store: StoreInstanceId,
    /// Exclusive object nonce.
    pub artifact: ArtifactId,
    /// Preparation's target graph generation.
    pub generation: GraphGeneration,
    /// Serial allocated under the writes publication mutex.
    pub creation_serial: u64,
}

/// One borrowed input block; encoding does not retain or allocate its bytes.
#[derive(Clone, Copy, Debug)]
pub struct Block<'a> {
    /// Required framing kind.
    pub kind: BlockKind,
    /// Bytes interpreted by the kind's logical codec.
    pub payload: &'a [u8],
}

/// A reference to one entire framed block, including its 24-byte header.
/// It does not denote an arbitrary record extent inside a block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalRef {
    /// Exact object nonce.
    pub artifact: ArtifactId,
    /// File offset of the block header.
    pub offset: u64,
    /// Header plus payload length.
    pub length: u32,
    /// Required block kind.
    pub kind: BlockKind,
    /// Required block framing version (currently one).
    pub version: u16,
}

/// Validated container framing borrowing the caller's full immutable bytes.
/// Root-envelope success is not checkpoint or store admission.
#[derive(Debug)]
pub struct ArtifactFrame<'a> {
    bytes: &'a [u8],
    identity: ArtifactIdentity,
    kind: ContainerKind,
    directory: usize,
    count: usize,
    file_checksum: u64,
}

/// Copyable proof that one immutable artifact's complete framing and checksum
/// were validated. The owner must retain it beside the same immutable mapping.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ValidatedArtifact {
    identity: ArtifactIdentity,
    kind: ContainerKind,
    directory: usize,
    count: usize,
    file_length: usize,
    file_checksum: u64,
}
impl ArtifactFrame<'_> {
    /// Returns the validated store/object identity.
    pub const fn identity(&self) -> ArtifactIdentity {
        self.identity
    }
    /// Returns the required container family.
    pub const fn kind(&self) -> ContainerKind {
        self.kind
    }
    pub(crate) const fn validation(&self) -> ValidatedArtifact {
        ValidatedArtifact {
            identity: self.identity,
            kind: self.kind,
            directory: self.directory,
            count: self.count,
            file_length: self.bytes.len(),
            file_checksum: self.file_checksum,
        }
    }
    /// Returns one directory reference.
    pub fn reference(&self, index: usize) -> Result<PhysicalRef, FormatError> {
        if index >= self.count {
            return Err(invalid(
                FormatCheck::BlockLength,
                "directory index out of range",
            ));
        }
        let entry = add(self.directory, index.checked_mul(24).ok_or_else(overflow)?)?;
        Ok(PhysicalRef {
            artifact: self.identity.artifact,
            offset: frame::read_u64("native graph artifact", self.bytes, entry)?,
            length: frame::read_u32("native graph artifact", self.bytes, add(entry, 8)?)?,
            kind: block_kind(frame::read_u16(
                "native graph artifact",
                self.bytes,
                add(entry, 12)?,
            )?)?,
            version: frame::read_u16("native graph artifact", self.bytes, add(entry, 14)?)?,
        })
    }
    /// Resolves an exact framed-block reference after identity/kind validation.
    pub fn resolve_framed_block(&self, reference: PhysicalRef) -> Result<&[u8], FormatError> {
        validate_reference(reference)?;
        if reference.artifact != self.identity.artifact {
            return Err(invalid(
                FormatCheck::ObjectIdentity,
                "reference names another artifact",
            ));
        }
        let mut low = 0;
        let mut high = self.count;
        while low < high {
            let middle = low + (high - low) / 2;
            let candidate = self.reference(middle)?;
            match candidate.offset.cmp(&reference.offset) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal if candidate == reference => {
                    let start = usize_from(reference.offset)?;
                    return self
                        .bytes
                        .get(add(start, 24)?..add(start, reference.length as usize)?)
                        .ok_or_else(|| {
                            invalid(FormatCheck::BlockLength, "reference outside artifact")
                        });
                }
                std::cmp::Ordering::Equal => break,
            }
        }
        Err(invalid(
            FormatCheck::BlockLength,
            "reference is not an exact directory entry",
        ))
    }
}

impl ValidatedArtifact {
    pub(crate) fn framed_block<'a>(
        self,
        bytes: &'a [u8],
        reference: PhysicalRef,
    ) -> Result<FramedBlock<'a>, FormatError> {
        if bytes.len() != self.file_length {
            return Err(invalid(
                FormatCheck::FileLength,
                "validated artifact mapping length changed",
            ));
        }
        ArtifactFrame {
            bytes,
            identity: self.identity,
            kind: self.kind,
            directory: self.directory,
            count: self.count,
            file_checksum: self.file_checksum,
        }
        .framed_block(reference)
    }
}

impl ContainerKind {
    fn family(self) -> FormatFamily {
        match self {
            Self::Object => FormatFamily::NativeGraphObject,
            Self::RootEnvelope => FormatFamily::NativeGraphRoot,
        }
    }
}

/// Computes the complete caller-buffer reservation, including directory/trailer.
pub fn encoded_len(kind: ContainerKind, blocks: &[Block<'_>]) -> Result<usize, FormatError> {
    uncontrolled(encoded_len_with_control(kind, blocks, &mut |_| Ok(())))
}

/// Preflights geometry while polling before each input block descriptor.
pub fn encoded_len_with_control<E>(
    kind: ContainerKind,
    blocks: &[Block<'_>],
    control: &mut impl FnMut(usize) -> Result<(), E>,
) -> Result<usize, ArtifactControlError<E>> {
    poll(control, 0)?;
    if kind == ContainerKind::RootEnvelope && blocks.len() != 1 {
        return Err(ArtifactControlError::Format(invalid(
            FormatCheck::BlockLength,
            "root envelope requires one nonempty checkpoint payload",
        )));
    }
    if blocks.len() > MAX_BODY_BYTES / 48 {
        return Err(ArtifactControlError::Format(invalid(
            FormatCheck::BlockLength,
            "too many framed graph blocks",
        )));
    }
    let body_limit = MAX_BODY_BYTES - blocks.len() * 24;
    let mut body = 0_usize;
    for block in blocks {
        poll(control, std::mem::size_of::<Block<'_>>())?;
        if kind == ContainerKind::RootEnvelope
            && (block.kind != BlockKind::CheckpointPayload || block.payload.is_empty())
        {
            return Err(ArtifactControlError::Format(invalid(
                FormatCheck::BlockLength,
                "root envelope requires one nonempty checkpoint payload",
            )));
        }
        if kind == ContainerKind::Object && block.kind == BlockKind::CheckpointPayload {
            return Err(ArtifactControlError::Format(invalid(
                FormatCheck::Family,
                "checkpoint payload requires root envelope",
            )));
        }
        body = add(body, add(24, block.payload.len())?)?;
        if body > body_limit {
            return Err(ArtifactControlError::Format(invalid(
                FormatCheck::BlockLength,
                "complete graph artifact exceeds 4 MiB",
            )));
        }
    }
    let length = add(
        add(HEADER_BYTES, body)?,
        add(blocks.len().checked_mul(24).ok_or_else(overflow)?, 8)?,
    )?;
    poll(control, 0)?;
    Ok(length)
}

/// Writes an entire container into pre-reserved caller storage. Returns bytes used.
/// Preflight rejects malformed input and insufficient capacity before output changes.
pub fn encode_into(
    kind: ContainerKind,
    identity: ArtifactIdentity,
    blocks: &[Block<'_>],
    output: &mut [u8],
) -> Result<usize, FormatError> {
    uncontrolled(encode_into_with_control(
        kind,
        identity,
        blocks,
        output,
        &mut |_| Ok(()),
    ))
}

/// Encodes using bounded control callbacks before hashing, filling and copying.
/// Invalid input/capacity preflight leaves output unchanged. A later callback
/// failure can leave partially encoded private output; it does not roll it back.
/// The caller must discard that output instead of publishing it.
pub fn encode_into_with_control<E>(
    kind: ContainerKind,
    identity: ArtifactIdentity,
    blocks: &[Block<'_>],
    output: &mut [u8],
    control: &mut impl FnMut(usize) -> Result<(), E>,
) -> Result<usize, ArtifactControlError<E>> {
    let length = encoded_len_with_control(kind, blocks, control)?;
    if output.len() < length {
        return Err(ArtifactControlError::Format(invalid(
            FormatCheck::Length,
            "output reservation too small",
        )));
    }
    let directory_bytes = blocks.len().checked_mul(24).ok_or_else(overflow)?;
    let body = length - HEADER_BYTES - directory_bytes - 8;
    let target = output.get_mut(..length).ok_or_else(overflow)?;
    for chunk in target.chunks_mut(65_536) {
        poll(control, chunk.len())?;
        chunk.fill(0);
    }
    poll(control, HEADER_BYTES)?;
    encode_header(kind, identity, length, body, blocks.len(), target)?;
    let mut offset = HEADER_BYTES;
    for (index, block) in blocks.iter().enumerate() {
        let block_length = add(24, block.payload.len())?;
        let checksum = controlled_checksum(block.payload, control)?;
        poll(control, 24)?;
        put(target, offset, &(block.kind as u16).to_le_bytes())?;
        put(target, add(offset, 2)?, &1_u16.to_le_bytes())?;
        put(
            target,
            add(offset, 8)?,
            &(block.payload.len() as u64).to_le_bytes(),
        )?;
        put(target, add(offset, 16)?, &checksum.to_le_bytes())?;
        let payload_start = add(offset, 24)?;
        for (chunk_index, chunk) in block.payload.chunks(65_536).enumerate() {
            poll(control, chunk.len())?;
            put(
                target,
                add(
                    payload_start,
                    chunk_index.checked_mul(65_536).ok_or_else(overflow)?,
                )?,
                chunk,
            )?;
        }
        let directory = add(
            add(HEADER_BYTES, body)?,
            index.checked_mul(24).ok_or_else(overflow)?,
        )?;
        poll(control, 24)?;
        put(target, directory, &(offset as u64).to_le_bytes())?;
        put(
            target,
            add(directory, 8)?,
            &(block_length as u32).to_le_bytes(),
        )?;
        put(
            target,
            add(directory, 12)?,
            &(block.kind as u16).to_le_bytes(),
        )?;
        put(target, add(directory, 14)?, &1_u16.to_le_bytes())?;
        put(target, add(directory, 16)?, &checksum.to_le_bytes())?;
        offset = add(offset, block_length)?;
    }
    let checksum = controlled_checksum(target.get(..length - 8).ok_or_else(overflow)?, control)?;
    poll(control, 8)?;
    put(target, length - 8, &checksum.to_le_bytes())?;
    poll(control, 0)?;
    Ok(length)
}

fn encode_header(
    kind: ContainerKind,
    identity: ArtifactIdentity,
    length: usize,
    body: usize,
    count: usize,
    target: &mut [u8],
) -> Result<(), FormatError> {
    put(target, 0, &frame::FILE_MAGIC)?;
    put(target, 8, &kind.family().id().to_le_bytes())?;
    put(target, 10, &1_u16.to_le_bytes())?;
    put(target, 16, &(HEADER_BYTES as u64).to_le_bytes())?;
    put(target, 24, &(length as u64).to_le_bytes())?;
    put(target, 32, &identity.store.get().to_le_bytes())?;
    put(target, 48, &identity.artifact.get().to_le_bytes())?;
    put(target, 64, &identity.generation.get().to_le_bytes())?;
    put(target, 72, &(body as u64).to_le_bytes())?;
    put(target, 80, &(count as u32).to_le_bytes())?;
    put(target, 88, &identity.creation_serial.to_le_bytes())?;
    Ok(())
}

/// Validates required family, framing, identity, geometry and checksums before
/// returning borrowed payload access. Expected identity binds a referenced object;
/// root discovery may use None, then preserves the returned StoreInstanceId.
/// Root-envelope success does not validate the writes-owned checkpoint contents.
pub fn decode<'a>(
    kind: ContainerKind,
    expected: Option<(StoreInstanceId, ArtifactId)>,
    bytes: &'a [u8],
) -> Result<ArtifactFrame<'a>, FormatError> {
    uncontrolled(decode_with_control(kind, expected, bytes, &mut |_| Ok(())))
}

/// Admits immutable artifact framing with bounded cancellation/work callbacks.
/// Each callback precedes at most 64 KiB of hashing or fixed metadata visits.
/// Header/trailer and each block plus directory entry are charged separately
/// from checksum passes. A final zero-byte poll occurs before admission returns.
/// No bytes are copied or allocated on successful admission.
pub fn decode_with_control<'a, E>(
    kind: ContainerKind,
    expected: Option<(StoreInstanceId, ArtifactId)>,
    bytes: &'a [u8],
    control: &mut impl FnMut(usize) -> Result<(), E>,
) -> Result<ArtifactFrame<'a>, ArtifactControlError<E>> {
    poll(control, bytes.len().min(32))?;
    let header = frame::decode_header("native graph artifact", kind.family(), bytes)?;
    if header.flags != 0 || header.header_length != HEADER_BYTES as u64 {
        return Err(ArtifactControlError::Format(invalid(
            FormatCheck::HeaderLength,
            "invalid graph header flags or extent",
        )));
    }
    if bytes.len() > MAX_ARTIFACT_BYTES
        || bytes.len() < HEADER_BYTES + 8
        || header.file_length != bytes.len() as u64
    {
        return Err(ArtifactControlError::Format(invalid(
            FormatCheck::FileLength,
            "invalid graph file extent",
        )));
    }
    let trailer = bytes.len() - 8;
    poll(control, 8)?;
    let expected_checksum = frame::read_u64("native graph artifact", bytes, trailer)?;
    checksum(
        FormatCheck::FileChecksum,
        expected_checksum,
        controlled_checksum(bytes.get(..trailer).ok_or_else(overflow)?, control)?,
    )?;
    poll(control, HEADER_BYTES - 32)?;
    let store = StoreInstanceId::new(read_u128(bytes, 32)?)
        .map_err(|_| invalid(FormatCheck::ObjectIdentity, "zero store identity"))?;
    let artifact = ArtifactId::new(read_u128(bytes, 48)?)?;
    if expected.is_some_and(|value| value != (store, artifact)) {
        return Err(ArtifactControlError::Format(invalid(
            FormatCheck::ObjectIdentity,
            "wrong store or artifact identity",
        )));
    }
    let body = usize_from(frame::read_u64("native graph artifact", bytes, 72)?)?;
    let count = frame::read_u32("native graph artifact", bytes, 80)? as usize;
    if body > MAX_BODY_BYTES
        || count > body / 24
        || frame::read_u32("native graph artifact", bytes, 84)? != 0
    {
        return Err(ArtifactControlError::Format(invalid(
            FormatCheck::BlockLength,
            "invalid graph body extent or flags",
        )));
    }
    let directory = add(HEADER_BYTES, body)?;
    if add(directory, count.checked_mul(24).ok_or_else(overflow)?)? != trailer {
        return Err(ArtifactControlError::Format(invalid(
            FormatCheck::BlockLength,
            "directory does not consume complete artifact",
        )));
    }
    let frame = ArtifactFrame {
        bytes,
        identity: ArtifactIdentity {
            store,
            artifact,
            generation: GraphGeneration::new(frame::read_u64("native graph artifact", bytes, 64)?),
            creation_serial: frame::read_u64("native graph artifact", bytes, 88)?,
        },
        kind,
        directory,
        count,
        file_checksum: expected_checksum,
    };
    let mut next = HEADER_BYTES;
    for index in 0..count {
        poll(control, 24)?;
        let reference = frame.reference(index)?;
        validate_reference(reference)?;
        let offset = usize_from(reference.offset)?;
        let end = add(offset, reference.length as usize)?;
        if offset != next || end > directory {
            return Err(ArtifactControlError::Format(invalid(
                FormatCheck::BlockLength,
                "noncontiguous or overlapping graph blocks",
            )));
        }
        let entry_checksum = frame::read_u64(
            "native graph artifact",
            bytes,
            add(add(directory, index * 24)?, 16)?,
        )?;
        let payload = validate_block(bytes, reference, entry_checksum, control)?;
        if (kind == ContainerKind::RootEnvelope
            && (count != 1 || reference.kind != BlockKind::CheckpointPayload || payload.is_empty()))
            || (kind == ContainerKind::Object && reference.kind == BlockKind::CheckpointPayload)
        {
            return Err(ArtifactControlError::Format(invalid(
                FormatCheck::Family,
                "block is not allowed in this graph container",
            )));
        }
        next = end;
    }
    if next != directory || (kind == ContainerKind::RootEnvelope && count == 0) {
        return Err(ArtifactControlError::Format(invalid(
            FormatCheck::BlockLength,
            "unreferenced body or absent root payload",
        )));
    }
    poll(control, 0)?;
    Ok(frame)
}

fn validate_block<'a, E>(
    bytes: &'a [u8],
    reference: PhysicalRef,
    expected_checksum: u64,
    control: &mut impl FnMut(usize) -> Result<(), E>,
) -> Result<&'a [u8], ArtifactControlError<E>> {
    poll(control, 24)?;
    validate_reference(reference)?;
    let offset = usize_from(reference.offset)?;
    let end = add(offset, reference.length as usize)?;
    if frame::read_u16("native graph artifact", bytes, offset)? != reference.kind as u16
        || frame::read_u16("native graph artifact", bytes, add(offset, 2)?)? != reference.version
    {
        return Err(ArtifactControlError::Format(invalid(
            FormatCheck::Family,
            "directory kind/version differs from block",
        )));
    }
    if frame::read_u32("native graph artifact", bytes, add(offset, 4)?)? != 0
        || add(
            24,
            usize_from(frame::read_u64(
                "native graph artifact",
                bytes,
                add(offset, 8)?,
            )?)?,
        )? != reference.length as usize
    {
        return Err(ArtifactControlError::Format(invalid(
            FormatCheck::BlockLength,
            "invalid block flags or length",
        )));
    }
    let payload = bytes.get(add(offset, 24)?..end).ok_or_else(overflow)?;
    let block_checksum = frame::read_u64("native graph artifact", bytes, add(offset, 16)?)?;
    checksum(
        FormatCheck::BlockChecksum,
        expected_checksum,
        block_checksum,
    )?;
    checksum(
        FormatCheck::BlockChecksum,
        block_checksum,
        controlled_checksum(payload, control)?,
    )?;
    Ok(payload)
}

/// Serializes a checked, fixed-width physical block reference.
pub fn encode_reference(reference: PhysicalRef, output: &mut [u8]) -> Result<(), FormatError> {
    validate_reference(reference)?;
    if output.len() != 32 {
        return Err(invalid(
            FormatCheck::Length,
            "reference must occupy 32 bytes",
        ));
    }
    put(output, 0, &reference.artifact.get().to_le_bytes())?;
    put(output, 16, &reference.offset.to_le_bytes())?;
    put(output, 24, &reference.length.to_le_bytes())?;
    put(output, 28, &(reference.kind as u16).to_le_bytes())?;
    put(output, 30, &reference.version.to_le_bytes())
}

/// Parses framing geometry only; resolution must still bind the owning object.
pub fn decode_reference(bytes: &[u8]) -> Result<PhysicalRef, FormatError> {
    if bytes.len() != 32 {
        return Err(invalid(
            FormatCheck::Length,
            "reference must occupy 32 bytes",
        ));
    }
    let reference = PhysicalRef {
        artifact: ArtifactId::new(read_u128(bytes, 0)?)?,
        offset: frame::read_u64("graph reference", bytes, 16)?,
        length: frame::read_u32("graph reference", bytes, 24)?,
        kind: block_kind(frame::read_u16("graph reference", bytes, 28)?)?,
        version: frame::read_u16("graph reference", bytes, 30)?,
    };
    validate_reference(reference)?;
    Ok(reference)
}

fn validate_reference(reference: PhysicalRef) -> Result<(), FormatError> {
    if reference.version != 1 {
        return Err(invalid(
            FormatCheck::Version,
            "unsupported graph block version",
        ));
    }
    if reference.length < 24
        || reference.offset < HEADER_BYTES as u64
        || reference
            .offset
            .checked_add(u64::from(reference.length))
            .is_none_or(|end| end > MAX_ARTIFACT_BYTES as u64)
    {
        return Err(invalid(
            FormatCheck::BlockLength,
            "invalid framed-block reference extent",
        ));
    }
    Ok(())
}

fn block_kind(value: u16) -> Result<BlockKind, FormatError> {
    match value {
        1 => Ok(BlockKind::TreePage),
        2 => Ok(BlockKind::NodeRecord),
        3 => Ok(BlockKind::RelRecord),
        4 => Ok(BlockKind::CanonicalImage),
        5 => Ok(BlockKind::StoredText),
        6 => Ok(BlockKind::StoredVector),
        7 => Ok(BlockKind::OverflowKey),
        8 => Ok(BlockKind::ExtentList),
        9 => Ok(BlockKind::CheckpointPayload),
        10 => Ok(BlockKind::CommitParticipant),
        11 => Ok(BlockKind::PayloadChunk),
        12 => Ok(BlockKind::OperationProvenance),
        13 => Ok(BlockKind::AdjacencyBase),
        14 => Ok(BlockKind::AdjacencyDelta),
        15 => Ok(BlockKind::RetrievalRows),
        16 => Ok(BlockKind::RetrievalLexical),
        17 => Ok(BlockKind::RetrievalLiveRows),
        18 => Ok(BlockKind::RetrievalVectorIndex),
        _ => Err(invalid(
            FormatCheck::Family,
            "unknown required graph block kind",
        )),
    }
}

pub(super) fn read_u128(bytes: &[u8], offset: usize) -> Result<u128, FormatError> {
    let raw = bytes
        .get(offset..)
        .and_then(|tail| tail.first_chunk::<16>())
        .ok_or_else(overflow)?;
    Ok(u128::from_le_bytes(*raw))
}
pub(super) fn put(bytes: &mut [u8], offset: usize, value: &[u8]) -> Result<(), FormatError> {
    bytes
        .get_mut(offset..add(offset, value.len())?)
        .ok_or_else(overflow)?
        .copy_from_slice(value);
    Ok(())
}
pub(super) fn add(left: usize, right: usize) -> Result<usize, FormatError> {
    left.checked_add(right).ok_or_else(overflow)
}
pub(super) fn usize_from(value: u64) -> Result<usize, FormatError> {
    usize::try_from(value).map_err(|_| overflow())
}
fn overflow() -> FormatError {
    invalid(FormatCheck::Length, "graph extent overflow or truncation")
}
fn checksum(check: FormatCheck, expected: u64, actual: u64) -> Result<(), FormatError> {
    if expected == actual {
        Ok(())
    } else {
        Err(FormatError::checksum_mismatch(
            "native graph artifact",
            check,
            expected,
            actual,
        ))
    }
}

fn invalid(check: FormatCheck, detail: &str) -> FormatError {
    FormatError::new("native graph artifact", check, detail)
}

impl ArtifactFrame<'_> {
    /// Complete immutable bytes whose container framing was validated at decode.
    /// Callers may borrow them for descriptor/role checks; inner semantics are
    /// still the owning participant's responsibility.
    pub fn bytes(&self) -> &[u8] {
        self.bytes
    }
}

/// Exact validated block borrowed from an immutable, fully framed artifact.
/// Construction verifies directory membership; consumers still check that this
/// is the requested reference and apply their own inner semantic codec.
#[derive(Clone, Copy, Debug)]
pub struct FramedBlock<'a> {
    identity: ArtifactIdentity,
    reference: PhysicalRef,
    payload: &'a [u8],
    file_length: usize,
    file_checksum: u64,
}
impl<'a> FramedBlock<'a> {
    /// Complete identity of the containing immutable artifact.
    pub const fn identity(self) -> ArtifactIdentity {
        self.identity
    }
    /// Exact checked directory-member reference.
    pub const fn reference(self) -> PhysicalRef {
        self.reference
    }
    /// Inner payload, excluding the block header.
    pub const fn payload(self) -> &'a [u8] {
        self.payload
    }
    /// Complete validated container length for exact required descriptors.
    pub const fn file_length(self) -> usize {
        self.file_length
    }
    /// Complete validated container checksum from the admitted trailer.
    pub const fn file_checksum(self) -> u64 {
        self.file_checksum
    }
}
impl<'a> ArtifactFrame<'a> {
    /// Retains the underlying bytes' lifetime, independently of this temporary
    /// decoded frame. No unvalidated arbitrary byte slice can construct a block.
    pub fn framed_block(&self, reference: PhysicalRef) -> Result<FramedBlock<'a>, FormatError> {
        self.resolve_framed_block(reference)?;
        let start = usize_from(reference.offset)?;
        let payload = self
            .bytes
            .get(add(start, 24)?..add(start, reference.length as usize)?)
            .ok_or_else(|| invalid(FormatCheck::BlockLength, "framed block extent"))?;
        Ok(FramedBlock {
            identity: self.identity,
            reference,
            payload,
            file_length: self.bytes.len(),
            file_checksum: self.file_checksum,
        })
    }
}
