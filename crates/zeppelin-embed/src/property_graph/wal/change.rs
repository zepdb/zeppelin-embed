use super::codec::*;
use super::*;
use crate::property_graph::storage::artifact::BlockKind;
use crate::property_graph::{
    ApplicationKey, EntityId, EntityKind, ExpectedGraphState, GraphDeleteMode, GraphOperation,
    GraphRevision, MAX_GRAPH_INPUT_BYTES, NodeId, RelId,
};

fn kind(v: EntityKind) -> u8 {
    match v {
        EntityKind::Node => 1,
        EntityKind::Relationship => 2,
    }
}
fn entity(v: EntityId, w: &mut Writer<'_>, r: &mut WalResources<'_>) -> Result<(), WalError> {
    w.u8(kind(v.kind()), r)?;
    w.u128(
        match v {
            EntityId::Node(id) => id.get(),
            EntityId::Relationship(id) => id.get(),
        },
        r,
    )
}
fn read_kind(v: u8) -> Result<EntityKind, WalError> {
    match v {
        1 => Ok(EntityKind::Node),
        2 => Ok(EntityKind::Relationship),
        _ => Err(WalError::Malformed),
    }
}
fn read_entity(rd: &mut Reader<'_>, r: &mut WalResources<'_>) -> Result<EntityId, WalError> {
    let kind = read_kind(rd.u8(r)?)?;
    let id = rd.u128(r)?;
    match kind {
        EntityKind::Node => NodeId::new(id).map(EntityId::Node),
        EntityKind::Relationship => RelId::new(id).map(EntityId::Relationship),
    }
    .map_err(|_| WalError::Malformed)
}
fn blob(v: &[u8], w: &mut Writer<'_>, r: &mut WalResources<'_>) -> Result<(), WalError> {
    w.u64(v.len() as u64, r)?;
    w.put(v, r)
}
fn text<'a>(rd: &mut Reader<'a>, r: &mut WalResources<'_>) -> Result<&'a str, WalError> {
    let n = usize::try_from(rd.u64(r)?).map_err(|_| WalError::Capacity)?;
    if n > MAX_GRAPH_INPUT_BYTES {
        return Err(WalError::Capacity);
    }
    let bytes = rd.take(n, r)?;
    let mut remaining = bytes;
    while !remaining.is_empty() {
        let n = remaining.len().min(CHUNK);
        r.charge(n as u64)?;
        let part = remaining.get(..n).ok_or(WalError::Malformed)?;
        let consumed = match std::str::from_utf8(part) {
            Ok(_) => n,
            Err(e) if e.error_len().is_none() && n < remaining.len() => e.valid_up_to(),
            Err(_) => return Err(WalError::Malformed),
        };
        if consumed == 0 {
            return Err(WalError::Malformed);
        }
        remaining = remaining.get(consumed..).ok_or(WalError::Malformed)?;
    }
    // SAFETY: all bytes were checked in complete immutable UTF-8 spans above;
    // a code point crossing a chunk boundary remained for the next validation.
    Ok(unsafe { std::str::from_utf8_unchecked(bytes) })
}
fn provenance(
    v: Mutation<'_>,
    w: &mut Writer<'_>,
    r: &mut WalResources<'_>,
) -> Result<(), WalError> {
    let p = v.provenance;
    w.put(b"ZGOP", r)?;
    w.u16(v.provenance_version, r)?;
    w.u8(
        match p.operation {
            GraphOperation::StructuredCreate => 1,
            GraphOperation::StructuredPut => 2,
            GraphOperation::StructuredDelete => 3,
            GraphOperation::StructuredRecreate => 4,
            GraphOperation::CypherEdit => 5,
        },
        r,
    )?;
    w.u8(u8::from(p.key.is_some()), r)?;
    if let Some(key) = p.key {
        w.u8(kind(key.kind()), r)?;
        blob(key.namespace().as_str().as_bytes(), w, r)?;
        blob(key.key().as_str().as_bytes(), w, r)?;
    }
    w.u64(p.requested_revision.get(), r)?;
    w.u64(p.installed_revision.get(), r)?;
    match p.expected {
        ExpectedGraphState::Absent => w.u8(1, r)?,
        ExpectedGraphState::Entity(id) => {
            w.u8(2, r)?;
            entity(id, w, r)?;
        }
        ExpectedGraphState::Deletion(rev) => {
            w.u8(3, r)?;
            w.u64(rev.get(), r)?;
        }
    }
    entity(p.incarnation, w, r)?;
    w.u8(
        match p.delete_mode {
            None => 0,
            Some(GraphDeleteMode::Restrict) => 1,
            Some(GraphDeleteMode::Detach) => 2,
        },
        r,
    )?;
    w.u64(p.original_generation.get(), r)
}
fn read_provenance<'a>(
    rd: &mut Reader<'a>,
    r: &mut WalResources<'_>,
) -> Result<(u16, OperationFields<'a>), WalError> {
    if rd.take(4, r)? != b"ZGOP" {
        return Err(WalError::Malformed);
    }
    let version = rd.u16(r)?;
    if version != 1 {
        return Err(WalError::Unsupported);
    }
    let operation = match rd.u8(r)? {
        1 => GraphOperation::StructuredCreate,
        2 => GraphOperation::StructuredPut,
        3 => GraphOperation::StructuredDelete,
        4 => GraphOperation::StructuredRecreate,
        5 => GraphOperation::CypherEdit,
        _ => return Err(WalError::Unsupported),
    };
    let key = match rd.u8(r)? {
        0 => None,
        1 => {
            let k = read_kind(rd.u8(r)?)?;
            Some(
                ApplicationKey::new(k, text(rd, r)?, text(rd, r)?)
                    .map_err(|_| WalError::Malformed)?,
            )
        }
        _ => return Err(WalError::Malformed),
    };
    let requested_revision = GraphRevision::new(rd.u64(r)?).map_err(|_| WalError::Malformed)?;
    let installed_revision = GraphRevision::new(rd.u64(r)?).map_err(|_| WalError::Malformed)?;
    let expected = match rd.u8(r)? {
        1 => ExpectedGraphState::Absent,
        2 => ExpectedGraphState::Entity(read_entity(rd, r)?),
        3 => ExpectedGraphState::Deletion(
            GraphRevision::new(rd.u64(r)?).map_err(|_| WalError::Malformed)?,
        ),
        _ => return Err(WalError::Malformed),
    };
    let incarnation = read_entity(rd, r)?;
    let delete_mode = match rd.u8(r)? {
        0 => None,
        1 => Some(GraphDeleteMode::Restrict),
        2 => Some(GraphDeleteMode::Detach),
        _ => return Err(WalError::Malformed),
    };
    let original_generation = GraphGeneration::new(rd.u64(r)?);
    Ok((
        version,
        OperationFields {
            operation,
            key,
            requested_revision,
            installed_revision,
            expected,
            incarnation,
            delete_mode,
            original_generation,
        },
    ))
}
pub(super) fn validate_mutation(v: Mutation<'_>, state: CommitState<'_>) -> Result<(), WalError> {
    let p = v.provenance;
    if v.provenance_version != 1 {
        return Err(WalError::Unsupported);
    }
    if p.key.is_some_and(|k| k.kind() != p.incarnation.kind())
        || p.requested_revision != p.installed_revision
        || p.original_generation != state.generation
        || matches!(p.expected,ExpectedGraphState::Entity(id) if id!=p.incarnation)
    {
        return Err(WalError::Participant);
    }
    let (id, high) = match p.incarnation {
        EntityId::Node(id) => (id.get(), state.high_waters.node),
        EntityId::Relationship(id) => (id.get(), state.high_waters.relationship),
    };
    if id > high {
        return Err(WalError::HighWater);
    }
    if v.live != v.canonical.is_some()
        || (!v.live && (v.membership.text_after || v.membership.vector_after))
        || (p.incarnation.kind() == EntityKind::Relationship
            && (v.membership != Membership::default()
                || p.delete_mode == Some(GraphDeleteMode::Detach)))
    {
        return Err(WalError::Participant);
    }
    let admissible = match p.operation {
        GraphOperation::StructuredCreate => {
            p.key.is_some()
                && p.expected == ExpectedGraphState::Absent
                && v.live
                && p.delete_mode.is_none()
        }
        GraphOperation::StructuredPut => {
            p.key.is_some()
                && matches!(p.expected, ExpectedGraphState::Entity(_))
                && v.live
                && p.delete_mode.is_none()
        }
        GraphOperation::StructuredDelete => {
            p.key.is_some()
                && matches!(p.expected, ExpectedGraphState::Entity(_))
                && !v.live
                && p.delete_mode.is_some()
        }
        GraphOperation::StructuredRecreate => {
            p.key.is_some()
                && matches!(p.expected,ExpectedGraphState::Deletion(d)if d.get()<p.requested_revision.get())
                && v.live
                && p.delete_mode.is_none()
        }
        GraphOperation::CypherEdit => {
            !matches!(p.expected, ExpectedGraphState::Deletion(_))
                && (v.live || matches!(p.expected, ExpectedGraphState::Entity(_)))
                && (v.live == p.delete_mode.is_none())
        }
    };
    if !admissible {
        return Err(WalError::Participant);
    }
    if let Some(reference) = v.canonical {
        if !matches!(
            reference.block.kind,
            BlockKind::CanonicalImage | BlockKind::ExtentList
        ) {
            return Err(WalError::Participant);
        }
        super::framing::validate_ref(reference, state, reference.block.kind)?;
    }
    Ok(())
}
pub(super) fn tag(v: Change<'_>) -> u16 {
    match v {
        Change::Mutation(_) => 2,
        Change::Inventory(_) => 3,
        Change::ReclaimIntent(_) => 4,
        Change::ReclaimComplete(_) => 5,
    }
}
pub(super) fn write(
    v: Change<'_>,
    state: CommitState<'_>,
    w: &mut Writer<'_>,
    r: &mut WalResources<'_>,
) -> Result<(), WalError> {
    match v {
        Change::Mutation(v) => {
            validate_mutation(v, state)?;
            let mut measure = Writer {
                bytes: None,
                pos: 0,
            };
            provenance(v, &mut measure, r)?;
            if measure.pos > MAX_GRAPH_INPUT_BYTES {
                return Err(WalError::Capacity);
            }
            w.u8(u8::from(v.live), r)?;
            let m = v.membership;
            w.u8(
                u8::from(m.text_before)
                    | (u8::from(m.text_after) << 1)
                    | (u8::from(m.vector_before) << 2)
                    | (u8::from(m.vector_after) << 3),
                r,
            )?;
            w.put(&[0; 6], r)?;
            w.u64(measure.pos as u64, r)?;
            provenance(v, w, r)?;
            put_optional(v.canonical, w, r)
        }
        _ => super::maintenance::write(v, state, w, r),
    }
}
pub(super) fn read<'a>(
    tag: u16,
    payload: &'a [u8],
    state: CommitState<'_>,
    r: &mut WalResources<'_>,
) -> Result<Change<'a>, WalError> {
    let mut rd = Reader {
        bytes: payload,
        pos: 0,
    };
    let change = match tag {
        2 => {
            let live = match rd.u8(r)? {
                0 => false,
                1 => true,
                _ => return Err(WalError::Malformed),
            };
            let flags = rd.u8(r)?;
            if flags & !15 != 0 {
                return Err(WalError::Malformed);
            }
            rd.zero(6, r)?;
            let n = usize::try_from(rd.u64(r)?).map_err(|_| WalError::Capacity)?;
            if n > MAX_GRAPH_INPUT_BYTES {
                return Err(WalError::Capacity);
            }
            let mut pr = Reader {
                bytes: rd.take(n, r)?,
                pos: 0,
            };
            let (provenance_version, provenance) = read_provenance(&mut pr, r)?;
            pr.end()?;
            let canonical = get_optional(&mut rd, r)?;
            let v = Mutation {
                provenance_version,
                provenance,
                live,
                canonical,
                membership: Membership {
                    text_before: flags & 1 != 0,
                    text_after: flags & 2 != 0,
                    vector_before: flags & 4 != 0,
                    vector_after: flags & 8 != 0,
                },
            };
            validate_mutation(v, state)?;
            Change::Mutation(v)
        }
        _ => super::maintenance::read(tag, &mut rd, state, r)?,
    };
    rd.end()?;
    Ok(change)
}
