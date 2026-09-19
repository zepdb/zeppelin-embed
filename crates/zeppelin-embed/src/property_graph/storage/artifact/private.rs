//! Charged private packing capability; its partial envelope is never published.
use super::*;
use crate::property_graph::storage::memory::{StorageBuffer, StorageMemory};
use crate::property_graph::storage::tree::directory::{TreeError, TreeResources};

#[derive(Clone, Copy)]
// Constructed only after complete frame/hash validation of this pack's immutable
// prefix. The private owner never exposes mutable bytes or rewrites those blocks.
struct VerifiedEntry {
    reference: PhysicalRef,
    checksum: u64,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum State {
    Open,
    Failed,
    Sealed,
}

/// An append-only private object under one bounded preparation. Block bytes never
/// change once appended. Only complete sealing grants access to container bytes;
/// cancellation can leave a failed private body requiring whole-object abort.
/// This owns no filesystem path, sync or publication authority.
pub struct PrivateArtifact<'a> {
    bytes: StorageBuffer<'a, u8>,
    entries: StorageBuffer<'a, VerifiedEntry>,
    identity: ArtifactIdentity,
    memory: &'a StorageMemory<'a>,
    state: State,
}
impl<'a> PrivateArtifact<'a> {
    /// Reserve complete object and directory capacities before writing any block.
    pub fn new(
        identity: ArtifactIdentity,
        capacity: usize,
        blocks: usize,
        memory: &'a StorageMemory<'a>,
        r: &mut TreeResources<'_>,
    ) -> Result<Self, TreeError> {
        r.require_preparation(memory)?;
        if !(HEADER_BYTES + 8..=MAX_ARTIFACT_BYTES).contains(&capacity)
            || blocks > MAX_BODY_BYTES / 48
        {
            return Err(TreeError::Memory);
        }
        let mut bytes = StorageBuffer::new(memory, capacity)?;
        let entries = StorageBuffer::new(memory, blocks)?;
        r.step(HEADER_BYTES as u64)?;
        bytes.extend_from_slice(&[0; HEADER_BYTES])?;
        Ok(Self {
            bytes,
            entries,
            identity,
            memory,
            state: State::Open,
        })
    }
    /// Exact private allocation identity for the enclosing abort inventory.
    pub const fn identity(&self) -> ArtifactIdentity {
        self.identity
    }
    /// Full retained object and descriptor capacities, including unused backing.
    pub fn owned_bytes(&self) -> usize {
        self.bytes.owned_bytes() + self.entries.owned_bytes()
    }
    /// Whether this input fits without replacing or growing any live allocation.
    pub fn can_append(&self, payload_bytes: usize) -> bool {
        self.state == State::Open
            && self.entries.as_slice().len() < self.entries.capacity()
            && self
                .bytes
                .as_slice()
                .len()
                .checked_add(24)
                .and_then(|n| n.checked_add(payload_bytes))
                .and_then(|n| n.checked_add((self.entries.as_slice().len() + 1) * 24 + 8))
                .is_some_and(|n| n <= self.bytes.capacity())
    }
    /// Append one immutable framed block. Insufficient capacity leaves this pack
    /// usable; a control error after mutation permanently prevents sealing it.
    pub fn append(
        &mut self,
        kind: BlockKind,
        payload: &[u8],
        r: &mut TreeResources<'_>,
    ) -> Result<PhysicalRef, TreeError> {
        r.require_preparation(self.memory)?;
        r.step(0)?;
        if self.state != State::Open {
            return Err(TreeError::Invalid("private artifact is not open"));
        }
        if kind == BlockKind::CheckpointPayload {
            return Err(TreeError::Invalid("checkpoint requires root envelope"));
        }
        if !self.can_append(payload.len()) {
            return Err(TreeError::Memory);
        }
        let checksum =
            controlled_checksum(payload, &mut |n| r.step(n as u64)).map_err(tree_error)?;
        let reference = PhysicalRef {
            artifact: self.identity.artifact,
            offset: self.bytes.as_slice().len() as u64,
            length: u32::try_from(payload.len() + 24).map_err(|_| TreeError::Memory)?,
            kind,
            version: 1,
        };
        let mut header = [0; 24];
        put(&mut header, 0, &(kind as u16).to_le_bytes())?;
        put(&mut header, 2, &1u16.to_le_bytes())?;
        put(&mut header, 8, &(payload.len() as u64).to_le_bytes())?;
        put(&mut header, 16, &checksum.to_le_bytes())?;
        self.state = State::Failed;
        r.step(24)?;
        self.bytes.extend_from_slice(&header)?;
        for chunk in payload.chunks(65_536) {
            r.step(chunk.len() as u64)?;
            self.bytes.extend_from_slice(chunk)?;
        }
        validate_block(self.bytes.as_slice(), reference, checksum, &mut |n| {
            r.step(n as u64)
        })
        .map_err(tree_error)?;
        r.step(std::mem::size_of::<VerifiedEntry>() as u64)?;
        self.entries.push(VerifiedEntry {
            reference,
            checksum,
        })?;
        r.step(0)?;
        self.state = State::Open;
        Ok(reference)
    }
    /// Resolve an exact reference admitted by complete append-time frame/hash
    /// validation. Later appends and sealing preserve its immutable payload bytes.
    /// Rust borrowing forbids an append while these immutable bytes are borrowed.
    pub fn framed_block(
        &self,
        reference: PhysicalRef,
        r: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'_>, TreeError> {
        r.require_preparation(self.memory)?;
        if self.state == State::Failed || reference.artifact != self.identity.artifact {
            return Err(TreeError::Invalid("failed or substituted private artifact"));
        }
        let (mut low, mut high) = (0, self.entries.as_slice().len());
        while low < high {
            r.step(std::mem::size_of::<VerifiedEntry>() as u64)?;
            let middle = low + (high - low) / 2;
            let entry = self
                .entries
                .as_slice()
                .get(middle)
                .ok_or(TreeError::Memory)?;
            match entry.reference.offset.cmp(&reference.offset) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal if entry.reference == reference => {
                    // Entry construction is the complete validation boundary;
                    // exact reference matching binds it to this immutable prefix.
                    let start = usize_from(reference.offset)?;
                    let payload = self
                        .bytes
                        .as_slice()
                        .get(add(start, 24)?..add(start, reference.length as usize)?)
                        .ok_or(TreeError::Invalid("admitted private block extent"))?;
                    r.step(0)?;
                    return Ok(FramedBlock {
                        identity: self.identity,
                        reference,
                        payload,
                    });
                }
                std::cmp::Ordering::Equal => break,
            }
        }
        Err(TreeError::Invalid(
            "private reference is not an exact directory entry",
        ))
    }
    /// Finish the one immutable container and verify the shared complete codec.
    /// Failure retains this identity and private bytes for explicit abort handling.
    pub fn seal(&mut self, r: &mut TreeResources<'_>) -> Result<(), TreeError> {
        r.require_preparation(self.memory)?;
        r.step(0)?;
        if self.state != State::Open {
            return Err(TreeError::Invalid("private artifact cannot seal"));
        }
        self.state = State::Failed;
        let body = self.bytes.as_slice().len() - HEADER_BYTES;
        for entry in self.entries.as_slice() {
            let mut bytes = [0; 24];
            put(&mut bytes, 0, &entry.reference.offset.to_le_bytes())?;
            put(&mut bytes, 8, &entry.reference.length.to_le_bytes())?;
            put(&mut bytes, 12, &(entry.reference.kind as u16).to_le_bytes())?;
            put(&mut bytes, 14, &entry.reference.version.to_le_bytes())?;
            put(&mut bytes, 16, &entry.checksum.to_le_bytes())?;
            r.step(24)?;
            self.bytes.extend_from_slice(&bytes)?;
        }
        let length = self
            .bytes
            .as_slice()
            .len()
            .checked_add(8)
            .ok_or(TreeError::Memory)?;
        r.step(HEADER_BYTES as u64)?;
        encode_header(
            ContainerKind::Object,
            self.identity,
            length,
            body,
            self.entries.as_slice().len(),
            self.bytes.as_mut_slice(),
        )?;
        let checksum = controlled_checksum(self.bytes.as_slice(), &mut |n| r.step(n as u64))
            .map_err(tree_error)?;
        r.step(8)?;
        self.bytes.extend_from_slice(&checksum.to_le_bytes())?;
        decode_with_control(
            ContainerKind::Object,
            Some((self.identity.store, self.identity.artifact)),
            self.bytes.as_slice(),
            &mut |n| r.step(n as u64),
        )
        .map_err(tree_error)?;
        r.step(0)?;
        self.state = State::Sealed;
        Ok(())
    }
    /// Complete immutable bytes only after successful final validation.
    pub fn sealed_bytes(&self) -> Option<&[u8]> {
        (self.state == State::Sealed).then(|| self.bytes.as_slice())
    }
}
fn tree_error(error: ArtifactControlError<TreeError>) -> TreeError {
    match error {
        ArtifactControlError::Format(error) => TreeError::Format(error),
        ArtifactControlError::Control(error) => error,
    }
}

/// Fully framed immutable bytes retained under preparation ownership. This is
/// an artifact validation capability, not a coherent graph read-view lease.
/// Its complete decoder is run exactly once before cached resolution is allowed.
pub struct OwnedArtifact<'a> {
    bytes: StorageBuffer<'a, u8>,
    identity: ArtifactIdentity,
    directory: usize,
    count: usize,
}
impl<'a> OwnedArtifact<'a> {
    /// Read one complete physical object in controlled <=64KiB spans. A length
    /// mismatch, trailing byte, identity mismatch or malformed frame rejects.
    pub fn read_from(
        reader: &mut impl std::io::Read,
        length: usize,
        expected: (StoreInstanceId, ArtifactId),
        memory: &'a StorageMemory<'a>,
        r: &mut TreeResources<'_>,
    ) -> Result<Self, TreeError> {
        r.require_preparation(memory)?;
        if !(HEADER_BYTES + 8..=MAX_ARTIFACT_BYTES).contains(&length) {
            return Err(TreeError::Invalid("owned artifact length"));
        }
        let mut bytes = StorageBuffer::new(memory, length)?;
        let mut scratch = [0; 65_536];
        while bytes.as_slice().len() < length {
            let count = (length - bytes.as_slice().len()).min(scratch.len());
            let chunk = scratch.get_mut(..count).ok_or(TreeError::Memory)?;
            // read_exact retries Interrupted internally without a checkpoint.
            // Poll each physical attempt, including zero-progress interruptions,
            // and charge only the bytes the reader actually returned.
            r.step(0)?;
            let read = match reader.read(chunk) {
                Ok(0) => return Err(TreeError::Io(std::io::ErrorKind::UnexpectedEof.into())),
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(TreeError::Io(error)),
            };
            let chunk = chunk
                .get(..read)
                .ok_or(TreeError::Invalid("reader exceeded output"))?;
            r.step(read as u64)?;
            r.step(read as u64)?;
            bytes.extend_from_slice(chunk)?;
        }
        let mut trailing = [0];
        loop {
            r.step(0)?;
            match reader.read(&mut trailing) {
                Ok(0) => break,
                Ok(_) => return Err(TreeError::Invalid("trailing artifact bytes")),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(TreeError::Io(error)),
            }
        }
        let frame = decode_with_control(
            ContainerKind::Object,
            Some(expected),
            bytes.as_slice(),
            &mut |n| r.step(n as u64),
        )
        .map_err(tree_error)?;
        let (identity, directory, count) = (frame.identity, frame.directory, frame.count);
        r.step(0)?;
        Ok(Self {
            bytes,
            identity,
            directory,
            count,
        })
    }
    /// Exact identity validated before this owner was constructed.
    pub const fn identity(&self) -> ArtifactIdentity {
        self.identity
    }
    /// Complete actual retained backing capacity.
    pub fn owned_bytes(&self) -> usize {
        self.bytes.owned_bytes()
    }
}
impl crate::property_graph::storage::tree::directory::BlockSource for OwnedArtifact<'_> {
    fn resolve<'a>(
        &'a self,
        reference: PhysicalRef,
        r: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        // One bounded binary search over immutable already validated entries.
        // There is no disk access, allocation or new view admission here.
        r.step(1)?;
        let frame = ArtifactFrame {
            bytes: self.bytes.as_slice(),
            identity: self.identity,
            kind: ContainerKind::Object,
            directory: self.directory,
            count: self.count,
        };
        let block = frame.framed_block(reference)?;
        r.step(0)?;
        Ok(block)
    }
}
