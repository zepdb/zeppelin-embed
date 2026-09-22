use crate::format::FormatFamily;
use crate::property_graph::storage::artifact::{self, ArtifactId, BlockKind};
use crate::property_graph::storage::tree::directory::TreeError;
use crate::property_graph::wal::{ArtifactDescriptor, RequiredRef};
use crate::property_graph::{GraphGeneration, StoreInstanceId};
use xxhash_rust::xxh3::Xxh3;

const REQUIRED_BYTES: usize = 96;
const DESCRIPTOR_BYTES: usize = 64;
pub(super) const PARTIAL_TARGET_BYTES: usize = 80;
pub(super) const PROTECTED_PAGE_HEADER_BYTES: usize = 224;
pub(super) const PROTECTED_STREAM_RECORD_BYTES: usize = 112;
pub(super) const PROTECTED_STREAM_PAGE_RECORDS: usize = 32;
const PENDING_INTENT_HEADER_BYTES: usize = 368;
const COMPLETION_HEADER_BYTES: usize = 200;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub(crate) enum ProofRole {
    ProtectedRoots = 3,
    CompletedMark = 4,
    ReclaimState = 5,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum ProtectedClass {
    Current = 1,
    Checkpoint = 2,
    Wal = 3,
    Reader = 4,
    PreparedBase = 5,
    PreparedAllocation = 6,
    Proof = 7,
    InFlight = 8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProtectedValue {
    Required(RequiredRef),
    Descriptor(ArtifactDescriptor),
    WalAuthority {
        identity: u128,
        first_sequence: u64,
        bytes: u64,
    },
    CapturedState {
        checkpoint: RequiredRef,
        sequence: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProtectedRecord {
    pub(crate) class: ProtectedClass,
    pub(crate) value: ProtectedValue,
}

impl ProtectedRecord {
    pub(crate) const fn required(class: ProtectedClass, required: RequiredRef) -> Self {
        Self {
            class,
            value: ProtectedValue::Required(required),
        }
    }

    pub(crate) const fn descriptor(class: ProtectedClass, value: ArtifactDescriptor) -> Self {
        Self {
            class,
            value: ProtectedValue::Descriptor(value),
        }
    }

    pub(crate) const fn captured_state(
        class: ProtectedClass,
        checkpoint: RequiredRef,
        sequence: u64,
    ) -> Self {
        Self {
            class,
            value: ProtectedValue::CapturedState {
                checkpoint,
                sequence,
            },
        }
    }

    pub(crate) fn artifact(self) -> Result<ArtifactId, TreeError> {
        match self.value {
            ProtectedValue::Required(required) => Ok(required.object.artifact),
            ProtectedValue::Descriptor(descriptor) => Ok(descriptor.artifact),
            ProtectedValue::WalAuthority { identity, .. } => Ok(ArtifactId::new(identity)?),
            ProtectedValue::CapturedState { checkpoint, .. } => Ok(checkpoint.object.artifact),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ProtectedPage {
    pub(super) previous: Option<RequiredRef>,
    pub(super) ordinal: u64,
    pub(super) cumulative_count: u64,
    pub(super) prior_digest: u64,
    pub(super) digest: u64,
    pub(super) count: usize,
}

pub(super) fn encode_protected_page(
    binding: super::SpillBinding,
    previous: Option<RequiredRef>,
    ordinal: u64,
    cumulative_before: u64,
    prior_digest: u64,
    records: &[ProtectedRecord],
    output: &mut [u8],
) -> Result<(usize, u64), TreeError> {
    if records.is_empty()
        || records.len() > PROTECTED_STREAM_PAGE_RECORDS
        || previous.is_some() != (ordinal != 0)
    {
        return Err(TreeError::Invalid("protected stream page geometry"));
    }
    let cumulative_count = cumulative_before
        .checked_add(u64::try_from(records.len()).map_err(|_| TreeError::Memory)?)
        .ok_or(TreeError::Memory)?;
    let length = PROTECTED_PAGE_HEADER_BYTES
        .checked_add(
            records
                .len()
                .checked_mul(PROTECTED_STREAM_RECORD_BYTES)
                .ok_or(TreeError::Memory)?,
        )
        .ok_or(TreeError::Memory)?;
    let target = output.get_mut(..length).ok_or(TreeError::Memory)?;
    target.fill(0);
    put(target, 0, b"ZGCP")?;
    put(target, 4, &(ProofRole::ProtectedRoots as u16).to_le_bytes())?;
    put(target, 6, &1_u16.to_le_bytes())?;
    put(target, 8, &1_u16.to_le_bytes())?;
    put(target, 10, &u16::from(previous.is_some()).to_le_bytes())?;
    put(
        target,
        12,
        &u32::try_from(length)
            .map_err(|_| TreeError::Memory)?
            .to_le_bytes(),
    )?;
    encode_spill_binding(target, binding)?;
    put(target, 80, &ordinal.to_le_bytes())?;
    put(target, 88, &cumulative_count.to_le_bytes())?;
    put(target, 96, &prior_digest.to_le_bytes())?;
    put(
        target,
        112,
        &u32::try_from(records.len())
            .map_err(|_| TreeError::Memory)?
            .to_le_bytes(),
    )?;
    put(
        target,
        116,
        &(PROTECTED_STREAM_RECORD_BYTES as u32).to_le_bytes(),
    )?;
    if let Some(previous) = previous {
        put_required(target, 120, previous)?;
    }
    for (index, record) in records.iter().copied().enumerate() {
        encode_protected_record(
            record,
            binding,
            target,
            PROTECTED_PAGE_HEADER_BYTES + index * PROTECTED_STREAM_RECORD_BYTES,
        )?;
    }
    let mut hasher = Xxh3::new();
    hasher.update(&prior_digest.to_le_bytes());
    hasher.update(
        target
            .get(PROTECTED_PAGE_HEADER_BYTES..length)
            .ok_or(TreeError::Memory)?,
    );
    let digest = hasher.digest();
    put(target, 104, &digest.to_le_bytes())?;
    Ok((length, digest))
}

pub(super) fn decode_protected_page(
    bytes: &[u8],
    binding: super::SpillBinding,
) -> Result<ProtectedPage, TreeError> {
    if bytes.len() < PROTECTED_PAGE_HEADER_BYTES
        || bytes.get(..4) != Some(b"ZGCP".as_slice())
        || read_u16(bytes, 4)? != ProofRole::ProtectedRoots as u16
        || read_u16(bytes, 6)? != 1
        || read_u16(bytes, 8)? != 1
        || usize::try_from(read_u32(bytes, 12)?).map_err(|_| TreeError::Memory)? != bytes.len()
        || bytes.get(216..224) != Some([0_u8; 8].as_slice())
    {
        return Err(TreeError::Invalid("protected stream page header"));
    }
    validate_spill_binding(bytes, binding)?;
    let ordinal = read_u64(bytes, 80)?;
    let cumulative_count = read_u64(bytes, 88)?;
    let prior_digest = read_u64(bytes, 96)?;
    let digest = read_u64(bytes, 104)?;
    let count = usize::try_from(read_u32(bytes, 112)?).map_err(|_| TreeError::Memory)?;
    let has_previous = match read_u16(bytes, 10)? {
        0 => false,
        1 => true,
        _ => return Err(TreeError::Invalid("protected stream predecessor flag")),
    };
    if count == 0
        || count > PROTECTED_STREAM_PAGE_RECORDS
        || read_u32(bytes, 116)? as usize != PROTECTED_STREAM_RECORD_BYTES
        || has_previous != (ordinal != 0)
        || cumulative_count < u64::try_from(count).map_err(|_| TreeError::Memory)?
        || bytes.len()
            != PROTECTED_PAGE_HEADER_BYTES
                .checked_add(
                    count
                        .checked_mul(PROTECTED_STREAM_RECORD_BYTES)
                        .ok_or(TreeError::Memory)?,
                )
                .ok_or(TreeError::Memory)?
    {
        return Err(TreeError::Invalid("protected stream page geometry"));
    }
    let previous = if has_previous {
        Some(required(bytes, 120)?)
    } else {
        if bytes.get(120..216) != Some([0_u8; 96].as_slice()) {
            return Err(TreeError::Invalid("protected stream empty predecessor"));
        }
        None
    };
    let mut hasher = Xxh3::new();
    hasher.update(&prior_digest.to_le_bytes());
    hasher.update(
        bytes
            .get(PROTECTED_PAGE_HEADER_BYTES..)
            .ok_or(TreeError::Invalid("protected stream page body"))?,
    );
    if hasher.digest() != digest {
        return Err(TreeError::Invalid("protected stream page digest"));
    }
    for index in 0..count {
        let _ = protected_record_at(bytes, index, binding)?;
    }
    Ok(ProtectedPage {
        previous,
        ordinal,
        cumulative_count,
        prior_digest,
        digest,
        count,
    })
}

pub(super) fn protected_record_at(
    bytes: &[u8],
    index: usize,
    binding: super::SpillBinding,
) -> Result<ProtectedRecord, TreeError> {
    let count = usize::try_from(read_u32(bytes, 112)?).map_err(|_| TreeError::Memory)?;
    if index >= count {
        return Err(TreeError::Invalid("protected stream record index"));
    }
    let start = PROTECTED_PAGE_HEADER_BYTES
        .checked_add(
            index
                .checked_mul(PROTECTED_STREAM_RECORD_BYTES)
                .ok_or(TreeError::Memory)?,
        )
        .ok_or(TreeError::Memory)?;
    let class = match byte(bytes, start)? {
        1 => ProtectedClass::Current,
        2 => ProtectedClass::Checkpoint,
        3 => ProtectedClass::Wal,
        4 => ProtectedClass::Reader,
        5 => ProtectedClass::PreparedBase,
        6 => ProtectedClass::PreparedAllocation,
        7 => ProtectedClass::Proof,
        8 => ProtectedClass::InFlight,
        _ => return Err(TreeError::Invalid("protected stream record class")),
    };
    if bytes.get(start + 2..start + 8) != Some([0_u8; 6].as_slice()) {
        return Err(TreeError::Invalid("protected stream record reserved"));
    }
    let value = match byte(bytes, start + 1)? {
        1 => {
            if bytes.get(start + 104..start + 112) != Some([0_u8; 8].as_slice()) {
                return Err(TreeError::Invalid("protected required record reserved"));
            }
            let required = protected_required(bytes, start + 8)?;
            if required.object.store != binding.store {
                return Err(TreeError::Invalid("protected required record store"));
            }
            ProtectedValue::Required(required)
        }
        2 => {
            if bytes.get(start + 72..start + 112) != Some([0_u8; 40].as_slice()) {
                return Err(TreeError::Invalid("protected descriptor record reserved"));
            }
            let descriptor = descriptor(bytes, start + 8)?;
            if descriptor.store != binding.store {
                return Err(TreeError::Invalid("protected descriptor record store"));
            }
            ProtectedValue::Descriptor(descriptor)
        }
        3 => {
            if class != ProtectedClass::Wal
                || bytes.get(start + 40..start + 112) != Some([0_u8; 72].as_slice())
            {
                return Err(TreeError::Invalid("protected WAL authority record"));
            }
            let identity = read_u128(bytes, start + 8)?;
            let first_sequence = read_u64(bytes, start + 24)?;
            let extent = read_u64(bytes, start + 32)?;
            if identity == 0 || extent == 0 {
                return Err(TreeError::Invalid("protected WAL authority fields"));
            }
            ProtectedValue::WalAuthority {
                identity,
                first_sequence,
                bytes: extent,
            }
        }
        4 => {
            if !matches!(
                class,
                ProtectedClass::Current | ProtectedClass::Reader | ProtectedClass::PreparedBase
            ) {
                return Err(TreeError::Invalid("captured state record class"));
            }
            let checkpoint = protected_required(bytes, start + 8)?;
            let sequence = read_u64(bytes, start + 104)?;
            if checkpoint.object.family != FormatFamily::NativeGraphRoot.id()
                || checkpoint.object.version != 1
                || checkpoint.block.kind != BlockKind::CheckpointPayload
                || checkpoint.block.version != 1
                || checkpoint.object.artifact != checkpoint.block.artifact
                || checkpoint.object.store != binding.store
            {
                return Err(TreeError::Invalid("captured state record fields"));
            }
            ProtectedValue::CapturedState {
                checkpoint,
                sequence,
            }
        }
        _ => return Err(TreeError::Invalid("protected stream record value")),
    };
    Ok(ProtectedRecord { class, value })
}

fn encode_protected_record(
    record: ProtectedRecord,
    binding: super::SpillBinding,
    output: &mut [u8],
    start: usize,
) -> Result<(), TreeError> {
    put(output, start, &[record.class as u8])?;
    match record.value {
        ProtectedValue::Required(required) => {
            if required.object.store != binding.store {
                return Err(TreeError::Invalid("protected required record store"));
            }
            put(output, start + 1, &[1])?;
            put_required(output, start + 8, required)?;
        }
        ProtectedValue::Descriptor(descriptor) => {
            if descriptor.store != binding.store {
                return Err(TreeError::Invalid("protected descriptor record store"));
            }
            put(output, start + 1, &[2])?;
            put_descriptor(output, start + 8, descriptor)?;
        }
        ProtectedValue::WalAuthority {
            identity,
            first_sequence,
            bytes,
        } => {
            if record.class != ProtectedClass::Wal || identity == 0 || bytes == 0 {
                return Err(TreeError::Invalid("protected WAL authority fields"));
            }
            put(output, start + 1, &[3])?;
            put(output, start + 8, &identity.to_le_bytes())?;
            put(output, start + 24, &first_sequence.to_le_bytes())?;
            put(output, start + 32, &bytes.to_le_bytes())?;
        }
        ProtectedValue::CapturedState {
            checkpoint,
            sequence,
        } => {
            if !matches!(
                record.class,
                ProtectedClass::Current | ProtectedClass::Reader | ProtectedClass::PreparedBase
            ) || checkpoint.object.store != binding.store
                || checkpoint.object.family != FormatFamily::NativeGraphRoot.id()
                || checkpoint.object.version != 1
                || checkpoint.block.kind != BlockKind::CheckpointPayload
                || checkpoint.block.version != 1
                || checkpoint.object.artifact != checkpoint.block.artifact
            {
                return Err(TreeError::Invalid("captured state record fields"));
            }
            put(output, start + 1, &[4])?;
            put_required(output, start + 8, checkpoint)?;
            put(output, start + 104, &sequence.to_le_bytes())?;
        }
    }
    Ok(())
}

fn encode_spill_binding(output: &mut [u8], binding: super::SpillBinding) -> Result<(), TreeError> {
    put(output, 16, &binding.store.get().to_le_bytes())?;
    put(output, 32, &binding.session.get().to_le_bytes())?;
    put(output, 48, &binding.capture_generation.get().to_le_bytes())?;
    put(output, 56, &binding.target_generation.get().to_le_bytes())?;
    put(output, 64, &binding.sequence.to_le_bytes())?;
    put(output, 72, &binding.serial_fence.to_le_bytes())
}

fn decode_spill_binding(bytes: &[u8]) -> Result<super::SpillBinding, TreeError> {
    Ok(super::SpillBinding {
        store: StoreInstanceId::new(read_u128(bytes, 16)?)
            .map_err(|_| TreeError::Invalid("reclaim spill store"))?,
        session: ArtifactId::new(read_u128(bytes, 32)?)?,
        capture_generation: GraphGeneration::new(read_u64(bytes, 48)?),
        target_generation: GraphGeneration::new(read_u64(bytes, 56)?),
        sequence: read_u64(bytes, 64)?,
        serial_fence: read_u64(bytes, 72)?,
    })
}

fn validate_spill_binding(bytes: &[u8], binding: super::SpillBinding) -> Result<(), TreeError> {
    if read_u128(bytes, 16)? != binding.store.get()
        || read_u128(bytes, 32)? != binding.session.get()
        || read_u64(bytes, 48)? != binding.capture_generation.get()
        || read_u64(bytes, 56)? != binding.target_generation.get()
        || read_u64(bytes, 64)? != binding.sequence
        || read_u64(bytes, 72)? != binding.serial_fence
    {
        return Err(TreeError::Invalid("protected stream binding"));
    }
    Ok(())
}

/// The total encoded length of one pending intent: the fixed header, the
/// genuine object descriptors, then the tagged partial-target partition.
pub(super) fn pending_intent_bytes(candidates: usize, partials: usize) -> Result<usize, TreeError> {
    PENDING_INTENT_HEADER_BYTES
        .checked_add(
            candidates
                .checked_mul(DESCRIPTOR_BYTES)
                .ok_or(TreeError::Memory)?,
        )
        .and_then(|length| length.checked_add(partials.checked_mul(PARTIAL_TARGET_BYTES)?))
        .ok_or(TreeError::Memory)
}

pub(super) fn reclaim_completion_bytes(
    targets: usize,
    partials: usize,
) -> Result<usize, TreeError> {
    COMPLETION_HEADER_BYTES
        .checked_add(
            targets
                .checked_mul(DESCRIPTOR_BYTES)
                .ok_or(TreeError::Memory)?,
        )
        .and_then(|length| length.checked_add(partials.checked_mul(PARTIAL_TARGET_BYTES)?))
        .ok_or(TreeError::Memory)
}

/// Every rule the tagged partial partition must satisfy in both directions.
///
/// A partial target is not an object: it has no whole-file checksum and it can
/// never be validated against a descriptor. Its authority to be unlinked comes
/// entirely from these facts, so both the encoder and every decoder check them.
fn validate_partial_domain(
    binding: super::SpillBinding,
    candidates: &[ArtifactDescriptor],
    partials: &[super::PartialTarget],
) -> Result<(), TreeError> {
    if candidates
        .len()
        .checked_add(partials.len())
        .ok_or(TreeError::Memory)?
        > super::MAX_CANDIDATES
        || partials
            .windows(2)
            .any(|pair| matches!(pair, [left, right] if left.artifact >= right.artifact))
        || partials.iter().any(|partial| {
            partial.store != binding.store
                || partial.generation > binding.capture_generation
                || partial.serial == 0
                || partial.serial > binding.serial_fence
                || partial.family != FormatFamily::NativeGraphObject.id()
                || partial.version != 1
                || partial.observed < artifact::HEADER_BYTES as u64
                || partial.declared <= partial.observed
                || partial.declared > artifact::MAX_ARTIFACT_BYTES as u64
        })
        || partials.iter().any(|partial| {
            candidates
                .iter()
                .any(|candidate| candidate.artifact == partial.artifact)
        })
    {
        return Err(TreeError::Invalid("reclaim partial target domain"));
    }
    Ok(())
}

fn put_partial(
    output: &mut [u8],
    offset: usize,
    partial: super::PartialTarget,
) -> Result<(), TreeError> {
    let target = output
        .get_mut(
            offset
                ..offset
                    .checked_add(PARTIAL_TARGET_BYTES)
                    .ok_or(TreeError::Memory)?,
        )
        .ok_or(TreeError::Memory)?;
    put(target, 0, &partial.store.get().to_le_bytes())?;
    put(target, 16, &partial.artifact.get().to_le_bytes())?;
    put(target, 32, &partial.generation.get().to_le_bytes())?;
    put(target, 40, &partial.serial.to_le_bytes())?;
    put(target, 48, &partial.observed.to_le_bytes())?;
    put(target, 56, &partial.declared.to_le_bytes())?;
    put(target, 64, &partial.digest.to_le_bytes())?;
    put(target, 72, &partial.family.to_le_bytes())?;
    put(target, 74, &partial.version.to_le_bytes())?;
    put(target, 76, &[0; 4])
}

fn partial(bytes: &[u8], offset: usize) -> Result<super::PartialTarget, TreeError> {
    let source = bytes
        .get(
            offset
                ..offset
                    .checked_add(PARTIAL_TARGET_BYTES)
                    .ok_or(TreeError::Memory)?,
        )
        .ok_or(TreeError::Memory)?;
    if source.get(76..80) != Some([0_u8; 4].as_slice()) {
        return Err(TreeError::Invalid("reclaim partial target reserved bytes"));
    }
    Ok(super::PartialTarget {
        store: StoreInstanceId::new(read_u128(source, 0)?)
            .map_err(|_| TreeError::Invalid("reclaim partial target store"))?,
        artifact: ArtifactId::new(read_u128(source, 16)?)?,
        generation: GraphGeneration::new(read_u64(source, 32)?),
        serial: read_u64(source, 40)?,
        observed: read_u64(source, 48)?,
        declared: read_u64(source, 56)?,
        digest: read_u64(source, 64)?,
        family: read_u16(source, 72)?,
        version: read_u16(source, 74)?,
    })
}

/// Encode the bounded role-5 pending target manifest. The protected and mark
/// roots are completed durable stream manifests; target rows remain whole
/// object descriptors and never become live edges.
///
/// `partials` is the ZE-165 tagged partition: interrupted creations that keep
/// an intact header. They follow the descriptors and are never mixed into
/// them, because they are not objects and can never be validated as one.
pub(super) fn encode_pending_intent(
    binding: super::SpillBinding,
    protected: super::DurableProtectedStream,
    mark: super::DurableRun,
    candidates: &[ArtifactDescriptor],
    partials: &[super::PartialTarget],
    output: &mut [u8],
) -> Result<(usize, u64), TreeError> {
    if protected.binding != binding
        || mark.binding != binding
        || candidates.len() > super::MAX_CANDIDATES
        || candidates
            .windows(2)
            .any(|pair| matches!(pair, [left, right] if left.artifact >= right.artifact))
        || candidates.iter().any(|candidate| {
            candidate.store != binding.store
                || candidate.serial == 0
                || candidate.serial > binding.serial_fence
                || candidate.family != FormatFamily::NativeGraphObject.id()
                || candidate.version != 1
        })
    {
        return Err(TreeError::Invalid("pending reclaim intent domain"));
    }
    validate_partial_domain(binding, candidates, partials)?;
    let length = pending_intent_bytes(candidates.len(), partials.len())?;
    let target = output.get_mut(..length).ok_or(TreeError::Memory)?;
    target.fill(0);
    put(target, 0, b"ZGCP")?;
    put(target, 4, &(ProofRole::ReclaimState as u16).to_le_bytes())?;
    put(target, 6, &1_u16.to_le_bytes())?;
    put(target, 8, &2_u16.to_le_bytes())?;
    put(
        target,
        12,
        &u32::try_from(length)
            .map_err(|_| TreeError::Memory)?
            .to_le_bytes(),
    )?;
    encode_spill_binding(target, binding)?;
    put(
        target,
        80,
        &u32::try_from(candidates.len())
            .map_err(|_| TreeError::Memory)?
            .to_le_bytes(),
    )?;
    put(
        target,
        84,
        &u32::try_from(partials.len())
            .map_err(|_| TreeError::Memory)?
            .to_le_bytes(),
    )?;
    put_required(target, 88, protected.head)?;
    put(target, 184, &protected.count.to_le_bytes())?;
    put(target, 192, &protected.digest.to_le_bytes())?;
    put(target, 200, &protected.pages.to_le_bytes())?;
    put_required(target, 208, mark.root)?;
    put(target, 304, &mark.count.to_le_bytes())?;
    put(target, 312, &mark.digest.to_le_bytes())?;
    put(target, 320, &mark.first.get().to_le_bytes())?;
    put(target, 336, &mark.last.get().to_le_bytes())?;
    put(target, 360, &mark.height.to_le_bytes())?;
    for (index, candidate) in candidates.iter().copied().enumerate() {
        put_descriptor(
            target,
            PENDING_INTENT_HEADER_BYTES + index * DESCRIPTOR_BYTES,
            candidate,
        )?;
    }
    let partition = pending_intent_bytes(candidates.len(), 0)?;
    for (index, value) in partials.iter().copied().enumerate() {
        put_partial(
            target,
            partition
                .checked_add(
                    index
                        .checked_mul(PARTIAL_TARGET_BYTES)
                        .ok_or(TreeError::Memory)?,
                )
                .ok_or(TreeError::Memory)?,
            value,
        )?;
    }
    let mut hasher = Xxh3::new();
    hasher.update(target.get(..352).ok_or(TreeError::Memory)?);
    hasher.update(&[0; 8]);
    hasher.update(target.get(360..).ok_or(TreeError::Memory)?);
    let digest = hasher.digest();
    put(target, 352, &digest.to_le_bytes())?;
    Ok((length, digest))
}

pub(super) fn validate_pending_intent(
    bytes: &[u8],
    binding: super::SpillBinding,
    protected: super::DurableProtectedStream,
    mark: super::DurableRun,
    candidates: &[ArtifactDescriptor],
    partials: &[super::PartialTarget],
    expected_digest: u64,
) -> Result<(), TreeError> {
    let expected_length = pending_intent_bytes(candidates.len(), partials.len())?;
    if bytes.len() != expected_length
        || bytes.get(..4) != Some(b"ZGCP".as_slice())
        || read_u16(bytes, 4)? != ProofRole::ReclaimState as u16
        || read_u16(bytes, 6)? != 1
        || read_u16(bytes, 8)? != 2
        || read_u16(bytes, 10)? != 0
        || usize::try_from(read_u32(bytes, 12)?).map_err(|_| TreeError::Memory)? != expected_length
        || bytes.get(362..368) != Some([0_u8; 6].as_slice())
        || usize::try_from(read_u32(bytes, 80)?).map_err(|_| TreeError::Memory)? != candidates.len()
        || usize::try_from(read_u32(bytes, 84)?).map_err(|_| TreeError::Memory)? != partials.len()
    {
        return Err(TreeError::Invalid("pending reclaim intent header"));
    }
    validate_spill_binding(bytes, binding)?;
    validate_partial_domain(binding, candidates, partials)?;
    if required(bytes, 88)? != protected.head
        || read_u64(bytes, 184)? != protected.count
        || read_u64(bytes, 192)? != protected.digest
        || read_u64(bytes, 200)? != protected.pages
        || required(bytes, 208)? != mark.root
        || read_u64(bytes, 304)? != mark.count
        || read_u64(bytes, 312)? != mark.digest
        || read_u128(bytes, 320)? != mark.first.get()
        || read_u128(bytes, 336)? != mark.last.get()
    {
        return Err(TreeError::Invalid("pending reclaim intent proof roots"));
    }
    for (index, expected) in candidates.iter().copied().enumerate() {
        let offset = PENDING_INTENT_HEADER_BYTES
            .checked_add(
                index
                    .checked_mul(DESCRIPTOR_BYTES)
                    .ok_or(TreeError::Memory)?,
            )
            .ok_or(TreeError::Memory)?;
        if descriptor(bytes, offset)? != expected {
            return Err(TreeError::Invalid("pending reclaim intent candidate"));
        }
    }
    for (index, expected) in partials.iter().copied().enumerate() {
        if pending_intent_partial_at(bytes, index)? != expected {
            return Err(TreeError::Invalid("pending reclaim intent partial target"));
        }
    }
    let mut hasher = Xxh3::new();
    hasher.update(bytes.get(..352).ok_or(TreeError::Memory)?);
    hasher.update(&[0; 8]);
    hasher.update(bytes.get(360..).ok_or(TreeError::Memory)?);
    let digest = hasher.digest();
    if read_u64(bytes, 352)? != digest || expected_digest != digest {
        return Err(TreeError::Invalid("pending reclaim intent digest"));
    }
    Ok(())
}

pub(super) fn decode_pending_intent_manifest(
    bytes: &[u8],
) -> Result<super::PendingIntentManifest, TreeError> {
    if bytes.len() < PENDING_INTENT_HEADER_BYTES
        || bytes.get(..4) != Some(b"ZGCP".as_slice())
        || read_u16(bytes, 4)? != ProofRole::ReclaimState as u16
        || read_u16(bytes, 6)? != 1
        || read_u16(bytes, 8)? != 2
        || read_u16(bytes, 10)? != 0
        || usize::try_from(read_u32(bytes, 12)?).map_err(|_| TreeError::Memory)? != bytes.len()
        || bytes.get(362..368) != Some([0_u8; 6].as_slice())
    {
        return Err(TreeError::Invalid("pending reclaim intent header"));
    }
    let binding = super::SpillBinding {
        store: StoreInstanceId::new(read_u128(bytes, 16)?)
            .map_err(|_| TreeError::Invalid("pending intent store"))?,
        session: ArtifactId::new(read_u128(bytes, 32)?)?,
        capture_generation: GraphGeneration::new(read_u64(bytes, 48)?),
        target_generation: GraphGeneration::new(read_u64(bytes, 56)?),
        sequence: read_u64(bytes, 64)?,
        serial_fence: read_u64(bytes, 72)?,
    };
    let candidate_count = usize::try_from(read_u32(bytes, 80)?).map_err(|_| TreeError::Memory)?;
    let partial_count = usize::try_from(read_u32(bytes, 84)?).map_err(|_| TreeError::Memory)?;
    // An intent must name at least one target, but either partition alone is a
    // complete reason to have written it: a crash that interrupts one object
    // creation leaves exactly one partial file and nothing else to reclaim.
    if candidate_count
        .checked_add(partial_count)
        .is_none_or(|total| total == 0 || total > super::MAX_CANDIDATES)
        || bytes.len() != pending_intent_bytes(candidate_count, partial_count)?
    {
        return Err(TreeError::Invalid("pending reclaim intent candidate count"));
    }
    let protected = super::DurableProtectedStream {
        head: required(bytes, 88)?,
        count: read_u64(bytes, 184)?,
        digest: read_u64(bytes, 192)?,
        pages: read_u64(bytes, 200)?,
        binding,
    };
    let mark = super::DurableRun {
        root: required(bytes, 208)?,
        count: read_u64(bytes, 304)?,
        digest: read_u64(bytes, 312)?,
        first: ArtifactId::new(read_u128(bytes, 320)?)?,
        last: ArtifactId::new(read_u128(bytes, 336)?)?,
        binding,
        height: read_u16(bytes, 360)?,
    };
    if protected.count == 0 || protected.pages == 0 || mark.count == 0 || mark.first > mark.last {
        return Err(TreeError::Invalid("pending reclaim intent proof summary"));
    }
    let mut previous = None;
    for index in 0..candidate_count {
        let candidate = pending_intent_candidate_at(bytes, index)?;
        if candidate.store != binding.store
            || candidate.serial == 0
            || candidate.serial > binding.serial_fence
            || candidate.family != FormatFamily::NativeGraphObject.id()
            || candidate.version != 1
            || previous.is_some_and(|artifact| artifact >= candidate.artifact)
        {
            return Err(TreeError::Invalid(
                "pending reclaim intent candidate domain",
            ));
        }
        previous = Some(candidate.artifact);
    }
    let mut previous_partial = None;
    for index in 0..partial_count {
        let target = pending_intent_partial_at(bytes, index)?;
        if target.store != binding.store
            || target.generation > binding.capture_generation
            || target.serial == 0
            || target.serial > binding.serial_fence
            || target.family != FormatFamily::NativeGraphObject.id()
            || target.version != 1
            || target.observed < artifact::HEADER_BYTES as u64
            || target.declared <= target.observed
            || target.declared > artifact::MAX_ARTIFACT_BYTES as u64
            || previous_partial.is_some_and(|artifact| artifact >= target.artifact)
        {
            return Err(TreeError::Invalid("reclaim partial target domain"));
        }
        for candidate in 0..candidate_count {
            if pending_intent_candidate_at(bytes, candidate)?.artifact == target.artifact {
                return Err(TreeError::Invalid("reclaim partial target domain"));
            }
        }
        previous_partial = Some(target.artifact);
    }
    let mut hasher = Xxh3::new();
    hasher.update(bytes.get(..352).ok_or(TreeError::Memory)?);
    hasher.update(&[0; 8]);
    hasher.update(bytes.get(360..).ok_or(TreeError::Memory)?);
    let digest = hasher.digest();
    if read_u64(bytes, 352)? != digest {
        return Err(TreeError::Invalid("pending reclaim intent digest"));
    }
    Ok(super::PendingIntentManifest {
        binding,
        protected,
        mark,
        candidate_count,
        partial_count,
        digest,
    })
}

pub(super) fn pending_intent_candidate_at(
    bytes: &[u8],
    index: usize,
) -> Result<ArtifactDescriptor, TreeError> {
    let count = usize::try_from(read_u32(bytes, 80)?).map_err(|_| TreeError::Memory)?;
    if index >= count {
        return Err(TreeError::Invalid("pending reclaim candidate index"));
    }
    let offset = PENDING_INTENT_HEADER_BYTES
        .checked_add(
            index
                .checked_mul(DESCRIPTOR_BYTES)
                .ok_or(TreeError::Memory)?,
        )
        .ok_or(TreeError::Memory)?;
    descriptor(bytes, offset)
}

pub(super) fn pending_intent_partial_at(
    bytes: &[u8],
    index: usize,
) -> Result<super::PartialTarget, TreeError> {
    let candidates = usize::try_from(read_u32(bytes, 80)?).map_err(|_| TreeError::Memory)?;
    let count = usize::try_from(read_u32(bytes, 84)?).map_err(|_| TreeError::Memory)?;
    if index >= count {
        return Err(TreeError::Invalid("pending reclaim partial index"));
    }
    partial(bytes, pending_intent_bytes(candidates, index)?)
}

pub(super) fn completed_intent_partial_at(
    bytes: &[u8],
    index: usize,
) -> Result<super::PartialTarget, TreeError> {
    let completed = usize::try_from(read_u32(bytes, 80)?).map_err(|_| TreeError::Memory)?;
    let remaining = usize::try_from(read_u32(bytes, 84)?).map_err(|_| TreeError::Memory)?;
    let count = usize::try_from(read_u32(bytes, 192)?).map_err(|_| TreeError::Memory)?;
    if index >= count {
        return Err(TreeError::Invalid("reclaim completion partial index"));
    }
    let targets = completed.checked_add(remaining).ok_or(TreeError::Memory)?;
    partial(bytes, reclaim_completion_bytes(targets, index)?)
}

pub(super) fn encode_reclaim_completion(
    binding: super::SpillBinding,
    intent: RequiredRef,
    completed: &[ArtifactDescriptor],
    remaining: &[ArtifactDescriptor],
    partials: &[super::PartialTarget],
    output: &mut [u8],
) -> Result<(usize, u64), TreeError> {
    let total = completed
        .len()
        .checked_add(remaining.len())
        .ok_or(TreeError::Memory)?;
    if total
        .checked_add(partials.len())
        .is_none_or(|rows| rows == 0 || rows > super::MAX_CANDIDATES)
        || intent.object.store != binding.store
        || intent.object.generation != binding.target_generation
        || completed
            .windows(2)
            .chain(remaining.windows(2))
            .any(|pair| matches!(pair, [left, right] if left.artifact >= right.artifact))
        || completed.iter().any(|left| {
            remaining
                .iter()
                .any(|right| left.artifact == right.artifact)
        })
    {
        return Err(TreeError::Invalid("reclaim completion domain"));
    }
    validate_partial_domain(binding, completed, partials)?;
    validate_partial_domain(binding, remaining, partials)?;
    let length = reclaim_completion_bytes(total, partials.len())?;
    let target = output.get_mut(..length).ok_or(TreeError::Memory)?;
    target.fill(0);
    put(target, 0, b"ZGCP")?;
    put(target, 4, &(ProofRole::ReclaimState as u16).to_le_bytes())?;
    put(target, 6, &1_u16.to_le_bytes())?;
    put(target, 8, &3_u16.to_le_bytes())?;
    put(
        target,
        12,
        &u32::try_from(length)
            .map_err(|_| TreeError::Memory)?
            .to_le_bytes(),
    )?;
    encode_spill_binding(target, binding)?;
    put(
        target,
        80,
        &u32::try_from(completed.len())
            .map_err(|_| TreeError::Memory)?
            .to_le_bytes(),
    )?;
    put(
        target,
        84,
        &u32::try_from(remaining.len())
            .map_err(|_| TreeError::Memory)?
            .to_le_bytes(),
    )?;
    put_required(target, 88, intent)?;
    put(
        target,
        192,
        &u32::try_from(partials.len())
            .map_err(|_| TreeError::Memory)?
            .to_le_bytes(),
    )?;
    let mut offset = COMPLETION_HEADER_BYTES;
    for descriptor in completed.iter().chain(remaining).copied() {
        put_descriptor(target, offset, descriptor)?;
        offset += DESCRIPTOR_BYTES;
    }
    for value in partials.iter().copied() {
        put_partial(target, offset, value)?;
        offset += PARTIAL_TARGET_BYTES;
    }
    let mut hasher = Xxh3::new();
    hasher.update(target.get(..184).ok_or(TreeError::Memory)?);
    hasher.update(&[0; 8]);
    hasher.update(target.get(192..).ok_or(TreeError::Memory)?);
    let digest = hasher.digest();
    put(target, 184, &digest.to_le_bytes())?;
    Ok((length, digest))
}

pub(super) fn decode_completed_intent_manifest(
    bytes: &[u8],
) -> Result<super::CompletedIntentManifest, TreeError> {
    if bytes.len() < COMPLETION_HEADER_BYTES
        || bytes.get(..4) != Some(b"ZGCP".as_slice())
        || read_u16(bytes, 4)? != ProofRole::ReclaimState as u16
        || read_u16(bytes, 6)? != 1
        || read_u16(bytes, 8)? != 3
        || read_u16(bytes, 10)? != 0
        || usize::try_from(read_u32(bytes, 12)?).map_err(|_| TreeError::Memory)? != bytes.len()
        || bytes.get(196..200) != Some([0_u8; 4].as_slice())
    {
        return Err(TreeError::Invalid("reclaim completion header"));
    }
    let binding = decode_spill_binding(bytes)?;
    let completed_count = usize::try_from(read_u32(bytes, 80)?).map_err(|_| TreeError::Memory)?;
    let remaining_count = usize::try_from(read_u32(bytes, 84)?).map_err(|_| TreeError::Memory)?;
    let partial_count = usize::try_from(read_u32(bytes, 192)?).map_err(|_| TreeError::Memory)?;
    let total = completed_count
        .checked_add(remaining_count)
        .ok_or(TreeError::Memory)?;
    if total
        .checked_add(partial_count)
        .is_none_or(|rows| rows == 0 || rows > super::MAX_CANDIDATES)
        || bytes.len() != reclaim_completion_bytes(total, partial_count)?
    {
        return Err(TreeError::Invalid("reclaim completion geometry"));
    }
    let intent = required(bytes, 88)?;
    if intent.object.store != binding.store || intent.object.generation != binding.target_generation
    {
        return Err(TreeError::Invalid("reclaim completion intent association"));
    }
    let mut completed_previous = None;
    let mut remaining_previous = None;
    for index in 0..total {
        let candidate = completed_intent_candidate_at(bytes, index)?;
        if candidate.store != binding.store
            || candidate.serial == 0
            || candidate.serial > binding.serial_fence
            || candidate.family != FormatFamily::NativeGraphObject.id()
            || candidate.version != 1
        {
            return Err(TreeError::Invalid("reclaim completion candidate domain"));
        }
        let previous = if index < completed_count {
            &mut completed_previous
        } else {
            &mut remaining_previous
        };
        if previous.is_some_and(|artifact| artifact >= candidate.artifact) {
            return Err(TreeError::Invalid("reclaim completion candidate ordering"));
        }
        *previous = Some(candidate.artifact);
    }
    for completed in 0..completed_count {
        let left = completed_intent_candidate_at(bytes, completed)?.artifact;
        for remaining in completed_count..total {
            if left == completed_intent_candidate_at(bytes, remaining)?.artifact {
                return Err(TreeError::Invalid("reclaim completion partition overlap"));
            }
        }
    }
    let mut previous_partial = None;
    for index in 0..partial_count {
        let target = completed_intent_partial_at(bytes, index)?;
        if target.store != binding.store
            || target.generation > binding.capture_generation
            || target.serial == 0
            || target.serial > binding.serial_fence
            || target.family != FormatFamily::NativeGraphObject.id()
            || target.version != 1
            || target.observed < artifact::HEADER_BYTES as u64
            || target.declared <= target.observed
            || target.declared > artifact::MAX_ARTIFACT_BYTES as u64
            || previous_partial.is_some_and(|artifact| artifact >= target.artifact)
        {
            return Err(TreeError::Invalid("reclaim partial target domain"));
        }
        for row in 0..total {
            if completed_intent_candidate_at(bytes, row)?.artifact == target.artifact {
                return Err(TreeError::Invalid("reclaim completion partition overlap"));
            }
        }
        previous_partial = Some(target.artifact);
    }
    let mut hasher = Xxh3::new();
    hasher.update(bytes.get(..184).ok_or(TreeError::Memory)?);
    hasher.update(&[0; 8]);
    hasher.update(bytes.get(192..).ok_or(TreeError::Memory)?);
    let digest = hasher.digest();
    if read_u64(bytes, 184)? != digest {
        return Err(TreeError::Invalid("reclaim completion digest"));
    }
    Ok(super::CompletedIntentManifest {
        binding,
        intent,
        completed_count,
        remaining_count,
        partial_count,
        digest,
    })
}

pub(super) fn completed_intent_candidate_at(
    bytes: &[u8],
    index: usize,
) -> Result<ArtifactDescriptor, TreeError> {
    let completed = usize::try_from(read_u32(bytes, 80)?).map_err(|_| TreeError::Memory)?;
    let remaining = usize::try_from(read_u32(bytes, 84)?).map_err(|_| TreeError::Memory)?;
    let total = completed.checked_add(remaining).ok_or(TreeError::Memory)?;
    if index >= total {
        return Err(TreeError::Invalid("reclaim completion candidate index"));
    }
    let offset = COMPLETION_HEADER_BYTES
        .checked_add(
            index
                .checked_mul(DESCRIPTOR_BYTES)
                .ok_or(TreeError::Memory)?,
        )
        .ok_or(TreeError::Memory)?;
    descriptor(bytes, offset)
}

fn put_required(output: &mut [u8], offset: usize, required: RequiredRef) -> Result<(), TreeError> {
    put_descriptor(output, offset, required.object)?;
    let mut encoded = [0_u8; 32];
    artifact::encode_reference(required.block, &mut encoded)?;
    put(output, offset + DESCRIPTOR_BYTES, &encoded)
}

fn required(bytes: &[u8], offset: usize) -> Result<RequiredRef, TreeError> {
    let object = descriptor(bytes, offset)?;
    let block = artifact::decode_reference(
        bytes
            .get(offset + DESCRIPTOR_BYTES..offset + REQUIRED_BYTES)
            .ok_or(TreeError::Invalid("proof required reference extent"))?,
    )?;
    if block.artifact != object.artifact || block.kind != BlockKind::CommitParticipant {
        return Err(TreeError::Invalid("proof required reference role"));
    }
    Ok(RequiredRef { object, block })
}

fn protected_required(bytes: &[u8], offset: usize) -> Result<RequiredRef, TreeError> {
    let object = descriptor(bytes, offset)?;
    let block = artifact::decode_reference(
        bytes
            .get(offset + DESCRIPTOR_BYTES..offset + REQUIRED_BYTES)
            .ok_or(TreeError::Invalid("protected required reference extent"))?,
    )?;
    if block.artifact != object.artifact || block.version != 1 {
        return Err(TreeError::Invalid("protected required reference identity"));
    }
    Ok(RequiredRef { object, block })
}

fn put_descriptor(
    output: &mut [u8],
    offset: usize,
    descriptor: ArtifactDescriptor,
) -> Result<(), TreeError> {
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

fn descriptor(bytes: &[u8], offset: usize) -> Result<ArtifactDescriptor, TreeError> {
    Ok(ArtifactDescriptor {
        store: StoreInstanceId::new(read_u128(bytes, offset)?)
            .map_err(|_| TreeError::Invalid("zero proof descriptor store"))?,
        artifact: ArtifactId::new(read_u128(bytes, offset + 16)?)?,
        generation: GraphGeneration::new(read_u64(bytes, offset + 32)?),
        serial: read_u64(bytes, offset + 40)?,
        bytes: read_u32(bytes, offset + 48)?,
        family: read_u16(bytes, offset + 52)?,
        version: read_u16(bytes, offset + 54)?,
        checksum: read_u64(bytes, offset + 56)?,
    })
}

fn put(output: &mut [u8], offset: usize, input: &[u8]) -> Result<(), TreeError> {
    output
        .get_mut(offset..offset.checked_add(input.len()).ok_or(TreeError::Memory)?)
        .ok_or(TreeError::Memory)?
        .copy_from_slice(input);
    Ok(())
}

fn byte(bytes: &[u8], offset: usize) -> Result<u8, TreeError> {
    bytes
        .get(offset)
        .copied()
        .ok_or(TreeError::Invalid("proof field extent"))
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
        .ok_or(TreeError::Invalid("proof field extent"))
}
