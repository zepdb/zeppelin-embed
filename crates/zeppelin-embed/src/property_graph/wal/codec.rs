use super::*;
use xxhash_rust::xxh3::Xxh3;
pub(super) const CHUNK: usize = 64 * 1024;
pub(super) fn hash(bytes: &[u8], r: &mut WalResources<'_>) -> Result<u64, WalError> {
    let mut hash = Xxh3::new();
    for chunk in bytes.chunks(CHUNK) {
        r.charge(chunk.len() as u64)?;
        hash.update(chunk);
    }
    Ok(hash.digest())
}
pub(super) struct Writer<'a> {
    pub bytes: Option<&'a mut [u8]>,
    pub pos: usize,
}
impl Writer<'_> {
    pub fn put(&mut self, value: &[u8], r: &mut WalResources<'_>) -> Result<(), WalError> {
        let end = self
            .pos
            .checked_add(value.len())
            .filter(|v| *v <= MAX_ENVELOPE_BYTES)
            .ok_or(WalError::Capacity)?;
        for chunk in value.chunks(CHUNK) {
            r.charge(chunk.len() as u64)?;
            if let Some(bytes) = &mut self.bytes {
                bytes
                    .get_mut(self.pos..self.pos + chunk.len())
                    .ok_or(WalError::Capacity)?
                    .copy_from_slice(chunk);
            }
            self.pos += chunk.len();
        }
        self.pos = end;
        Ok(())
    }
    pub fn u8(&mut self, v: u8, r: &mut WalResources<'_>) -> Result<(), WalError> {
        self.put(&[v], r)
    }
    pub fn u16(&mut self, v: u16, r: &mut WalResources<'_>) -> Result<(), WalError> {
        self.put(&v.to_le_bytes(), r)
    }
    pub fn u32(&mut self, v: u32, r: &mut WalResources<'_>) -> Result<(), WalError> {
        self.put(&v.to_le_bytes(), r)
    }
    pub fn u64(&mut self, v: u64, r: &mut WalResources<'_>) -> Result<(), WalError> {
        self.put(&v.to_le_bytes(), r)
    }
    pub fn u128(&mut self, v: u128, r: &mut WalResources<'_>) -> Result<(), WalError> {
        self.put(&v.to_le_bytes(), r)
    }
}
pub(super) struct Reader<'a> {
    pub bytes: &'a [u8],
    pub pos: usize,
}
impl<'a> Reader<'a> {
    pub fn take(&mut self, n: usize, r: &mut WalResources<'_>) -> Result<&'a [u8], WalError> {
        r.charge(n as u64)?;
        let end = self.pos.checked_add(n).ok_or(WalError::Malformed)?;
        let part = self.bytes.get(self.pos..end).ok_or(WalError::Malformed)?;
        self.pos = end;
        Ok(part)
    }
    pub fn array<const N: usize>(&mut self, r: &mut WalResources<'_>) -> Result<[u8; N], WalError> {
        self.take(N, r)?.try_into().map_err(|_| WalError::Malformed)
    }
    pub fn u8(&mut self, r: &mut WalResources<'_>) -> Result<u8, WalError> {
        Ok(u8::from_le_bytes(self.array(r)?))
    }
    pub fn u16(&mut self, r: &mut WalResources<'_>) -> Result<u16, WalError> {
        Ok(u16::from_le_bytes(self.array(r)?))
    }
    pub fn u32(&mut self, r: &mut WalResources<'_>) -> Result<u32, WalError> {
        Ok(u32::from_le_bytes(self.array(r)?))
    }
    pub fn u64(&mut self, r: &mut WalResources<'_>) -> Result<u64, WalError> {
        Ok(u64::from_le_bytes(self.array(r)?))
    }
    pub fn u128(&mut self, r: &mut WalResources<'_>) -> Result<u128, WalError> {
        Ok(u128::from_le_bytes(self.array(r)?))
    }
    pub fn zero(&mut self, n: usize, r: &mut WalResources<'_>) -> Result<(), WalError> {
        if self.take(n, r)?.iter().any(|b| *b != 0) {
            Err(WalError::Malformed)
        } else {
            Ok(())
        }
    }
    pub fn end(self) -> Result<(), WalError> {
        if self.pos == self.bytes.len() {
            Ok(())
        } else {
            Err(WalError::Malformed)
        }
    }
}
pub(super) fn descriptor(v: ArtifactDescriptor) -> Result<(), WalError> {
    if v.family != 17 || v.version != 1 {
        return Err(WalError::Unsupported);
    }
    if v.bytes < 104
        || v.bytes as usize > super::super::storage::artifact::MAX_ARTIFACT_BYTES
        || v.serial == 0
    {
        return Err(WalError::Malformed);
    }
    Ok(())
}
pub(super) fn put_descriptor(
    v: ArtifactDescriptor,
    w: &mut Writer<'_>,
    r: &mut WalResources<'_>,
) -> Result<(), WalError> {
    descriptor(v)?;
    w.u128(v.store.get(), r)?;
    w.u128(v.artifact.get(), r)?;
    w.u64(v.generation.get(), r)?;
    w.u64(v.serial, r)?;
    w.u32(v.bytes, r)?;
    w.u16(v.family, r)?;
    w.u16(v.version, r)?;
    w.u64(v.checksum, r)
}
pub(super) fn get_descriptor(
    rd: &mut Reader<'_>,
    r: &mut WalResources<'_>,
) -> Result<ArtifactDescriptor, WalError> {
    let v = ArtifactDescriptor {
        store: StoreInstanceId::new(rd.u128(r)?).map_err(|_| WalError::Malformed)?,
        artifact: artifact_id(rd.u128(r)?)?,
        generation: GraphGeneration::new(rd.u64(r)?),
        serial: rd.u64(r)?,
        bytes: rd.u32(r)?,
        family: rd.u16(r)?,
        version: rd.u16(r)?,
        checksum: rd.u64(r)?,
    };
    descriptor(v)?;
    Ok(v)
}
fn artifact_id(value: u128) -> Result<ArtifactId, WalError> {
    if value == 0 {
        return Err(WalError::Malformed);
    }
    // The checked nonzero branch cannot allocate ArtifactId's owned error.
    ArtifactId::new(value).map_err(|_| WalError::Malformed)
}
fn block_kind(value: u16) -> Result<super::super::storage::artifact::BlockKind, WalError> {
    use super::super::storage::artifact::BlockKind::*;
    match value {
        1 => Ok(TreePage),
        2 => Ok(NodeRecord),
        3 => Ok(RelRecord),
        4 => Ok(CanonicalImage),
        5 => Ok(StoredText),
        6 => Ok(StoredVector),
        7 => Ok(OverflowKey),
        8 => Ok(ExtentList),
        9 => Ok(CheckpointPayload),
        10 => Ok(CommitParticipant),
        _ => Err(WalError::Unsupported),
    }
}
fn reference(v: RequiredRef) -> Result<(), WalError> {
    if v.object.artifact != v.block.artifact
        || v.block.offset < 96
        || v.block.length < 24
        || v.block.version != 1
        || v.block
            .offset
            .checked_add(u64::from(v.block.length))
            .is_none_or(|end| end > u64::from(v.object.bytes))
    {
        return Err(WalError::Malformed);
    }
    Ok(())
}
pub(super) fn put_ref(
    v: RequiredRef,
    w: &mut Writer<'_>,
    r: &mut WalResources<'_>,
) -> Result<(), WalError> {
    reference(v)?;
    put_descriptor(v.object, w, r)?;
    w.u128(v.block.artifact.get(), r)?;
    w.u64(v.block.offset, r)?;
    w.u32(v.block.length, r)?;
    w.u16(v.block.kind as u16, r)?;
    w.u16(v.block.version, r)
}
pub(super) fn get_ref(
    rd: &mut Reader<'_>,
    r: &mut WalResources<'_>,
) -> Result<RequiredRef, WalError> {
    let object = get_descriptor(rd, r)?;
    let block = PhysicalRef {
        artifact: artifact_id(rd.u128(r)?)?,
        offset: rd.u64(r)?,
        length: rd.u32(r)?,
        kind: block_kind(rd.u16(r)?)?,
        version: rd.u16(r)?,
    };
    let v = RequiredRef { object, block };
    reference(v)?;
    Ok(v)
}
pub(super) fn put_optional(
    v: Option<RequiredRef>,
    w: &mut Writer<'_>,
    r: &mut WalResources<'_>,
) -> Result<(), WalError> {
    w.u8(u8::from(v.is_some()), r)?;
    w.put(&[0; 7], r)?;
    if let Some(v) = v {
        put_ref(v, w, r)?;
    }
    Ok(())
}
pub(super) fn get_optional(
    rd: &mut Reader<'_>,
    r: &mut WalResources<'_>,
) -> Result<Option<RequiredRef>, WalError> {
    let tag = rd.u8(r)?;
    rd.zero(7, r)?;
    match tag {
        0 => Ok(None),
        1 => get_ref(rd, r).map(Some),
        _ => Err(WalError::Malformed),
    }
}
impl ReferenceList<'_> {
    /// Number of retained descriptors, rejecting a partial encoded row.
    pub fn len(self) -> Result<usize, WalError> {
        match self {
            Self::Values(v) => Ok(v.len()),
            Self::Encoded(v) if v.len().is_multiple_of(96) => Ok(v.len() / 96),
            _ => Err(WalError::Malformed),
        }
    }
    /// Whether the descriptor list is empty.
    pub fn is_empty(self) -> Result<bool, WalError> {
        Ok(self.len()? == 0)
    }
    /// Decodes one required descriptor without allocation.
    pub fn get(self, index: usize, r: &mut WalResources<'_>) -> Result<RequiredRef, WalError> {
        r.charge(1)?;
        match self {
            Self::Values(v) => v.get(index).copied().ok_or(WalError::Malformed),
            Self::Encoded(v) => {
                let start = index.checked_mul(96).ok_or(WalError::Malformed)?;
                get_ref(
                    &mut Reader {
                        bytes: v
                            .get(start..start.checked_add(96).ok_or(WalError::Malformed)?)
                            .ok_or(WalError::Malformed)?,
                        pos: 0,
                    },
                    r,
                )
            }
        }
    }
}
impl DescriptorList<'_> {
    /// Number of retained descriptors, rejecting a partial encoded row.
    pub fn len(self) -> Result<usize, WalError> {
        match self {
            Self::Values(v) => Ok(v.len()),
            Self::Encoded(v) if v.len().is_multiple_of(64) => Ok(v.len() / 64),
            _ => Err(WalError::Malformed),
        }
    }
    /// Whether the list is empty.
    pub fn is_empty(self) -> Result<bool, WalError> {
        Ok(self.len()? == 0)
    }
    /// Decodes one descriptor without allocation.
    pub fn get(
        self,
        index: usize,
        r: &mut WalResources<'_>,
    ) -> Result<ArtifactDescriptor, WalError> {
        r.charge(1)?;
        match self {
            Self::Values(v) => v.get(index).copied().ok_or(WalError::Malformed),
            Self::Encoded(v) => {
                let start = index.checked_mul(64).ok_or(WalError::Malformed)?;
                get_descriptor(
                    &mut Reader {
                        bytes: v
                            .get(start..start.checked_add(64).ok_or(WalError::Malformed)?)
                            .ok_or(WalError::Malformed)?,
                        pos: 0,
                    },
                    r,
                )
            }
        }
    }
}
