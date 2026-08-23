//! Fixed-stride, cache-line-aligned graph node-block persistence.

use crate::kernels::Bit4Row;
use crate::quant::Bit4Factors;
use xxhash_rust::xxh3::xxh3_64;

/// Cache-line width frozen by the graph node-block v1 format.
pub const CACHE_LINE_BYTES: usize = 128;
/// Fixed-width metadata trailer following all directly addressable blocks.
pub const NODE_BLOCK_TRAILER_LEN: usize = CACHE_LINE_BYTES;
/// Sentinel occupying every unused neighbour slot.
pub const UNUSED_NEIGHBOR_ID: u32 = u32::MAX;

const NODE_BLOCK_MAGIC: [u8; 8] = *b"ZEGRNB01";
const NODE_BLOCK_VERSION: u16 = 1;
const NODE_METADATA_BYTES: usize = 16;
const ALLOWED_NODE_FLAGS: u8 = 0b0000_0011;

/// Validated geometry shared by every node block in one region.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphNodeLayout {
    dims: u32,
    padded_dims: u32,
    max_degree: u8,
    stride: u32,
}

impl GraphNodeLayout {
    /// Derives the frozen stride from logical/padded dimensions and maximum degree.
    pub fn new(dims: u32, padded_dims: u32, max_degree: u8) -> Result<Self, GraphNodeError> {
        if dims == 0 {
            return Err(GraphNodeError::Layout(
                "logical dimensions must not be zero".to_owned(),
            ));
        }
        if padded_dims < dims {
            return Err(GraphNodeError::Layout(format!(
                "padded dimensions {padded_dims} are below logical dimensions {dims}"
            )));
        }
        if !padded_dims.is_multiple_of(CACHE_LINE_BYTES as u32) {
            return Err(GraphNodeError::Layout(format!(
                "padded dimensions {padded_dims} are not a multiple of {CACHE_LINE_BYTES}"
            )));
        }
        let code_bytes = usize::try_from(padded_dims)
            .map_err(|_| GraphNodeError::Layout("padded dimensions exceed usize".to_owned()))?
            .div_ceil(2);
        let neighbor_bytes = usize::from(max_degree)
            .checked_mul(std::mem::size_of::<u32>())
            .ok_or_else(|| GraphNodeError::Layout("neighbour bytes overflow".to_owned()))?;
        let unaligned = code_bytes
            .checked_add(NODE_METADATA_BYTES)
            .and_then(|value| value.checked_add(neighbor_bytes))
            .ok_or_else(|| GraphNodeError::Layout("node block size overflow".to_owned()))?;
        let stride = round_up_cache_line(unaligned)?;
        Ok(Self {
            dims,
            padded_dims,
            max_degree,
            stride: u32::try_from(stride)
                .map_err(|_| GraphNodeError::Layout("node stride exceeds u32".to_owned()))?,
        })
    }

    /// Returns the logical vector dimension.
    #[must_use]
    pub const fn dims(self) -> u32 {
        self.dims
    }

    /// Returns the stored, zero-padded vector dimension.
    #[must_use]
    pub const fn padded_dims(self) -> u32 {
        self.padded_dims
    }

    /// Returns the fixed neighbour-slot capacity.
    #[must_use]
    pub const fn max_degree(self) -> u8 {
        self.max_degree
    }

    /// Returns packed Bit4 bytes stored at the start of every block.
    #[must_use]
    pub const fn code_bytes(self) -> usize {
        (self.padded_dims as usize).div_ceil(2)
    }

    /// Returns the cache-line-rounded bytes between adjacent node ids.
    #[must_use]
    pub const fn stride(self) -> u32 {
        self.stride
    }

    /// Returns the byte offset for a dense segment-local node/row id.
    pub fn block_offset(self, node_id: u32) -> Result<u32, GraphNodeError> {
        node_id
            .checked_mul(self.stride)
            .ok_or(GraphNodeError::ArithmeticOverflow)
    }
}

/// Borrowed values written into one fixed-stride node block.
#[derive(Clone, Copy, Debug)]
pub struct GraphNodeBlockInput<'a> {
    /// Existing frozen MSB-first packed Bit4 row, including zero padding.
    pub codes: &'a [u8],
    /// Persisted Bit4 factor fields in their frozen declaration order.
    pub factors: Bit4Factors,
    /// Bit 0 marks an entry seed; bit 1 marks a hub.
    pub flags: u8,
    /// Dense segment-local neighbour row ids.
    pub neighbors: &'a [u32],
}

/// Complete borrowed node-block set supplied to the production writer.
#[derive(Clone, Copy, Debug)]
pub struct GraphNodeBlockBuild<'a> {
    /// Shared fixed-stride geometry.
    pub layout: GraphNodeLayout,
    /// Nodes in dense row-id order; array position is the graph node id.
    pub nodes: &'a [GraphNodeBlockInput<'a>],
}

/// Owned encoded graph region produced by the production writer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedNodeBlocks {
    bytes: Vec<u8>,
    layout: GraphNodeLayout,
    node_count: u32,
}

/// Validated borrowed view over one complete mmap-backed graph region.
#[derive(Clone, Copy, Debug)]
pub struct GraphNodeBlocks<'a> {
    bytes: &'a [u8],
    layout: GraphNodeLayout,
    node_count: u32,
    block_bytes: usize,
}

/// Dense node id whose fixed-stride byte offset was checked once.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CheckedNodeId {
    raw: u32,
    offset: usize,
}

impl CheckedNodeId {
    #[must_use]
    pub(super) const fn raw(self) -> u32 {
        self.raw
    }
}

impl GraphNodeBlocks<'_> {
    /// Returns the geometry validated from the fixed metadata trailer.
    #[must_use]
    pub const fn layout(&self) -> GraphNodeLayout {
        self.layout
    }

    /// Returns the dense segment-local graph node / row count.
    #[must_use]
    pub const fn node_count(&self) -> u32 {
        self.node_count
    }

    /// Returns one validated node block by its identical segment row id.
    pub fn block(&self, node_id: u32) -> Result<GraphNodeBlock<'_>, GraphNodeError> {
        self.block_checked(self.checked_node_id(node_id)?)
    }

    pub(super) fn checked_node_id(&self, node_id: u32) -> Result<CheckedNodeId, GraphNodeError> {
        if node_id >= self.node_count {
            return Err(GraphNodeError::NodeIdOutOfRange {
                node_id,
                node_count: self.node_count,
            });
        }
        let offset = usize::try_from(self.layout.block_offset(node_id)?)
            .map_err(|_| GraphNodeError::ArithmeticOverflow)?;
        Ok(CheckedNodeId {
            raw: node_id,
            offset,
        })
    }

    pub(super) fn block_checked(
        &self,
        node_id: CheckedNodeId,
    ) -> Result<GraphNodeBlock<'_>, GraphNodeError> {
        let end = node_id
            .offset
            .checked_add(self.layout.stride as usize)
            .ok_or(GraphNodeError::ArithmeticOverflow)?;
        let block = self
            .bytes
            .get(node_id.offset..end.min(self.block_bytes))
            .ok_or_else(|| GraphNodeError::InvalidNode {
                node_id: node_id.raw,
                detail: "validated block range is unavailable".to_owned(),
            })?;
        decode_block_view(self.layout, node_id.raw, block)
    }

    pub(super) fn code_row_checked(
        &self,
        node_id: CheckedNodeId,
    ) -> Result<Bit4Row<'_>, GraphNodeError> {
        Bit4Row::from_mapped_region(self.bytes, node_id.offset, self.layout.code_bytes())
            .ok_or_else(|| GraphNodeError::InvalidNode {
                node_id: node_id.raw,
                detail: "validated Bit4 row is unavailable".to_owned(),
            })
    }

    pub(super) fn prefetch_line0(&self, node_id: CheckedNodeId) {
        if let Some(bytes) = self.bytes.get(node_id.offset..) {
            prefetch_address(bytes.as_ptr());
        }
    }

    pub(super) fn prefetch_head_block(&self, node_id: CheckedNodeId) {
        let Some(bytes) = self.bytes.get(node_id.offset..) else {
            return;
        };
        prefetch_address(bytes.as_ptr());
        if self.layout.stride as usize > CACHE_LINE_BYTES
            && let Some(second_line) = bytes.get(CACHE_LINE_BYTES..)
        {
            prefetch_address(second_line.as_ptr());
        }
    }
}

#[inline]
fn prefetch_address(address: *const u8) {
    #[cfg(target_arch = "aarch64")]
    // SAFETY: callers derive the address from a live immutable graph-region slice.
    unsafe {
        std::arch::asm!(
            "prfm pldl1keep, [{address}]",
            address = in(reg) address,
            options(readonly, nostack)
        );
    }
    #[cfg(not(target_arch = "aarch64"))]
    let _ = address;
}

/// Borrowed fields from one validated fixed-stride node block.
#[derive(Clone, Copy, Debug)]
pub struct GraphNodeBlock<'a> {
    codes: &'a [u8],
    factors: Bit4Factors,
    degree: u8,
    flags: u8,
    reserved: [u8; 2],
    neighbors: &'a [u8],
    padding: &'a [u8],
}

impl GraphNodeBlock<'_> {
    /// Returns the existing frozen MSB-first packed Bit4 row.
    #[must_use]
    pub const fn codes(&self) -> &[u8] {
        self.codes
    }

    /// Returns the three persisted Bit4 factor fields.
    #[must_use]
    pub const fn factors(&self) -> Bit4Factors {
        self.factors
    }

    /// Returns the active neighbour count.
    #[must_use]
    pub const fn degree(&self) -> u8 {
        self.degree
    }

    /// Returns the entry-seed/hub flag byte.
    #[must_use]
    pub const fn flags(&self) -> u8 {
        self.flags
    }

    /// Returns the two must-be-zero bytes after degree and flags.
    #[must_use]
    pub const fn reserved_bytes(&self) -> [u8; 2] {
        self.reserved
    }

    /// Iterates all fixed slots, including `u32::MAX` unused sentinels.
    #[must_use]
    pub fn neighbors_padded(&self) -> NeighborIds<'_> {
        NeighborIds {
            chunks: self.neighbors.chunks_exact(std::mem::size_of::<u32>()),
        }
    }

    /// Returns cache-line rounding bytes, all validated as zero.
    #[must_use]
    pub const fn padding_bytes(&self) -> &[u8] {
        self.padding
    }
}

/// Exact-size iterator over little-endian fixed neighbour slots.
#[derive(Clone, Debug)]
pub struct NeighborIds<'a> {
    chunks: std::slice::ChunksExact<'a, u8>,
}

impl Iterator for NeighborIds<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        let raw: [u8; 4] = self.chunks.next()?.try_into().ok()?;
        Some(u32::from_le_bytes(raw))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let length = self.chunks.len();
        (length, Some(length))
    }
}

impl ExactSizeIterator for NeighborIds<'_> {}

impl EncodedNodeBlocks {
    /// Returns the complete region bytes, including the fixed metadata trailer.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consumes the wrapper and returns the complete persisted region bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Returns the byte offset of a present dense node id.
    pub fn block_offset(&self, node_id: u32) -> Result<u32, GraphNodeError> {
        if node_id >= self.node_count {
            return Err(GraphNodeError::NodeIdOutOfRange {
                node_id,
                node_count: self.node_count,
            });
        }
        self.layout.block_offset(node_id)
    }
}

/// Typed graph node-block encode/decode failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GraphNodeError {
    /// The region is shorter than its bounded fixed trailer or block geometry.
    Truncated {
        /// Minimum bytes required by the declared format.
        minimum: usize,
        /// Bytes supplied by the caller.
        actual: usize,
    },
    /// Fixed magic, version, flags, lengths, or reserved trailer bytes are invalid.
    InvalidHeader(String),
    /// Layout geometry cannot describe a bounded fixed-stride region.
    Layout(String),
    /// A node id is outside the dense row-id range.
    NodeIdOutOfRange {
        /// Requested graph node / segment row id.
        node_id: u32,
        /// Number of nodes present in the region.
        node_count: u32,
    },
    /// One node's production input violates the persisted contract.
    InvalidNode {
        /// Dense graph node / segment row id.
        node_id: u32,
        /// Contract violation.
        detail: String,
    },
    /// Checked offset or length arithmetic overflowed.
    ArithmeticOverflow,
}

impl std::fmt::Display for GraphNodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated { minimum, actual } => write!(
                formatter,
                "graph node region is truncated: need {minimum} bytes, got {actual}"
            ),
            Self::InvalidHeader(detail) => {
                write!(formatter, "graph node region header is invalid: {detail}")
            }
            Self::Layout(detail) => write!(formatter, "graph node layout is invalid: {detail}"),
            Self::NodeIdOutOfRange {
                node_id,
                node_count,
            } => write!(
                formatter,
                "graph node id {node_id} is outside dense row count {node_count}"
            ),
            Self::InvalidNode { node_id, detail } => {
                write!(formatter, "graph node {node_id} is invalid: {detail}")
            }
            Self::ArithmeticOverflow => {
                formatter.write_str("graph node block arithmetic overflowed")
            }
        }
    }
}

impl std::error::Error for GraphNodeError {}

/// Encodes node blocks in dense row-id order followed by a 128-byte trailer.
pub fn encode_node_blocks(
    build: GraphNodeBlockBuild<'_>,
) -> Result<EncodedNodeBlocks, GraphNodeError> {
    let node_count = u32::try_from(build.nodes.len())
        .map_err(|_| GraphNodeError::Layout("node count exceeds u32".to_owned()))?;
    let block_bytes = usize::try_from(build.layout.stride)
        .map_err(|_| GraphNodeError::ArithmeticOverflow)?
        .checked_mul(build.nodes.len())
        .ok_or(GraphNodeError::ArithmeticOverflow)?;
    let total_bytes = block_bytes
        .checked_add(NODE_BLOCK_TRAILER_LEN)
        .ok_or(GraphNodeError::ArithmeticOverflow)?;
    let mut output = Vec::with_capacity(total_bytes);
    for (position, node) in build.nodes.iter().enumerate() {
        let node_id = u32::try_from(position).map_err(|_| GraphNodeError::ArithmeticOverflow)?;
        encode_node(build.layout, node_id, node, node_count, &mut output)?;
    }
    encode_trailer(build.layout, node_count, &mut output);
    Ok(EncodedNodeBlocks {
        bytes: output,
        layout: build.layout,
        node_count,
    })
}

/// Validates and borrows a complete fixed-stride graph node-block region.
pub fn decode_node_blocks(bytes: &[u8]) -> Result<GraphNodeBlocks<'_>, GraphNodeError> {
    if bytes.len() < NODE_BLOCK_TRAILER_LEN {
        return Err(GraphNodeError::Truncated {
            minimum: NODE_BLOCK_TRAILER_LEN,
            actual: bytes.len(),
        });
    }
    let trailer_start = bytes
        .len()
        .checked_sub(NODE_BLOCK_TRAILER_LEN)
        .ok_or(GraphNodeError::ArithmeticOverflow)?;
    let trailer = bytes
        .get(trailer_start..)
        .ok_or(GraphNodeError::Truncated {
            minimum: NODE_BLOCK_TRAILER_LEN,
            actual: bytes.len(),
        })?;
    if trailer.get(..8) != Some(NODE_BLOCK_MAGIC.as_slice()) {
        return Err(GraphNodeError::InvalidHeader(
            "magic does not match ZEGRNB01".to_owned(),
        ));
    }
    let version = read_u16(trailer, 8, "version")?;
    if version != NODE_BLOCK_VERSION {
        return Err(GraphNodeError::InvalidHeader(format!(
            "version {version}, expected {NODE_BLOCK_VERSION}"
        )));
    }
    if read_u16(trailer, 10, "flags")? != 0 {
        return Err(GraphNodeError::InvalidHeader(
            "reserved flags are non-zero".to_owned(),
        ));
    }
    let dims = read_u32(trailer, 12, "logical dimensions")?;
    let padded_dims = read_u32(trailer, 16, "padded dimensions")?;
    let max_degree = *trailer.get(20).ok_or(GraphNodeError::Truncated {
        minimum: 21,
        actual: trailer.len(),
    })?;
    if trailer
        .get(21..24)
        .is_none_or(|reserved| reserved.iter().any(|byte| *byte != 0))
    {
        return Err(GraphNodeError::InvalidHeader(
            "degree padding is non-zero".to_owned(),
        ));
    }
    let stored_stride = read_u32(trailer, 24, "stride")?;
    let node_count = read_u32(trailer, 28, "node count")?;
    let stored_checksum = read_u64(trailer, 32, "region checksum")?;
    let checksummed_end = trailer_start
        .checked_add(32)
        .ok_or(GraphNodeError::ArithmeticOverflow)?;
    let checksummed = bytes
        .get(..checksummed_end)
        .ok_or(GraphNodeError::Truncated {
            minimum: checksummed_end,
            actual: bytes.len(),
        })?;
    let actual_checksum = xxh3_64(checksummed);
    if actual_checksum != stored_checksum {
        return Err(GraphNodeError::InvalidHeader(format!(
            "xxh3-64 expected {stored_checksum:#018x}, computed {actual_checksum:#018x}"
        )));
    }
    if trailer
        .get(40..)
        .is_none_or(|reserved| reserved.iter().any(|byte| *byte != 0))
    {
        return Err(GraphNodeError::InvalidHeader(
            "reserved trailer bytes are non-zero".to_owned(),
        ));
    }
    let layout = GraphNodeLayout::new(dims, padded_dims, max_degree)?;
    if stored_stride != layout.stride {
        return Err(GraphNodeError::InvalidHeader(format!(
            "stored stride {stored_stride}, formula yields {}",
            layout.stride
        )));
    }
    let block_bytes = usize::try_from(layout.stride)
        .map_err(|_| GraphNodeError::ArithmeticOverflow)?
        .checked_mul(node_count as usize)
        .ok_or(GraphNodeError::ArithmeticOverflow)?;
    if trailer_start != block_bytes {
        let minimum = block_bytes
            .checked_add(NODE_BLOCK_TRAILER_LEN)
            .ok_or(GraphNodeError::ArithmeticOverflow)?;
        if bytes.len() < minimum {
            return Err(GraphNodeError::Truncated {
                minimum,
                actual: bytes.len(),
            });
        }
        return Err(GraphNodeError::InvalidHeader(format!(
            "region length {}, expected {minimum}",
            bytes.len()
        )));
    }
    for node_id in 0..node_count {
        let start = usize::try_from(layout.block_offset(node_id)?)
            .map_err(|_| GraphNodeError::ArithmeticOverflow)?;
        let end = start
            .checked_add(layout.stride as usize)
            .ok_or(GraphNodeError::ArithmeticOverflow)?;
        let block = bytes.get(start..end).ok_or(GraphNodeError::Truncated {
            minimum: end,
            actual: bytes.len(),
        })?;
        validate_decoded_node(layout, node_id, node_count, block)?;
    }
    Ok(GraphNodeBlocks {
        bytes,
        layout,
        node_count,
        block_bytes,
    })
}

fn validate_decoded_node(
    layout: GraphNodeLayout,
    node_id: u32,
    node_count: u32,
    block: &[u8],
) -> Result<(), GraphNodeError> {
    let view = decode_block_view(layout, node_id, block)?;
    validate_code_padding(layout, node_id, view.codes)?;
    if view.flags & !ALLOWED_NODE_FLAGS != 0 {
        return Err(GraphNodeError::InvalidNode {
            node_id,
            detail: format!("reserved flag bits are non-zero: {:#04x}", view.flags),
        });
    }
    if view.reserved != [0, 0] {
        return Err(GraphNodeError::InvalidNode {
            node_id,
            detail: "reserved bytes are non-zero".to_owned(),
        });
    }
    if view.padding.iter().any(|byte| *byte != 0) {
        return Err(GraphNodeError::InvalidNode {
            node_id,
            detail: "cache-line padding is non-zero".to_owned(),
        });
    }
    let degree = usize::from(view.degree);
    if degree > usize::from(layout.max_degree) {
        return Err(GraphNodeError::InvalidNode {
            node_id,
            detail: format!("degree {degree}, maximum {}", layout.max_degree),
        });
    }
    for (slot, neighbor) in view.neighbors_padded().enumerate() {
        if slot < degree {
            if neighbor >= node_count {
                return Err(GraphNodeError::InvalidNode {
                    node_id,
                    detail: format!(
                        "active neighbour slot {slot} id {neighbor} is outside row count {node_count}"
                    ),
                });
            }
        } else if neighbor != UNUSED_NEIGHBOR_ID {
            return Err(GraphNodeError::InvalidNode {
                node_id,
                detail: format!("unused neighbour slot {slot} holds {neighbor}, expected u32::MAX"),
            });
        }
    }
    Ok(())
}

fn decode_block_view<'a>(
    layout: GraphNodeLayout,
    node_id: u32,
    block: &'a [u8],
) -> Result<GraphNodeBlock<'a>, GraphNodeError> {
    if block.len() != layout.stride as usize {
        return Err(GraphNodeError::InvalidNode {
            node_id,
            detail: format!("block bytes {}, expected {}", block.len(), layout.stride),
        });
    }
    let code_end = layout.code_bytes();
    let codes = block.get(..code_end).ok_or(GraphNodeError::Truncated {
        minimum: code_end,
        actual: block.len(),
    })?;
    let scale = read_f32(block, code_end, "scale")?;
    let norm = read_f32(block, code_end.saturating_add(4), "normalized norm")?;
    let correction = read_f32(block, code_end.saturating_add(8), "normalized correction")?;
    if [scale, norm, correction]
        .iter()
        .any(|value| !value.is_finite())
    {
        return Err(GraphNodeError::InvalidNode {
            node_id,
            detail: "factor record contains a non-finite value".to_owned(),
        });
    }
    let degree_offset = code_end.saturating_add(12);
    let degree = *block.get(degree_offset).ok_or(GraphNodeError::Truncated {
        minimum: degree_offset.saturating_add(1),
        actual: block.len(),
    })?;
    let flags = *block
        .get(degree_offset.saturating_add(1))
        .ok_or(GraphNodeError::Truncated {
            minimum: degree_offset.saturating_add(2),
            actual: block.len(),
        })?;
    let reserved: [u8; 2] = block
        .get(degree_offset.saturating_add(2)..degree_offset.saturating_add(4))
        .ok_or(GraphNodeError::Truncated {
            minimum: degree_offset.saturating_add(4),
            actual: block.len(),
        })?
        .try_into()
        .map_err(|_| GraphNodeError::InvalidNode {
            node_id,
            detail: "reserved field width is invalid".to_owned(),
        })?;
    let neighbors_start = code_end.saturating_add(NODE_METADATA_BYTES);
    let neighbors_end = neighbors_start
        .checked_add(usize::from(layout.max_degree).saturating_mul(4))
        .ok_or(GraphNodeError::ArithmeticOverflow)?;
    let neighbors = block
        .get(neighbors_start..neighbors_end)
        .ok_or(GraphNodeError::Truncated {
            minimum: neighbors_end,
            actual: block.len(),
        })?;
    let padding = block
        .get(neighbors_end..)
        .ok_or(GraphNodeError::Truncated {
            minimum: neighbors_end,
            actual: block.len(),
        })?;
    Ok(GraphNodeBlock {
        codes,
        factors: Bit4Factors::from_persisted(scale, norm, correction),
        degree,
        flags,
        reserved,
        neighbors,
        padding,
    })
}

fn read_u16(bytes: &[u8], offset: usize, field: &str) -> Result<u16, GraphNodeError> {
    let end = offset
        .checked_add(2)
        .ok_or(GraphNodeError::ArithmeticOverflow)?;
    let raw: [u8; 2] = bytes
        .get(offset..end)
        .ok_or(GraphNodeError::Truncated {
            minimum: end,
            actual: bytes.len(),
        })?
        .try_into()
        .map_err(|_| GraphNodeError::InvalidHeader(format!("invalid {field} width")))?;
    Ok(u16::from_le_bytes(raw))
}

fn read_u32(bytes: &[u8], offset: usize, field: &str) -> Result<u32, GraphNodeError> {
    let end = offset
        .checked_add(4)
        .ok_or(GraphNodeError::ArithmeticOverflow)?;
    let raw: [u8; 4] = bytes
        .get(offset..end)
        .ok_or(GraphNodeError::Truncated {
            minimum: end,
            actual: bytes.len(),
        })?
        .try_into()
        .map_err(|_| GraphNodeError::InvalidHeader(format!("invalid {field} width")))?;
    Ok(u32::from_le_bytes(raw))
}

fn read_f32(bytes: &[u8], offset: usize, field: &str) -> Result<f32, GraphNodeError> {
    read_u32(bytes, offset, field).map(f32::from_bits)
}

fn read_u64(bytes: &[u8], offset: usize, field: &str) -> Result<u64, GraphNodeError> {
    let end = offset
        .checked_add(8)
        .ok_or(GraphNodeError::ArithmeticOverflow)?;
    let raw: [u8; 8] = bytes
        .get(offset..end)
        .ok_or(GraphNodeError::Truncated {
            minimum: end,
            actual: bytes.len(),
        })?
        .try_into()
        .map_err(|_| GraphNodeError::InvalidHeader(format!("invalid {field} width")))?;
    Ok(u64::from_le_bytes(raw))
}

fn encode_node(
    layout: GraphNodeLayout,
    node_id: u32,
    node: &GraphNodeBlockInput<'_>,
    node_count: u32,
    output: &mut Vec<u8>,
) -> Result<(), GraphNodeError> {
    if node.codes.len() != layout.code_bytes() {
        return Err(GraphNodeError::InvalidNode {
            node_id,
            detail: format!(
                "code bytes {}, expected {}",
                node.codes.len(),
                layout.code_bytes()
            ),
        });
    }
    validate_code_padding(layout, node_id, node.codes)?;
    if node
        .factors
        .persisted_fields()
        .iter()
        .any(|field| !field.is_finite())
    {
        return Err(GraphNodeError::InvalidNode {
            node_id,
            detail: "factor record contains a non-finite value".to_owned(),
        });
    }
    if node.flags & !ALLOWED_NODE_FLAGS != 0 {
        return Err(GraphNodeError::InvalidNode {
            node_id,
            detail: format!("reserved flag bits are non-zero: {:#04x}", node.flags),
        });
    }
    if node.neighbors.len() > usize::from(layout.max_degree) {
        return Err(GraphNodeError::InvalidNode {
            node_id,
            detail: format!(
                "degree {}, maximum {}",
                node.neighbors.len(),
                layout.max_degree
            ),
        });
    }
    if let Some(invalid) = node
        .neighbors
        .iter()
        .copied()
        .find(|neighbor| *neighbor >= node_count)
    {
        return Err(GraphNodeError::InvalidNode {
            node_id,
            detail: format!("neighbour id {invalid} is outside row count {node_count}"),
        });
    }

    let start = output.len();
    output.extend_from_slice(node.codes);
    for field in node.factors.persisted_fields() {
        output.extend_from_slice(&field.to_bits().to_le_bytes());
    }
    let degree = u8::try_from(node.neighbors.len()).map_err(|_| GraphNodeError::InvalidNode {
        node_id,
        detail: "degree exceeds u8".to_owned(),
    })?;
    output.push(degree);
    output.push(node.flags);
    output.extend_from_slice(&0_u16.to_le_bytes());
    for neighbor in node.neighbors {
        output.extend_from_slice(&neighbor.to_le_bytes());
    }
    for _ in node.neighbors.len()..usize::from(layout.max_degree) {
        output.extend_from_slice(&UNUSED_NEIGHBOR_ID.to_le_bytes());
    }
    let end = start
        .checked_add(layout.stride as usize)
        .ok_or(GraphNodeError::ArithmeticOverflow)?;
    output.resize(end, 0);
    Ok(())
}

fn validate_code_padding(
    layout: GraphNodeLayout,
    node_id: u32,
    codes: &[u8],
) -> Result<(), GraphNodeError> {
    let logical_bytes = (layout.dims as usize).div_ceil(2);
    if !layout.dims.is_multiple_of(2)
        && codes
            .get(logical_bytes.saturating_sub(1))
            .is_some_and(|byte| byte & 0x0f != 0)
    {
        return Err(GraphNodeError::InvalidNode {
            node_id,
            detail: "unused low nibble is non-zero".to_owned(),
        });
    }
    if codes
        .get(logical_bytes..)
        .is_some_and(|padding| padding.iter().any(|byte| *byte != 0))
    {
        return Err(GraphNodeError::InvalidNode {
            node_id,
            detail: "padded Bit4 dimensions are non-zero".to_owned(),
        });
    }
    Ok(())
}

fn encode_trailer(layout: GraphNodeLayout, node_count: u32, output: &mut Vec<u8>) {
    let start = output.len();
    output.extend_from_slice(&NODE_BLOCK_MAGIC);
    output.extend_from_slice(&NODE_BLOCK_VERSION.to_le_bytes());
    output.extend_from_slice(&0_u16.to_le_bytes());
    output.extend_from_slice(&layout.dims.to_le_bytes());
    output.extend_from_slice(&layout.padded_dims.to_le_bytes());
    output.push(layout.max_degree);
    output.extend_from_slice(&[0_u8; 3]);
    output.extend_from_slice(&layout.stride.to_le_bytes());
    output.extend_from_slice(&node_count.to_le_bytes());
    let checksum = xxh3_64(output);
    output.extend_from_slice(&checksum.to_le_bytes());
    output.resize(start.saturating_add(NODE_BLOCK_TRAILER_LEN), 0);
}

fn round_up_cache_line(value: usize) -> Result<usize, GraphNodeError> {
    value
        .checked_add(CACHE_LINE_BYTES.saturating_sub(1))
        .map(|rounded| rounded / CACHE_LINE_BYTES * CACHE_LINE_BYTES)
        .ok_or(GraphNodeError::ArithmeticOverflow)
}
