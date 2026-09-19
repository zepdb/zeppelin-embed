//! Fixed-page framing only. A FramedPage is not a validated directory: its owner
//! must resolve overflow keys and verify kind-specific ordering before routing.

use super::artifact::{self, add, put, read_u128, usize_from};
use super::artifact::{BlockKind, PhysicalRef};
use crate::format::frame::{self, FormatCheck, FormatError};
use crate::property_graph::{GraphGeneration, MAX_GRAPH_INPUT_BYTES};
use xxhash_rust::xxh3::{Xxh3, xxh3_64};

/// Fixed page geometry, including header, slots, cells and explicit zero tail.
pub const PAGE_BYTES: usize = 16 * 1024;
/// Append-only tree comparator domains, independent of block framing tags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum TreeKind {
    /// Full unsigned NodeId directory.
    Nodes = 1,
    /// Full unsigned RelId directory.
    Relationships = 2,
    /// Entity-kind, namespace-symbol, exact key bytes.
    KeyFences = 3,
    /// Label symbol then full NodeId.
    Labels = 4,
    /// Relationship type symbol then full RelId.
    RelationshipTypes = 5,
    /// NodeId, relationship type, lower RelId.
    OutRanges = 6,
    /// NodeId, relationship type, lower RelId.
    InRanges = 7,
    /// Full unsigned physical artifact nonce.
    ObjectInventory = 8,
}
/// Explicit logical-key representation. Overflow roots are never key bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key<'a> {
    /// Exact logical key bytes; comparison depends on the tree kind.
    Inline(&'a [u8]),
    /// Logical byte length and a framed overflow-key descriptor reference.
    /// Its extent/chunk semantics and streamed comparison belong to the tree owner.
    Overflow {
        /// Total reconstructed key bytes, at most the graph input bound.
        logical_length: u64,
        /// Required OverflowKey block, framing version one.
        reference: PhysicalRef,
    },
}
/// Borrowed framed cell, not a logically ordered/validated directory entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Cell<'a> {
    /// Leaf key descriptor and opaque value.
    Leaf {
        /// Exact inline/overflow key descriptor.
        key: Key<'a>,
        /// Value interpreted by the tree's owning codec.
        value: &'a [u8],
    },
    /// Exclusive upper separator; only the final child has unbounded upper None.
    Branch {
        /// None explicitly represents the final unbounded child.
        upper: Option<Key<'a>>,
        /// A framed TreePage reference, never an arbitrary record extent.
        child: PhysicalRef,
    },
}
/// Interpretation-critical page metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageHeader {
    /// Declared comparator domain.
    pub kind: TreeKind,
    /// Zero for leaf cells, positive for branch cells.
    pub level: u16,
    /// Immutable creation generation.
    pub generation: GraphGeneration,
}
/// A borrowed page with validated extents/checksums/descriptors only.
#[derive(Debug)]
pub struct FramedPage<'a> {
    bytes: &'a [u8],
    header: PageHeader,
}
impl FramedPage<'_> {
    /// Returns checked page metadata.
    pub const fn header(&self) -> PageHeader {
        self.header
    }
    /// Returns a structurally checked cell; overflow ordering remains unresolved.
    pub fn cell(&self, index: usize) -> Result<Cell<'_>, FormatError> {
        let count = frame::read_u32("graph page", self.bytes, 12)? as usize;
        if index >= count {
            return Err(invalid("page cell index out of range"));
        }
        let slot = add(
            64,
            index
                .checked_mul(8)
                .ok_or_else(|| invalid("slot overflow"))?,
        )?;
        let offset = frame::read_u32("graph page", self.bytes, slot)? as usize;
        let length = frame::read_u32("graph page", self.bytes, add(slot, 4)?)? as usize;
        parse_cell(
            self.header,
            self.bytes
                .get(offset..add(offset, length)?)
                .ok_or_else(|| invalid("cell outside page"))?,
        )
    }
}

/// Encodes one page into an exact, caller-reserved 16 KiB destination.
/// All cell geometry/descriptor checks precede changing the destination.
pub fn encode_page(
    header: PageHeader,
    cells: &[Cell<'_>],
    output: &mut [u8],
) -> Result<(), FormatError> {
    if output.len() != PAGE_BYTES {
        return Err(invalid("page reservation must be exactly 16 KiB"));
    }
    let slots = cells
        .len()
        .checked_mul(8)
        .ok_or_else(|| invalid("slot length overflow"))?;
    let mut extent = add(64, slots)?;
    for (index, cell) in cells.iter().enumerate() {
        validate_cell(header, *cell, index + 1 == cells.len())?;
        extent = add(extent, cell_len(*cell)?)?;
    }
    if extent > PAGE_BYTES || (header.level > 0 && cells.is_empty()) {
        return Err(invalid("cells do not fit the page"));
    }
    output.fill(0);
    put(output, 0, b"ZGTP")?;
    put(output, 4, &1_u16.to_le_bytes())?;
    put(output, 6, &(header.kind as u16).to_le_bytes())?;
    put(output, 8, &(PAGE_BYTES as u32).to_le_bytes())?;
    put(output, 12, &(cells.len() as u32).to_le_bytes())?;
    put(output, 16, &header.level.to_le_bytes())?;
    put(output, 20, &(slots as u32).to_le_bytes())?;
    put(output, 24, &header.generation.get().to_le_bytes())?;
    let mut offset = add(64, slots)?;
    for (index, cell) in cells.iter().enumerate() {
        let length = cell_len(*cell)?;
        let slot = add(64, index * 8)?;
        put(output, slot, &(offset as u32).to_le_bytes())?;
        put(output, add(slot, 4)?, &(length as u32).to_le_bytes())?;
        let target = output
            .get_mut(offset..add(offset, length)?)
            .ok_or_else(|| invalid("cell extent"))?;
        match *cell {
            Cell::Leaf { key, value } => {
                let key_bytes = key_len(key)?;
                put(target, 0, &(key_bytes as u32).to_le_bytes())?;
                put(target, 4, &(value.len() as u32).to_le_bytes())?;
                encode_key(
                    key,
                    target
                        .get_mut(8..add(8, key_bytes)?)
                        .ok_or_else(|| invalid("key extent"))?,
                )?;
                put(target, add(8, key_bytes)?, value)?;
            }
            Cell::Branch { upper, child } => {
                put(target, 0, &[u8::from(upper.is_none())])?;
                put(
                    target,
                    4,
                    &(upper.map(key_len).transpose()?.unwrap_or(0) as u32).to_le_bytes(),
                )?;
                artifact::encode_reference(
                    child,
                    target
                        .get_mut(8..40)
                        .ok_or_else(|| invalid("child extent"))?,
                )?;
                if let Some(key) = upper {
                    encode_key(
                        key,
                        target.get_mut(40..).ok_or_else(|| invalid("key extent"))?,
                    )?;
                }
            }
        }
        offset = add(offset, length)?;
    }
    let checksum = xxh3_64(output);
    put(output, 56, &checksum.to_le_bytes())
}

/// Checks page framing without following child/overflow references or admitting a tree.
/// Overflow streams and strict key ordering are mandatory later tree-owner checks.
pub fn decode_page(expected: TreeKind, bytes: &[u8]) -> Result<FramedPage<'_>, FormatError> {
    if bytes.len() != PAGE_BYTES || bytes.get(..4) != Some(b"ZGTP".as_slice()) {
        return Err(invalid("page length or magic"));
    }
    if frame::read_u16("graph page", bytes, 4)? != 1 {
        return Err(FormatError::new(
            "graph page",
            FormatCheck::Version,
            "unsupported page version",
        ));
    }
    if frame::read_u16("graph page", bytes, 6)? != expected as u16 {
        return Err(FormatError::new(
            "graph page",
            FormatCheck::Family,
            "wrong tree comparator kind",
        ));
    }
    if frame::read_u32("graph page", bytes, 8)? as usize != PAGE_BYTES
        || frame::read_u16("graph page", bytes, 18)? != 0
        || bytes
            .get(32..56)
            .is_none_or(|part| part.iter().any(|byte| *byte != 0))
    {
        return Err(invalid("page length, flags or reserved bytes"));
    }
    let stored = frame::read_u64("graph page", bytes, 56)?;
    let mut hasher = Xxh3::new();
    hasher.update(bytes.get(..56).ok_or_else(|| invalid("header extent"))?);
    hasher.update(&[0_u8; 8]);
    hasher.update(bytes.get(64..).ok_or_else(|| invalid("body extent"))?);
    if stored != hasher.digest() {
        return Err(FormatError::checksum_mismatch(
            "graph page",
            FormatCheck::BlockChecksum,
            stored,
            hasher.digest(),
        ));
    }
    let count = frame::read_u32("graph page", bytes, 12)? as usize;
    let slots = count
        .checked_mul(8)
        .ok_or_else(|| invalid("slot length overflow"))?;
    if slots != frame::read_u32("graph page", bytes, 20)? as usize {
        return Err(invalid("slot count differs from slot-array length"));
    }
    let header = PageHeader {
        kind: expected,
        level: frame::read_u16("graph page", bytes, 16)?,
        generation: GraphGeneration::new(frame::read_u64("graph page", bytes, 24)?),
    };
    let page = FramedPage { bytes, header };
    let mut next = add(64, slots)?;
    if next > PAGE_BYTES || (header.level > 0 && count == 0) {
        return Err(invalid("slot extent or empty branch"));
    }
    for index in 0..count {
        let slot = add(64, index * 8)?;
        let offset = frame::read_u32("graph page", bytes, slot)? as usize;
        let length = frame::read_u32("graph page", bytes, add(slot, 4)?)? as usize;
        if offset != next {
            return Err(invalid("overlapping or noncontiguous cells"));
        }
        next = add(offset, length)?;
        if next > PAGE_BYTES {
            return Err(invalid("cell exceeds page"));
        }
        validate_cell(header, page.cell(index)?, index + 1 == count)?;
    }
    if bytes
        .get(next..)
        .is_none_or(|tail| tail.iter().any(|byte| *byte != 0))
    {
        return Err(invalid("nonzero unused page tail"));
    }
    Ok(page)
}

/// Compares complete inline keys using declared numeric fields, never LE byte order.
/// This does not resolve overflow keys or establish whole-tree routing invariants.
pub fn compare_inline_keys(
    kind: TreeKind,
    left: &[u8],
    right: &[u8],
) -> Result<std::cmp::Ordering, FormatError> {
    validate_inline(kind, left)?;
    validate_inline(kind, right)?;
    let order = match kind {
        TreeKind::Nodes | TreeKind::Relationships | TreeKind::ObjectInventory => {
            read_u128(left, 0)?.cmp(&read_u128(right, 0)?)
        }
        TreeKind::Labels | TreeKind::RelationshipTypes => frame::read_u64("graph key", left, 0)?
            .cmp(&frame::read_u64("graph key", right, 0)?)
            .then(read_u128(left, 8)?.cmp(&read_u128(right, 8)?)),
        TreeKind::OutRanges | TreeKind::InRanges => read_u128(left, 0)?
            .cmp(&read_u128(right, 0)?)
            .then(
                frame::read_u64("graph key", left, 16)?.cmp(&frame::read_u64(
                    "graph key",
                    right,
                    16,
                )?),
            )
            .then(read_u128(left, 24)?.cmp(&read_u128(right, 24)?)),
        TreeKind::KeyFences => left
            .first()
            .cmp(&right.first())
            .then(frame::read_u64("graph key", left, 1)?.cmp(&frame::read_u64(
                "graph key",
                right,
                1,
            )?))
            .then(left.get(9..).cmp(&right.get(9..))),
    };
    Ok(order)
}

fn validate_inline(kind: TreeKind, bytes: &[u8]) -> Result<(), FormatError> {
    let valid = match kind {
        TreeKind::Nodes | TreeKind::Relationships | TreeKind::ObjectInventory => bytes.len() == 16,
        TreeKind::Labels | TreeKind::RelationshipTypes => bytes.len() == 24,
        TreeKind::OutRanges | TreeKind::InRanges => bytes.len() == 40,
        TreeKind::KeyFences => bytes.len() >= 9 && bytes.len() <= MAX_GRAPH_INPUT_BYTES,
    };
    if valid {
        Ok(())
    } else {
        Err(invalid("inline key has wrong declared width"))
    }
}
fn validate_key(kind: TreeKind, key: Key<'_>) -> Result<(), FormatError> {
    match key {
        Key::Inline(bytes) => validate_inline(kind, bytes),
        Key::Overflow {
            logical_length,
            reference,
        } => {
            if kind != TreeKind::KeyFences
                || logical_length < 9
                || logical_length > MAX_GRAPH_INPUT_BYTES as u64
                || reference.kind != BlockKind::OverflowKey
            {
                return Err(invalid("invalid overflow key descriptor"));
            }
            artifact::encode_reference(reference, &mut [0_u8; 32])
        }
    }
}
fn validate_cell(header: PageHeader, cell: Cell<'_>, last: bool) -> Result<(), FormatError> {
    match cell {
        Cell::Leaf { key, .. } if header.level == 0 => validate_key(header.kind, key),
        Cell::Branch { upper, child } if header.level > 0 => {
            if upper.is_none() != last || child.kind != BlockKind::TreePage {
                return Err(invalid("branch bound or child kind"));
            }
            artifact::encode_reference(child, &mut [0_u8; 32])?;
            if let Some(key) = upper {
                validate_key(header.kind, key)?;
            }
            Ok(())
        }
        _ => Err(invalid("cell kind differs from page level")),
    }
}
fn key_len(key: Key<'_>) -> Result<usize, FormatError> {
    match key {
        Key::Inline(bytes) => add(12, bytes.len()),
        Key::Overflow { .. } => Ok(44),
    }
}
fn cell_len(cell: Cell<'_>) -> Result<usize, FormatError> {
    match cell {
        Cell::Leaf { key, value } => add(add(8, key_len(key)?)?, value.len()),
        Cell::Branch { upper, .. } => add(40, upper.map(key_len).transpose()?.unwrap_or(0)),
    }
}
fn encode_key(key: Key<'_>, output: &mut [u8]) -> Result<(), FormatError> {
    match key {
        Key::Inline(bytes) => {
            put(output, 0, &[0])?;
            put(output, 4, &(bytes.len() as u64).to_le_bytes())?;
            put(output, 12, bytes)
        }
        Key::Overflow {
            logical_length,
            reference,
        } => {
            put(output, 0, &[1])?;
            put(output, 4, &logical_length.to_le_bytes())?;
            artifact::encode_reference(
                reference,
                output
                    .get_mut(12..)
                    .ok_or_else(|| invalid("overflow reference extent"))?,
            )
        }
    }
}
fn parse_key(kind: TreeKind, bytes: &[u8]) -> Result<Key<'_>, FormatError> {
    if bytes
        .get(1..4)
        .is_none_or(|reserved| reserved.iter().any(|byte| *byte != 0))
    {
        return Err(invalid("key descriptor reserved bytes"));
    }
    let length = frame::read_u64("graph key", bytes, 4)?;
    if length > MAX_GRAPH_INPUT_BYTES as u64 {
        return Err(invalid("logical key exceeds input limit"));
    }
    let key = match bytes.first() {
        Some(0) => {
            let raw = bytes
                .get(12..)
                .ok_or_else(|| invalid("inline key extent"))?;
            if raw.len() != usize_from(length)? {
                return Err(invalid("inline key length mismatch"));
            }
            Key::Inline(raw)
        }
        Some(1) => Key::Overflow {
            logical_length: length,
            reference: artifact::decode_reference(
                bytes
                    .get(12..)
                    .ok_or_else(|| invalid("overflow reference extent"))?,
            )?,
        },
        _ => return Err(invalid("unknown key descriptor tag")),
    };
    validate_key(kind, key)?;
    Ok(key)
}
fn parse_cell(header: PageHeader, bytes: &[u8]) -> Result<Cell<'_>, FormatError> {
    if header.level == 0 {
        let key_bytes = frame::read_u32("graph leaf", bytes, 0)? as usize;
        let value_bytes = frame::read_u32("graph leaf", bytes, 4)? as usize;
        let end = add(8, key_bytes)?;
        if add(end, value_bytes)? != bytes.len() {
            return Err(invalid("leaf cell lengths do not consume its extent"));
        }
        let key = parse_key(
            header.kind,
            bytes
                .get(8..end)
                .ok_or_else(|| invalid("leaf key extent"))?,
        )?;
        Ok(Cell::Leaf {
            key,
            value: bytes
                .get(end..)
                .ok_or_else(|| invalid("leaf value extent"))?,
        })
    } else {
        if bytes
            .get(1..4)
            .is_none_or(|reserved| reserved.iter().any(|byte| *byte != 0))
        {
            return Err(invalid("branch reserved bytes"));
        }
        let key_bytes = frame::read_u32("graph branch", bytes, 4)? as usize;
        if add(40, key_bytes)? != bytes.len() {
            return Err(invalid("branch key length mismatch"));
        }
        let child = artifact::decode_reference(
            bytes
                .get(8..40)
                .ok_or_else(|| invalid("branch child extent"))?,
        )?;
        let upper = match bytes.first() {
            Some(0) => Some(parse_key(
                header.kind,
                bytes
                    .get(40..)
                    .ok_or_else(|| invalid("branch key extent"))?,
            )?),
            Some(1) if key_bytes == 0 => None,
            _ => return Err(invalid("invalid branch bound tag or length")),
        };
        Ok(Cell::Branch { upper, child })
    }
}
fn invalid(detail: &str) -> FormatError {
    FormatError::new("native graph tree page", FormatCheck::BlockLength, detail)
}
