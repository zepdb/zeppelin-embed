//! Packed private object ownership and explicit abort inventory. Writes alone
//! exclusively creates files, synchronizes artifacts and publishes graph roots.
pub use super::artifact::OwnedArtifact;
use super::artifact::{
    ArtifactIdentity, BlockKind, FramedBlock, MAX_ARTIFACT_BYTES, PhysicalRef, PrivateArtifact,
};
use super::memory::{MAX_STORAGE_PREPARE_BYTES, StorageBuffer, StorageMemory};
use super::tree::directory::{BlockSink, BlockSource, TreeError, TreeResources};
use crate::property_graph::{GraphGeneration, StoreInstanceId};

/// Physical packing limits only; exhausting one pack starts another under the
/// same combined allowance. No logical request is silently split or published.
#[derive(Clone, Copy, Debug)]
pub struct PackLimits {
    /// Complete maximum bytes retained for each object (at most4MiB).
    pub artifact_bytes: usize,
    /// Maximum block descriptors in each packed object.
    pub blocks: usize,
}
impl Default for PackLimits {
    fn default() -> Self {
        Self {
            artifact_bytes: MAX_ARTIFACT_BYTES,
            blocks: 1024,
        }
    }
}

/// Immutable finalized object bytes that the sole coordinator may protect.
/// Constructed only after every private object has completed validation.
pub struct PreparedArtifact<'a> {
    identity: ArtifactIdentity,
    bytes: &'a [u8],
}
impl PreparedArtifact<'_> {
    /// Store/object/generation/creation serial retained without reinterpretation.
    pub const fn identity(&self) -> ArtifactIdentity {
        self.identity
    }
    /// Complete finalized file image, including framing and checksum trailer.
    pub const fn bytes(&self) -> &[u8] {
        self.bytes
    }
}

/// One complete private preparation's packed objects plus an immutable base.
/// All backing and inventory descriptors retain the same32MiB nested allowance.
/// The identity source is supplied by writes after allocating its checked serial;
/// exclusive filesystem creation remains mandatory before any commit protection.
pub struct PreparedObjects<'a, 'b, S, F> {
    packs: StorageBuffer<'a, PrivateArtifact<'a>>,
    base: &'b S,
    identity_source: F,
    store: StoreInstanceId,
    generation: GraphGeneration,
    limits: PackLimits,
    memory: &'a StorageMemory<'a>,
    finished: bool,
    failed: bool,
}
impl<'a, 'b, S: BlockSource, F: FnMut() -> Result<ArtifactIdentity, TreeError>>
    PreparedObjects<'a, 'b, S, F>
{
    /// Reserves the complete bounded pack/inventory descriptor capacity. No object
    /// identity is requested until the first append needs a new physical pack.
    /// The callback supplies an identity only: a serial may be burned before pack
    /// capacity/allocation succeeds, without transferring a file cleanup duty.
    /// Writes retains ownership of any path it creates before/inside that callback.
    /// Captured callback/base backing remains charged by its external writes owner;
    /// this participant charges its own pack, directory and inventory backing.
    pub fn new(
        base: &'b S,
        identity_source: F,
        store: StoreInstanceId,
        generation: GraphGeneration,
        limits: PackLimits,
        memory: &'a StorageMemory<'a>,
        r: &mut TreeResources<'_>,
    ) -> Result<Self, TreeError> {
        r.require_preparation(memory)?;
        r.step(0)?;
        if !(152..=MAX_ARTIFACT_BYTES).contains(&limits.artifact_bytes)
            || limits.blocks == 0
            || limits.blocks > super::artifact::MAX_BODY_BYTES / 48
        {
            return Err(TreeError::Memory);
        }
        let packs = StorageBuffer::new(memory, MAX_STORAGE_PREPARE_BYTES / limits.artifact_bytes)?;
        Ok(Self {
            packs,
            base,
            identity_source,
            store,
            generation,
            limits,
            memory,
            finished: false,
            failed: false,
        })
    }
    /// Number of real private object identities currently retained.
    pub fn len(&self) -> usize {
        self.packs.as_slice().len()
    }
    /// Whether this preparation allocated no object identity.
    pub fn is_empty(&self) -> bool {
        self.packs.as_slice().is_empty()
    }
    /// Exact owned candidates including an interrupted or failed current pack.
    /// This iterator allocates nothing and never includes base/published objects
    /// or identity-only callback results for which no private pack was allocated.
    pub fn abort_inventory(&self) -> impl Iterator<Item = ArtifactIdentity> + '_ {
        self.packs.as_slice().iter().map(PrivateArtifact::identity)
    }
    /// Seal all objects before allowing any complete prepared artifact to escape.
    pub fn finish(&mut self, r: &mut TreeResources<'_>) -> Result<(), TreeError> {
        r.require_preparation(self.memory)?;
        r.step(0)?;
        if self.failed || self.finished {
            return Err(TreeError::Invalid("prepared objects cannot finish"));
        }
        self.failed = true;
        for pack in self.packs.as_mut_slice() {
            if pack.sealed_bytes().is_none() {
                pack.seal(r)?;
            }
        }
        r.step(0)?;
        self.failed = false;
        self.finished = true;
        Ok(())
    }
    /// Borrow one complete prepared artifact only after successful finalization.
    pub fn artifact(&self, index: usize) -> Result<PreparedArtifact<'_>, TreeError> {
        if self.failed || !self.finished {
            return Err(TreeError::Invalid("unfinalized prepared objects"));
        }
        let pack = self
            .packs
            .as_slice()
            .get(index)
            .ok_or(TreeError::Invalid("prepared object index"))?;
        let bytes = pack
            .sealed_bytes()
            .ok_or(TreeError::Invalid("unsealed prepared object"))?;
        Ok(PreparedArtifact {
            identity: pack.identity(),
            bytes,
        })
    }
    fn append_inner(
        &mut self,
        kind: BlockKind,
        bytes: &[u8],
        r: &mut TreeResources<'_>,
    ) -> Result<PhysicalRef, TreeError> {
        if bytes
            .len()
            .checked_add(152)
            .is_none_or(|n| n > self.limits.artifact_bytes)
        {
            return Err(TreeError::Memory);
        }
        if self
            .packs
            .as_slice()
            .last()
            .is_none_or(|pack| !pack.can_append(bytes.len()))
        {
            if let Some(last) = self.packs.as_mut_slice().last_mut() {
                last.seal(r)?;
            }
            if self.packs.as_slice().len() == self.packs.capacity() {
                return Err(TreeError::Memory);
            }
            r.step(0)?;
            let identity = (self.identity_source)()?;
            if identity.store != self.store || identity.generation != self.generation {
                return Err(TreeError::Invalid("candidate identity store/generation"));
            }
            if self.packs.as_slice().last().is_some_and(|previous| {
                previous.identity().creation_serial >= identity.creation_serial
            }) {
                return Err(TreeError::Invalid(
                    "candidate creation serial did not advance",
                ));
            }
            for pack in self.packs.as_slice() {
                r.step(1)?;
                if pack.identity().artifact == identity.artifact {
                    return Err(TreeError::Invalid("reused private artifact identity"));
                }
            }
            let pack = PrivateArtifact::new(
                identity,
                self.limits.artifact_bytes,
                self.limits.blocks,
                self.memory,
                r,
            )?;
            self.packs.push(pack)?;
        }
        self.packs
            .as_mut_slice()
            .last_mut()
            .ok_or(TreeError::Memory)?
            .append(kind, bytes, r)
    }
}
impl<S: BlockSource, F> BlockSource for PreparedObjects<'_, '_, S, F> {
    fn resolve<'a>(
        &'a self,
        reference: PhysicalRef,
        r: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        r.require_preparation(self.memory)?;
        for pack in self.packs.as_slice() {
            r.step(1)?;
            if pack.identity().artifact == reference.artifact {
                return pack.framed_block(reference, r);
            }
        }
        self.base.resolve(reference, r)
    }
}
impl<S: BlockSource, F: FnMut() -> Result<ArtifactIdentity, TreeError>> BlockSink
    for PreparedObjects<'_, '_, S, F>
{
    fn append(
        &mut self,
        kind: BlockKind,
        generation: GraphGeneration,
        bytes: &[u8],
        r: &mut TreeResources<'_>,
    ) -> Result<PhysicalRef, TreeError> {
        r.require_preparation(self.memory)?;
        r.step(0)?;
        if self.failed || self.finished || generation != self.generation {
            return Err(TreeError::Invalid(
                "invalid private append state/generation",
            ));
        }
        self.failed = true;
        let reference = self.append_inner(kind, bytes, r)?;
        r.step(0)?;
        self.failed = false;
        Ok(reference)
    }
}
