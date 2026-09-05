//! The persisted posting format: bit-packed FOR blocks with positions.
//!
//! # Geometry, and why the block size is a header field
//!
//! Blocks hold
//! [`DEFAULT_POSTINGS_PER_BLOCK`](crate::fts::postings::DEFAULT_POSTINGS_PER_BLOCK)
//! postings. The research
//! disagrees with itself usefully here: ~40 postings per block prunes best
//! on GOV2 (3.6 ms against 4.2 ms at 128 — `research/02a:283`), but 128 is
//! what makes SIMD decode clean in Lucene and tantivy. Both numbers are
//! x86, on a 25M-document collection; neither transfers to a machine whose
//! cache line is 128 bytes (`tasks/evidence/19-M0-platform-premises.md:14`).
//!
//! 64 is the judgement call: 64 sixteen-bit deltas is exactly one 128-byte
//! line, and the block metadata row below is exactly 32 bytes, so four rows
//! also fill one line and the array indexes by shift rather than by
//! multiply. It is a cache-line argument, not a measurement, and it is
//! **not** a size-saving choice.
//!
//! The block size is written into the region header, so re-tuning it later
//! (task 27-B4 sweeps {32, 40, 64, 128}) changes a constant, never the
//! format. Do not "fix" a golden by editing the constant.
//!
//! # Streams
//!
//! Structure-of-arrays, all little-endian, all hand-written:
//!
//! - **docid deltas**, frame-of-reference bit-packed per block;
//! - **term frequencies**, bit-packed per block;
//! - **positions**, delta-packed per posting, `tf` of them per posting —
//!   the term frequency *is* the position count, so no separate count
//!   stream exists.
//!
//! Positions are in the format from day one because retrofitting them is a
//! format break (`research/03:484`). Task 15 reads them; task 13 only
//! stores them.
//!
//! # Block maxima
//!
//! Each block carries a `u8` quantized ceiling on the BM25 contribution of
//! any posting inside it. Task 14 reads it; task 13 writes it. Quantization
//! **rounds up**, so a stored maximum is never below a true score — a bound
//! that is too low would let pruning drop a document that belonged in the
//! top-k, and that is a wrong answer, not a slow one.
//!
//! `u8` is enough: 8-bit uniform impact quantization is statistically
//! indistinguishable from exact weights (Lin & Trotman, IRJ 2017). It is
//! kept for metadata density per cache line, not to save bytes, and the
//! same slot is deliberately shaped so a future caller-supplied impact
//! could reuse it.

use super::bm25::{DocLen, Tf};

/// Postings per block. Persisted; see the module docs before changing it.
pub const DEFAULT_POSTINGS_PER_BLOCK: u16 = 64;

/// Bytes in one block-metadata row. Four rows fill a 128-byte cache line.
pub const BLOCK_META_LEN: usize = 32;

/// Magic prefixing an encoded posting list.
const POSTINGS_MAGIC: [u8; 4] = *b"ZPST";

/// The original encoded-postings format version.
///
/// Its six reserved metadata bytes are zero. It remains readable, and the
/// frozen golden is a v1 stream, but the query path refuses it: a v1 block
/// carries no impact pair, and a silent exhaustive fallback would make a
/// stale index look correct and slow forever.
pub const POSTINGS_VERSION_V1: u16 = 1;

/// The current encoded-postings format version.
///
/// Version 2 spends the six reserved metadata bytes on the impact pair
/// `(max_tf: u32, min_len: u16)`. See [`BlockImpact`] for why that is a
/// soundness fix and not merely a wider bound.
pub const POSTINGS_VERSION: u16 = 2;

/// A rejected or malformed posting stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PostingsError {
    /// The byte stream ended inside a structure.
    Truncated {
        /// Bytes the decoder needed.
        needed: usize,
        /// Bytes actually available.
        available: usize,
    },
    /// The leading magic did not match.
    BadMagic,
    /// The declared version is not readable.
    UnsupportedVersion {
        /// Version found in the stream.
        found: u16,
    },
    /// A declared bit width exceeded 32.
    BitWidthTooLarge {
        /// The rejected width.
        bits: u8,
    },
    /// The block size in the header was zero.
    ZeroBlockSize,
    /// Document identifiers were not strictly ascending.
    DocidsNotAscending {
        /// The offending identifier.
        docid: u32,
    },
    /// Positions within one posting were not strictly ascending.
    PositionsNotAscending {
        /// The offending position.
        position: u32,
    },
    /// A term frequency was zero, which cannot occur in a posting list.
    ZeroTermFrequency,
    /// The stream declared more postings than its blocks can hold.
    InconsistentPostingCount {
        /// Postings declared in the header.
        declared: u32,
        /// Postings the blocks actually describe.
        described: u32,
    },
}

impl std::fmt::Display for PostingsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated { needed, available } => write!(
                formatter,
                "posting stream truncated: needed {needed} bytes, had {available}"
            ),
            Self::BadMagic => formatter.write_str("posting stream magic did not match"),
            Self::UnsupportedVersion { found } => {
                write!(formatter, "posting stream version {found} is not readable")
            }
            Self::BitWidthTooLarge { bits } => {
                write!(formatter, "declared bit width {bits} exceeds 32")
            }
            Self::ZeroBlockSize => formatter.write_str("posting block size was zero"),
            Self::DocidsNotAscending { docid } => {
                write!(formatter, "document ids are not ascending at {docid}")
            }
            Self::PositionsNotAscending { position } => {
                write!(formatter, "positions are not ascending at {position}")
            }
            Self::ZeroTermFrequency => formatter.write_str("a posting had a zero term frequency"),
            Self::InconsistentPostingCount {
                declared,
                described,
            } => write!(
                formatter,
                "header declares {declared} postings, blocks describe {described}"
            ),
        }
    }
}

impl std::error::Error for PostingsError {}

/// One document's entry in a posting list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Posting {
    /// Segment-local dense row id.
    pub docid: u32,
    /// Occurrences of the term in the document. Always at least one.
    pub tf: u32,
    /// Token positions, strictly ascending. Length equals `tf`.
    pub positions: Vec<u32>,
}

/// An in-memory posting list for one term.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PostingList {
    postings: Vec<Posting>,
}

impl PostingList {
    /// Creates an empty list.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            postings: Vec::new(),
        }
    }

    /// Returns the postings, ascending by document id.
    #[must_use]
    pub fn postings(&self) -> &[Posting] {
        &self.postings
    }

    pub(crate) fn resident_bytes(&self) -> usize {
        self.postings
            .capacity()
            .saturating_mul(std::mem::size_of::<Posting>())
            .saturating_add(
                self.postings
                    .iter()
                    .map(|posting| posting.positions.capacity().saturating_mul(4))
                    .fold(0_usize, usize::saturating_add),
            )
    }

    /// Returns the number of documents containing the term.
    #[must_use]
    pub fn document_frequency(&self) -> u32 {
        u32::try_from(self.postings.len()).unwrap_or(u32::MAX)
    }

    /// Returns true when no document contains the term.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.postings.is_empty()
    }

    /// Appends one posting.
    ///
    /// # Errors
    ///
    /// Returns [`PostingsError`] when document ids are not strictly
    /// ascending, when positions are not strictly ascending, or when the
    /// term frequency is zero. These are contract violations by the caller,
    /// so they fail loudly rather than being repaired.
    pub fn push(&mut self, posting: Posting) -> Result<(), PostingsError> {
        if posting.tf == 0 || posting.positions.is_empty() {
            return Err(PostingsError::ZeroTermFrequency);
        }
        if let Some(last) = self.postings.last()
            && posting.docid <= last.docid
        {
            return Err(PostingsError::DocidsNotAscending {
                docid: posting.docid,
            });
        }
        let mut previous: Option<u32> = None;
        for position in &posting.positions {
            if let Some(earlier) = previous
                && *position <= earlier
            {
                return Err(PostingsError::PositionsNotAscending {
                    position: *position,
                });
            }
            previous = Some(*position);
        }
        self.postings.push(posting);
        Ok(())
    }
}

/// Bits needed to represent the largest value in `values`.
fn required_bits_controlled<E>(
    values: &[u32],
    work: &mut super::control::WorkCheck<impl FnMut() -> Result<(), E>>,
) -> Result<u8, E> {
    let mut maximum = 0;
    for value in values {
        work.step()?;
        #[cfg(test)]
        ENCODE_BITS_PROBES.with(|value| value.set(value.get() + 1));
        maximum = maximum.max(*value);
    }
    Ok(if maximum == 0 {
        0
    } else {
        // 32 - leading_zeros is in 1..=32 for a non-zero value.
        u8::try_from(32 - maximum.leading_zeros()).unwrap_or(32)
    })
}

/// Appends `values` to `output`, packed at `bits` each.
fn pack_bits_controlled<E>(
    values: &[u32],
    bits: u8,
    output: &mut Vec<u8>,
    work: &mut super::control::WorkCheck<impl FnMut() -> Result<(), E>>,
) -> Result<(), E> {
    if bits == 0 {
        return Ok(());
    }
    let width = u32::from(bits);
    let mut accumulator: u64 = 0;
    let mut filled: u32 = 0;
    for value in values {
        work.step()?;
        #[cfg(test)]
        ENCODE_PACK_PROBES.with(|value| value.set(value.get() + 1));
        accumulator |= u64::from(*value) << filled;
        filled += width;
        while filled >= 8 {
            output.push((accumulator & 0xFF) as u8);
            accumulator >>= 8;
            filled -= 8;
        }
    }
    if filled > 0 {
        output.push((accumulator & 0xFF) as u8);
    }
    Ok(())
}

#[cfg(test)]
fn pack_bits(values: &[u32], bits: u8, output: &mut Vec<u8>) {
    let mut work = super::control::WorkCheck::new(|| Ok::<(), std::convert::Infallible>(()));
    match pack_bits_controlled(values, bits, output, &mut work) {
        Ok(()) => (),
        Err(never) => match never {},
    }
}

/// Bytes a packed run of `count` values at `bits` each occupies.
const fn packed_len(count: usize, bits: u8) -> usize {
    if bits == 0 {
        return 0;
    }
    (count * bits as usize).div_ceil(8)
}

/// Unpacks `count` values at `bits` each from the front of `input`.
///
/// Returns the number of bytes consumed.
///
/// # Errors
///
/// Returns [`PostingsError::BitWidthTooLarge`] for a width above 32 and
/// [`PostingsError::Truncated`] when `input` is too short. It never panics
/// and never reads past `input`.
pub fn unpack_bits(
    input: &[u8],
    bits: u8,
    count: usize,
    output: &mut [u32],
) -> Result<usize, PostingsError> {
    if bits > 32 {
        return Err(PostingsError::BitWidthTooLarge { bits });
    }
    if bits == 0 {
        for slot in output.iter_mut().take(count) {
            *slot = 0;
        }
        return Ok(0);
    }
    let needed = packed_len(count, bits);
    if input.len() < needed {
        return Err(PostingsError::Truncated {
            needed,
            available: input.len(),
        });
    }
    let width = u32::from(bits);
    let mask = if bits == 32 {
        u32::MAX
    } else {
        (1_u32 << width) - 1
    };
    let mut accumulator: u64 = 0;
    let mut filled: u32 = 0;
    let mut cursor = 0_usize;
    for slot in output.iter_mut().take(count) {
        while filled < width {
            let byte = input.get(cursor).copied().unwrap_or(0);
            cursor += 1;
            accumulator |= u64::from(byte) << filled;
            filled += 8;
        }
        *slot = (accumulator as u32) & mask;
        accumulator >>= width;
        filled -= width;
    }
    Ok(needed)
}

/// One block's metadata row, exactly [`BLOCK_META_LEN`] bytes on disk.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BlockMeta {
    /// Largest document id in the block; the skip key.
    pub last_docid: u32,
    /// Byte offset of the block's packed docid deltas.
    pub docids_offset: u32,
    /// Byte offset of the block's packed term frequencies.
    pub tfs_offset: u32,
    /// Byte offset of the block's packed positions.
    pub positions_offset: u32,
    /// Packed positions in the block.
    pub positions_count: u32,
    /// Postings in the block; the final block may be partial.
    pub count: u16,
    /// Bit width of the docid deltas.
    pub docid_bits: u8,
    /// Bit width of the term frequencies.
    pub tf_bits: u8,
    /// Bit width of the position deltas.
    pub position_bits: u8,
    /// Reserved for a caller-supplied impact. Written zero by this engine.
    ///
    /// Version 1 stored `quantize(score / ceiling)` here and computed the
    /// pruning bound from it. That was unsound: the quotient bakes in
    /// seal-time `avgdl` and seal-time `(k1, b)`, neither of which cancels,
    /// so the stored bound fell below a true score once statistics drifted.
    /// The slot is kept — its shape suits a caller-supplied learned impact —
    /// but nothing in the engine reads it for a bound.
    pub block_max: u8,
    /// Largest term frequency in the block. Zero in a version 1 stream.
    pub max_tf: u32,
    /// Smallest document length in the block, saturating DOWN to `u16::MAX`.
    ///
    /// Down, because a shorter document scores higher: saturating upward
    /// would put the stored bound below a true score.
    pub min_len: u16,
}

impl BlockMeta {
    fn write(&self, output: &mut Vec<u8>) {
        output.extend_from_slice(&self.last_docid.to_le_bytes());
        output.extend_from_slice(&self.docids_offset.to_le_bytes());
        output.extend_from_slice(&self.tfs_offset.to_le_bytes());
        output.extend_from_slice(&self.positions_offset.to_le_bytes());
        output.extend_from_slice(&self.positions_count.to_le_bytes());
        output.extend_from_slice(&self.count.to_le_bytes());
        output.push(self.docid_bits);
        output.push(self.tf_bits);
        output.push(self.position_bits);
        output.push(self.block_max);
        // 4 + 4 + 4 + 4 + 4 + 2 + 4 written above is 26 bytes; the impact
        // pair spends the remaining six exactly. A version 1 encode leaves
        // both fields zero, which reproduces the original zero padding byte
        // for byte, so the v1 golden is unmoved.
        output.extend_from_slice(&self.max_tf.to_le_bytes());
        output.extend_from_slice(&self.min_len.to_le_bytes());
    }

    /// Reads one metadata row from the front of `input`.
    ///
    /// # Errors
    ///
    /// Returns [`PostingsError::Truncated`] when fewer than
    /// [`BLOCK_META_LEN`] bytes are available.
    pub fn read(input: &[u8]) -> Result<Self, PostingsError> {
        let row = input
            .get(..BLOCK_META_LEN)
            .ok_or(PostingsError::Truncated {
                needed: BLOCK_META_LEN,
                available: input.len(),
            })?;
        Ok(Self {
            last_docid: read_u32(row, 0)?,
            docids_offset: read_u32(row, 4)?,
            tfs_offset: read_u32(row, 8)?,
            positions_offset: read_u32(row, 12)?,
            positions_count: read_u32(row, 16)?,
            count: read_u16(row, 20)?,
            docid_bits: read_u8(row, 22)?,
            tf_bits: read_u8(row, 23)?,
            position_bits: read_u8(row, 24)?,
            block_max: read_u8(row, 25)?,
            max_tf: read_u32(row, 26)?,
            min_len: read_u16(row, 30)?,
        })
    }
}

fn read_u8(input: &[u8], at: usize) -> Result<u8, PostingsError> {
    input.get(at).copied().ok_or(PostingsError::Truncated {
        needed: at + 1,
        available: input.len(),
    })
}

fn read_u16(input: &[u8], at: usize) -> Result<u16, PostingsError> {
    let bytes = input.get(at..at + 2).ok_or(PostingsError::Truncated {
        needed: at + 2,
        available: input.len(),
    })?;
    let mut buffer = [0_u8; 2];
    buffer.copy_from_slice(bytes);
    Ok(u16::from_le_bytes(buffer))
}

fn read_u32(input: &[u8], at: usize) -> Result<u32, PostingsError> {
    let bytes = input.get(at..at + 4).ok_or(PostingsError::Truncated {
        needed: at + 4,
        available: input.len(),
    })?;
    let mut buffer = [0_u8; 4];
    buffer.copy_from_slice(bytes);
    Ok(u32::from_le_bytes(buffer))
}

/// An encoded posting list: header, block metadata rows, then the streams.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EncodedPostings {
    bytes: Vec<u8>,
}

impl EncodedPostings {
    /// Returns the encoded bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Wraps already-encoded bytes.
    #[must_use]
    pub const fn from_bytes(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }
}

/// Header length: magic, version, block size, posting and block counts.
pub const HEADER_LEN: usize = 16;

/// Encodes one posting list in the original version 1 layout.
///
/// `block_maxima` supplies the per-block reserved `u8` slot; pass an empty
/// slice to store zeros. The impact-pair bytes stay zero, which is what
/// keeps the frozen v1 golden byte-identical.
///
/// New writers use [`encode_v2`]. This entry point exists so the v1 golden
/// stays exercised and so a v1 stream can still be produced deliberately.
///
/// # Errors
///
/// Returns [`PostingsError::ZeroBlockSize`] when the geometry is degenerate.
pub fn encode(
    list: &PostingList,
    postings_per_block: u16,
    block_maxima: &[u8],
) -> Result<EncodedPostings, PostingsError> {
    encode_versioned(
        list,
        postings_per_block,
        block_maxima,
        &[],
        POSTINGS_VERSION_V1,
        &mut super::control::WorkCheck::new(|| Ok::<(), PostingsError>(())),
    )
}

/// Encodes one posting list in the version 2 layout, carrying impact pairs.
///
/// `impacts` supplies one [`BlockImpact`] per block, normally from
/// [`block_impacts`]. A missing entry stores a zero pair, which a reader
/// rejects rather than treating as an unbounded block.
///
/// # Errors
///
/// Returns [`PostingsError::ZeroBlockSize`] when the geometry is degenerate.
pub fn encode_v2(
    list: &PostingList,
    postings_per_block: u16,
    block_maxima: &[u8],
    impacts: &[BlockImpact],
) -> Result<EncodedPostings, PostingsError> {
    encode_versioned(
        list,
        postings_per_block,
        block_maxima,
        impacts,
        POSTINGS_VERSION,
        &mut super::control::WorkCheck::new(|| Ok::<(), PostingsError>(())),
    )
}

pub(crate) fn encode_v2_controlled<E: From<PostingsError>>(
    list: &PostingList,
    postings_per_block: u16,
    block_maxima: &[u8],
    impacts: &[BlockImpact],
    work: &mut super::control::WorkCheck<impl FnMut() -> Result<(), E>>,
) -> Result<EncodedPostings, E> {
    encode_versioned(
        list,
        postings_per_block,
        block_maxima,
        impacts,
        POSTINGS_VERSION,
        work,
    )
}

/// The one encoder. Version selects only what the reserved bytes carry.
fn encode_versioned<E: From<PostingsError>>(
    list: &PostingList,
    postings_per_block: u16,
    block_maxima: &[u8],
    impacts: &[BlockImpact],
    version: u16,
    work: &mut super::control::WorkCheck<impl FnMut() -> Result<(), E>>,
) -> Result<EncodedPostings, E> {
    work.check_now()?;
    if postings_per_block == 0 {
        return Err(PostingsError::ZeroBlockSize.into());
    }
    let per_block = usize::from(postings_per_block);
    let postings = list.postings();
    let block_count = postings.len().div_ceil(per_block);

    let mut docid_stream: Vec<u8> = Vec::new();
    let mut tf_stream: Vec<u8> = Vec::new();
    let mut position_stream: Vec<u8> = Vec::new();
    let mut metadata: Vec<BlockMeta> = Vec::with_capacity(block_count);

    let mut previous_last_docid = 0_u32;
    for (index, chunk) in postings.chunks(per_block).enumerate() {
        work.step()?;
        let mut deltas: Vec<u32> = Vec::with_capacity(chunk.len());
        let mut frequencies: Vec<u32> = Vec::with_capacity(chunk.len());
        let mut position_deltas: Vec<u32> = Vec::new();

        let mut base = previous_last_docid;
        for (offset, posting) in chunk.iter().enumerate() {
            work.step()?;
            // The very first posting of the list is stored absolutely; every
            // other delta is a gap from the previous document id.
            let delta = if index == 0 && offset == 0 {
                posting.docid
            } else {
                posting.docid.saturating_sub(base)
            };
            deltas.push(delta);
            frequencies.push(posting.tf);
            let mut previous_position = 0_u32;
            for (slot, position) in posting.positions.iter().enumerate() {
                work.step()?;
                #[cfg(test)]
                ENCODE_POSITION_PROBES.with(|value| value.set(value.get() + 1));
                let gap = if slot == 0 {
                    *position
                } else {
                    position.saturating_sub(previous_position)
                };
                position_deltas.push(gap);
                previous_position = *position;
            }
            base = posting.docid;
        }
        previous_last_docid = base;

        let docid_bits = required_bits_controlled(&deltas, work)?;
        let tf_bits = required_bits_controlled(&frequencies, work)?;
        let position_bits = required_bits_controlled(&position_deltas, work)?;

        let meta = BlockMeta {
            last_docid: base,
            docids_offset: u32::try_from(docid_stream.len()).unwrap_or(u32::MAX),
            tfs_offset: u32::try_from(tf_stream.len()).unwrap_or(u32::MAX),
            positions_offset: u32::try_from(position_stream.len()).unwrap_or(u32::MAX),
            positions_count: u32::try_from(position_deltas.len()).unwrap_or(u32::MAX),
            count: u16::try_from(chunk.len()).unwrap_or(u16::MAX),
            docid_bits,
            tf_bits,
            position_bits,
            block_max: block_maxima.get(index).copied().unwrap_or(0),
            max_tf: impacts.get(index).map_or(0, |impact| impact.max_tf),
            min_len: impacts.get(index).map_or(0, |impact| impact.min_len),
        };

        pack_bits_controlled(&deltas, docid_bits, &mut docid_stream, work)?;
        pack_bits_controlled(&frequencies, tf_bits, &mut tf_stream, work)?;
        pack_bits_controlled(&position_deltas, position_bits, &mut position_stream, work)?;
        metadata.push(meta);
    }

    let mut bytes = Vec::with_capacity(
        HEADER_LEN
            + metadata.len() * BLOCK_META_LEN
            + docid_stream.len()
            + tf_stream.len()
            + position_stream.len(),
    );
    bytes.extend_from_slice(&POSTINGS_MAGIC);
    bytes.extend_from_slice(&version.to_le_bytes());
    bytes.extend_from_slice(&postings_per_block.to_le_bytes());
    bytes.extend_from_slice(
        &u32::try_from(postings.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    bytes.extend_from_slice(
        &u32::try_from(metadata.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    for meta in &metadata {
        work.step()?;
        meta.write(&mut bytes);
    }
    super::control::extend_bytes(&mut bytes, &docid_stream, work)?;
    super::control::extend_bytes(&mut bytes, &tf_stream, work)?;
    super::control::extend_bytes(&mut bytes, &position_stream, work)?;

    work.check_now()?;
    Ok(EncodedPostings { bytes })
}

/// A validated view over an encoded posting list.
#[derive(Clone, Debug)]
pub struct PostingsReader<'bytes> {
    version: u16,
    blocks: Vec<BlockMeta>,
    docids: &'bytes [u8],
    tfs: &'bytes [u8],
    positions: &'bytes [u8],
    posting_count: u32,
    postings_per_block: u16,
}

#[cfg(test)]
std::thread_local! {
    static ENCODE_POSITION_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ENCODE_BITS_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ENCODE_PACK_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ENCODE_IMPACT_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static POSITION_OPEN_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static POSITION_MEMBER_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static POSITION_DECODE_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl<'bytes> PostingsReader<'bytes> {
    /// Locates only one row's position deltas. Other rows' positions are not
    /// decoded; their frequencies establish the offset within this block.
    ///
    /// # Errors
    /// Returns a typed posting error for malformed frequencies or geometry.
    pub fn positions(&self, row: u32) -> Result<Option<RowPositions<'bytes>>, PostingsError> {
        self.positions_controlled(
            row,
            &mut super::control::WorkCheck::new(|| Ok::<(), PostingsError>(())),
        )
    }

    pub(crate) fn positions_controlled<E: From<PostingsError>>(
        &self,
        row: u32,
        work: &mut super::control::WorkCheck<impl FnMut() -> Result<(), E>>,
    ) -> Result<Option<RowPositions<'bytes>>, E> {
        work.check_now()?;
        let block = self.blocks.partition_point(|meta| meta.last_docid < row);
        let Some(meta) = self.blocks.get(block) else {
            return Ok(None);
        };
        let docids =
            self.docids
                .get(meta.docids_offset as usize..)
                .ok_or(PostingsError::Truncated {
                    needed: meta.docids_offset as usize,
                    available: self.docids.len(),
                })?;
        let tfs = self
            .tfs
            .get(meta.tfs_offset as usize..)
            .ok_or(PostingsError::Truncated {
                needed: meta.tfs_offset as usize,
                available: self.tfs.len(),
            })?;
        let mut docid = block
            .checked_sub(1)
            .and_then(|previous| self.blocks.get(previous))
            .map_or(0, |meta| meta.last_docid);
        let mut first = 0_usize;
        for slot in 0..usize::from(meta.count) {
            work.step()?;
            #[cfg(test)]
            POSITION_MEMBER_PROBES.with(|value| value.set(value.get() + 1));
            let delta = packed_value_at(docids, meta.docid_bits, slot)?;
            docid = docid
                .checked_add(delta)
                .ok_or(PostingsError::DocidsNotAscending { docid })?;
            if docid > row {
                return Ok(None);
            }
            let count = packed_value_at(tfs, meta.tf_bits, slot)? as usize;
            if count == 0 {
                return Err(PostingsError::ZeroTermFrequency.into());
            }
            let end = first.checked_add(count).ok_or(PostingsError::Truncated {
                needed: usize::MAX,
                available: meta.positions_count as usize,
            })?;
            if end > meta.positions_count as usize {
                return Err(PostingsError::Truncated {
                    needed: end,
                    available: meta.positions_count as usize,
                }
                .into());
            }
            if docid == row {
                let start = meta.positions_offset as usize;
                let end = start
                    .checked_add(packed_len(
                        meta.positions_count as usize,
                        meta.position_bits,
                    ))
                    .ok_or(PostingsError::Truncated {
                        needed: usize::MAX,
                        available: self.positions.len(),
                    })?;
                let bytes = self
                    .positions
                    .get(start..end)
                    .ok_or(PostingsError::Truncated {
                        needed: end,
                        available: self.positions.len(),
                    })?;
                return Ok(Some(RowPositions {
                    bytes,
                    bits: meta.position_bits,
                    first,
                    count,
                }));
            }
            first = end;
        }
        Ok(None)
    }

    /// Validates and opens an encoded posting list.
    ///
    /// Every structural claim the bytes make is checked here, so the
    /// iteration methods below cannot fail on a reader that exists. Corrupt
    /// bytes produce a typed error and never a panic.
    ///
    /// # Errors
    ///
    /// Returns [`PostingsError`] for bad magic, an unreadable version, a
    /// zero block size, an oversized bit width, an inconsistent posting
    /// count, or truncation anywhere.
    pub fn open(bytes: &'bytes [u8]) -> Result<Self, PostingsError> {
        Self::open_controlled(
            bytes,
            &mut super::control::WorkCheck::new(|| Ok::<(), PostingsError>(())),
        )
    }

    pub(crate) fn open_controlled<E: From<PostingsError>>(
        bytes: &'bytes [u8],
        work: &mut super::control::WorkCheck<impl FnMut() -> Result<(), E>>,
    ) -> Result<Self, E> {
        work.check_now()?;
        let header = bytes.get(..HEADER_LEN).ok_or(PostingsError::Truncated {
            needed: HEADER_LEN,
            available: bytes.len(),
        })?;
        if header.get(..4) != Some(&POSTINGS_MAGIC[..]) {
            return Err(PostingsError::BadMagic.into());
        }
        let version = read_u16(header, 4)?;
        if version != POSTINGS_VERSION_V1 && version != POSTINGS_VERSION {
            return Err(PostingsError::UnsupportedVersion { found: version }.into());
        }
        let postings_per_block = read_u16(header, 6)?;
        if postings_per_block == 0 {
            return Err(PostingsError::ZeroBlockSize.into());
        }
        let posting_count = read_u32(header, 8)?;
        let block_count = read_u32(header, 12)?;

        let block_count_usize = usize::try_from(block_count).unwrap_or(usize::MAX);
        let metadata_len =
            block_count_usize
                .checked_mul(BLOCK_META_LEN)
                .ok_or(PostingsError::Truncated {
                    needed: usize::MAX,
                    available: bytes.len(),
                })?;
        let metadata_end =
            HEADER_LEN
                .checked_add(metadata_len)
                .ok_or(PostingsError::Truncated {
                    needed: usize::MAX,
                    available: bytes.len(),
                })?;
        let metadata_bytes =
            bytes
                .get(HEADER_LEN..metadata_end)
                .ok_or(PostingsError::Truncated {
                    needed: metadata_end,
                    available: bytes.len(),
                })?;

        let mut blocks = Vec::with_capacity(block_count_usize);
        let mut described = 0_u32;
        let mut docid_bytes = 0_usize;
        let mut tf_bytes = 0_usize;
        let mut position_bytes = 0_usize;
        for index in 0..block_count_usize {
            work.step()?;
            #[cfg(test)]
            POSITION_OPEN_PROBES.with(|value| value.set(value.get() + 1));
            let start = index.saturating_mul(BLOCK_META_LEN);
            let row = metadata_bytes.get(start..start + BLOCK_META_LEN).ok_or(
                PostingsError::Truncated {
                    needed: start + BLOCK_META_LEN,
                    available: metadata_bytes.len(),
                },
            )?;
            let meta = BlockMeta::read(row)?;
            for bits in [meta.docid_bits, meta.tf_bits, meta.position_bits] {
                if bits > 32 {
                    return Err(PostingsError::BitWidthTooLarge { bits }.into());
                }
            }
            if meta.count == 0 || meta.count > postings_per_block {
                return Err(PostingsError::InconsistentPostingCount {
                    declared: posting_count,
                    described: described.saturating_add(u32::from(meta.count)),
                }
                .into());
            }
            described = described.saturating_add(u32::from(meta.count));
            let count = usize::from(meta.count);
            docid_bytes = docid_bytes.max(
                usize::try_from(meta.docids_offset).unwrap_or(usize::MAX)
                    + packed_len(count, meta.docid_bits),
            );
            tf_bytes = tf_bytes.max(
                usize::try_from(meta.tfs_offset).unwrap_or(usize::MAX)
                    + packed_len(count, meta.tf_bits),
            );
            position_bytes = position_bytes.max(
                usize::try_from(meta.positions_offset).unwrap_or(usize::MAX)
                    + packed_len(
                        usize::try_from(meta.positions_count).unwrap_or(usize::MAX),
                        meta.position_bits,
                    ),
            );
            blocks.push(meta);
        }
        if described != posting_count {
            return Err(PostingsError::InconsistentPostingCount {
                declared: posting_count,
                described,
            }
            .into());
        }

        let streams = bytes.get(metadata_end..).ok_or(PostingsError::Truncated {
            needed: metadata_end,
            available: bytes.len(),
        })?;
        let tf_start = docid_bytes;
        let position_start = tf_start
            .checked_add(tf_bytes)
            .ok_or(PostingsError::Truncated {
                needed: usize::MAX,
                available: streams.len(),
            })?;
        let position_end =
            position_start
                .checked_add(position_bytes)
                .ok_or(PostingsError::Truncated {
                    needed: usize::MAX,
                    available: streams.len(),
                })?;
        if streams.len() < position_end {
            return Err(PostingsError::Truncated {
                needed: position_end,
                available: streams.len(),
            }
            .into());
        }
        let docids = streams.get(..tf_start).ok_or(PostingsError::Truncated {
            needed: tf_start,
            available: streams.len(),
        })?;
        let tfs = streams
            .get(tf_start..position_start)
            .ok_or(PostingsError::Truncated {
                needed: position_start,
                available: streams.len(),
            })?;
        let positions =
            streams
                .get(position_start..position_end)
                .ok_or(PostingsError::Truncated {
                    needed: position_end,
                    available: streams.len(),
                })?;

        work.check_now()?;
        Ok(Self {
            version,
            blocks,
            docids,
            tfs,
            positions,
            posting_count,
            postings_per_block,
        })
    }

    /// Returns the block metadata rows.
    #[must_use]
    pub fn blocks(&self) -> &[BlockMeta] {
        &self.blocks
    }

    /// Returns the format version the stream declared.
    ///
    /// A caller that needs impact pairs checks this rather than inferring
    /// their presence from a zero pair, so a v1 stream is refused loudly
    /// instead of silently bounding nothing.
    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }

    /// Returns the postings-per-block geometry recorded in the header.
    #[must_use]
    pub const fn postings_per_block(&self) -> u16 {
        self.postings_per_block
    }

    /// Returns the total posting count.
    #[must_use]
    pub const fn document_frequency(&self) -> u32 {
        self.posting_count
    }

    /// Decodes one block into document ids, frequencies, and positions.
    ///
    /// # Errors
    ///
    /// Returns [`PostingsError::Truncated`] when a stream is short. Bounds
    /// were validated at [`PostingsReader::open`], so a well-formed reader
    /// decodes every block successfully.
    pub fn decode_block(&self, index: usize) -> Result<Vec<Posting>, PostingsError> {
        let Some(meta) = self.blocks.get(index) else {
            return Ok(Vec::new());
        };
        let count = usize::from(meta.count);
        let mut deltas = vec![0_u32; count];
        let docid_start = usize::try_from(meta.docids_offset).unwrap_or(usize::MAX);
        let docid_slice = self
            .docids
            .get(docid_start..)
            .ok_or(PostingsError::Truncated {
                needed: docid_start,
                available: self.docids.len(),
            })?;
        unpack_bits(docid_slice, meta.docid_bits, count, &mut deltas)?;

        let mut frequencies = vec![0_u32; count];
        let tf_start = usize::try_from(meta.tfs_offset).unwrap_or(usize::MAX);
        let tf_slice = self.tfs.get(tf_start..).ok_or(PostingsError::Truncated {
            needed: tf_start,
            available: self.tfs.len(),
        })?;
        unpack_bits(tf_slice, meta.tf_bits, count, &mut frequencies)?;

        let position_count = usize::try_from(meta.positions_count).unwrap_or(usize::MAX);
        let mut position_deltas = vec![0_u32; position_count];
        let position_start = usize::try_from(meta.positions_offset).unwrap_or(usize::MAX);
        let position_slice =
            self.positions
                .get(position_start..)
                .ok_or(PostingsError::Truncated {
                    needed: position_start,
                    available: self.positions.len(),
                })?;
        unpack_bits(
            position_slice,
            meta.position_bits,
            position_count,
            &mut position_deltas,
        )?;

        // Reconstruct absolute ids by prefix sum over the block.
        let mut base = if index == 0 {
            0
        } else {
            self.blocks
                .get(index.saturating_sub(1))
                .map_or(0, |previous| previous.last_docid)
        };
        let mut postings = Vec::with_capacity(count);
        let mut position_cursor = 0_usize;
        for slot in 0..count {
            let delta = deltas.get(slot).copied().unwrap_or(0);
            let docid = if index == 0 && slot == 0 {
                delta
            } else {
                base.saturating_add(delta)
            };
            base = docid;
            let tf = frequencies.get(slot).copied().unwrap_or(0);
            if tf == 0 {
                return Err(PostingsError::ZeroTermFrequency);
            }
            let tf_usize = usize::try_from(tf).unwrap_or(usize::MAX);
            let mut positions = Vec::with_capacity(tf_usize.min(position_count));
            let mut running = 0_u32;
            for step in 0..tf_usize {
                let Some(gap) = position_deltas.get(position_cursor + step).copied() else {
                    return Err(PostingsError::Truncated {
                        needed: position_cursor + step + 1,
                        available: position_deltas.len(),
                    });
                };
                running = if step == 0 {
                    gap
                } else {
                    running.saturating_add(gap)
                };
                positions.push(running);
            }
            position_cursor = position_cursor.saturating_add(tf_usize);
            postings.push(Posting {
                docid,
                tf,
                positions,
            });
        }
        Ok(postings)
    }

    /// Decodes the whole list.
    ///
    /// # Errors
    ///
    /// Propagates any [`PostingsError`] from block decoding.
    pub fn decode_all(&self) -> Result<PostingList, PostingsError> {
        let mut list = PostingList::new();
        for index in 0..self.blocks.len() {
            for posting in self.decode_block(index)? {
                list.push(posting)?;
            }
        }
        Ok(list)
    }
}

/// Borrowed packed positions for one requested posting. Locating this view
/// does not decode position values or allocate a position array.
#[derive(Clone, Copy, Debug)]
pub struct RowPositions<'bytes> {
    bytes: &'bytes [u8],
    bits: u8,
    first: usize,
    count: usize,
}

impl RowPositions<'_> {
    /// Number of positions of the requested term in this row.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.count
    }

    /// Whether this view contains no positions.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Decodes only the requested row, validating strict position ordering.
    ///
    /// # Errors
    /// Returns a typed posting error for truncation, invalid widths or order.
    pub fn decode(&self) -> Result<Vec<u32>, PostingsError> {
        self.decode_controlled(&mut super::control::WorkCheck::new(|| {
            Ok::<(), PostingsError>(())
        }))
    }

    pub(crate) fn decode_controlled<E: From<PostingsError>>(
        &self,
        work: &mut super::control::WorkCheck<impl FnMut() -> Result<(), E>>,
    ) -> Result<Vec<u32>, E> {
        work.check_now()?;
        let mut positions = Vec::with_capacity(self.count);
        let mut position = 0_u32;
        let result = (|| {
            for offset in 0..self.count {
                work.step()?;
                #[cfg(test)]
                POSITION_DECODE_PROBES.with(|value| value.set(value.get() + 1));
                let delta = packed_value_at(self.bytes, self.bits, self.first + offset)?;
                if offset > 0 && delta == 0 {
                    return Err(PostingsError::PositionsNotAscending { position }.into());
                }
                position = position
                    .checked_add(delta)
                    .ok_or(PostingsError::PositionsNotAscending { position })?;
                positions.push(position);
            }
            work.check_now()
        })();
        // Record completed decode work even when the next cooperative check
        // aborts. Partial scratch is never returned to the caller.
        #[cfg(any(test, feature = "test-support"))]
        if !positions.is_empty() {
            let first_bit = self.first * usize::from(self.bits);
            let last_bit = (self.first + positions.len()) * usize::from(self.bits);
            super::preparation_observer::phrase_positions(
                positions.len(),
                last_bit.div_ceil(8) - first_bit / 8,
            );
        }
        result?;
        Ok(positions)
    }
}

/// A bit-addressed read can start mid-byte, including 32-bit values spanning
/// five bytes. It never expands preceding position values to find this one.
fn packed_value_at(bytes: &[u8], bits: u8, slot: usize) -> Result<u32, PostingsError> {
    if bits > 32 {
        return Err(PostingsError::BitWidthTooLarge { bits });
    }
    if bits == 0 {
        return Ok(0);
    }
    let first = slot
        .checked_mul(usize::from(bits))
        .ok_or(PostingsError::Truncated {
            needed: usize::MAX,
            available: bytes.len(),
        })?;
    let end = first
        .checked_add(usize::from(bits))
        .ok_or(PostingsError::Truncated {
            needed: usize::MAX,
            available: bytes.len(),
        })?
        .div_ceil(8);
    let selected = bytes.get(first / 8..end).ok_or(PostingsError::Truncated {
        needed: end,
        available: bytes.len(),
    })?;
    let word = selected
        .iter()
        .enumerate()
        .fold(0_u64, |word, (offset, byte)| {
            word | (u64::from(*byte) << (offset * 8))
        });
    let mask = (1_u64 << bits) - 1;
    Ok(((word >> (first % 8)) & mask) as u32)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod astra_18_tests {
    use super::*;
    use crate::fts::control::WorkCheck;
    use std::cell::Cell;

    #[derive(Debug, PartialEq)]
    enum TestError {
        Cancelled,
        Postings(PostingsError),
    }

    impl From<PostingsError> for TestError {
        fn from(error: PostingsError) -> Self {
            Self::Postings(error)
        }
    }

    fn one_position_per_row() -> PostingList {
        let mut list = PostingList::new();
        for row in 0..4_096 {
            list.push(Posting {
                docid: row,
                tf: 1,
                positions: vec![row * 2],
            })
            .expect("literal posting");
        }
        list
    }

    #[test]
    fn astra_18_position_metadata_cancels_inside_block_walk() {
        let list = one_position_per_row();
        let encoded = encode(&list, 1, &[]).expect("one row per block");
        POSITION_OPEN_PROBES.with(|value| value.set(0));
        let mut work = WorkCheck::new(|| {
            if POSITION_OPEN_PROBES.with(Cell::get) >= 64 {
                Err(TestError::Cancelled)
            } else {
                Ok(())
            }
        });
        let result = PostingsReader::open_controlled(encoded.as_bytes(), &mut work);
        let probes = POSITION_OPEN_PROBES.with(Cell::get);
        println!("metadata_probes={probes}");
        assert!(matches!(result, Err(TestError::Cancelled)));
        assert!(
            (64..=128).contains(&probes),
            "metadata walk must stop inside the list"
        );
        let mut clean = WorkCheck::new(|| Ok::<(), TestError>(()));
        let reader = PostingsReader::open_controlled(encoded.as_bytes(), &mut clean)
            .expect("fresh control opens complete metadata");
        assert_eq!(reader.blocks().len(), 4_096);
        assert_eq!(reader.document_frequency(), 4_096);
        assert_eq!(reader.decode_all().expect("complete literal output"), list);
        assert!(matches!(
            PostingsReader::open_controlled(&[], &mut clean),
            Err(TestError::Postings(PostingsError::Truncated { .. }))
        ));
    }

    #[test]
    fn astra_18_position_membership_cancels_inside_one_block() {
        let list = one_position_per_row();
        let encoded = encode(&list, 4_096, &[]).expect("one large legal block");
        let reader = PostingsReader::open(encoded.as_bytes()).expect("reader");
        POSITION_MEMBER_PROBES.with(|value| value.set(0));
        let mut work = WorkCheck::new(|| {
            if POSITION_MEMBER_PROBES.with(Cell::get) >= 64 {
                Err(TestError::Cancelled)
            } else {
                Ok(())
            }
        });
        let result = reader.positions_controlled(4_095, &mut work);
        let probes = POSITION_MEMBER_PROBES.with(Cell::get);
        println!("membership_probes={probes}");
        assert!(matches!(result, Err(TestError::Cancelled)));
        assert!(
            (64..=128).contains(&probes),
            "membership must stop inside the block"
        );
        let mut clean = WorkCheck::new(|| Ok::<(), TestError>(()));
        assert_eq!(
            reader
                .positions_controlled(4_095, &mut clean)
                .expect("fresh control")
                .expect("last row")
                .decode()
                .expect("position"),
            vec![8_190]
        );
        assert!(
            reader
                .positions_controlled(4_096, &mut clean)
                .expect("absent row")
                .is_none()
        );
    }

    #[test]
    fn astra_18_position_decode_cancels_inside_one_row() {
        let literal = (0..4_096).collect::<Vec<_>>();
        let mut list = PostingList::new();
        list.push(Posting {
            docid: 0,
            tf: 4_096,
            positions: literal.clone(),
        })
        .expect("one long row");
        let encoded = encode(&list, 64, &[]).expect("encoded positions");
        let reader = PostingsReader::open(encoded.as_bytes()).expect("reader");
        let view = reader.positions(0).expect("membership").expect("present");
        POSITION_DECODE_PROBES.with(|value| value.set(0));
        crate::fts::preparation_observer::begin();
        let mut work = WorkCheck::new(|| {
            if POSITION_DECODE_PROBES.with(Cell::get) >= 64 {
                Err(TestError::Cancelled)
            } else {
                Ok(())
            }
        });
        let result = view.decode_controlled(&mut work);
        let probes = POSITION_DECODE_PROBES.with(Cell::get);
        let observed = crate::fts::preparation_observer::phrase_position_work();
        crate::fts::preparation_observer::take();
        println!("decode_probes={probes}, position_work={observed:?}");
        assert_eq!(result, Err(TestError::Cancelled));
        assert!(
            (64..=128).contains(&probes),
            "decode must stop inside the row"
        );
        assert_eq!(
            observed,
            (probes, probes.div_ceil(8)),
            "retain actual partial decode work"
        );
        let mut clean = WorkCheck::new(|| Ok::<(), TestError>(()));
        assert_eq!(
            view.decode_controlled(&mut clean).expect("fresh control"),
            literal
        );
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod astra_11_tests {
    use super::*;

    #[test]
    fn astra_11_position_reader_only_decodes_requested_row() {
        for per_block in [1, 2, 64] {
            let mut list = PostingList::new();
            let expected = (0..130)
                .map(|row| {
                    if row % 3 == 0 {
                        vec![0, 1, 31, u32::MAX]
                    } else {
                        vec![row * 3]
                    }
                })
                .collect::<Vec<_>>();
            for (row, positions) in expected.iter().enumerate() {
                list.push(Posting {
                    docid: row as u32 * 2,
                    tf: positions.len() as u32,
                    positions: positions.clone(),
                })
                .expect("posting");
            }
            let encoded = encode(&list, per_block, &[]).expect("encoded positions");
            let reader = PostingsReader::open(encoded.as_bytes()).expect("persisted reader");
            for row in [0, 1, 63, 64, 65, 129] {
                crate::fts::preparation_observer::begin();
                let view = reader
                    .positions(row as u32 * 2)
                    .expect("locate")
                    .expect("present");
                assert_eq!(
                    crate::fts::preparation_observer::phrase_position_work(),
                    (0, 0)
                );
                assert_eq!(view.decode().expect("selective decode"), expected[row]);
                assert_eq!(
                    crate::fts::preparation_observer::phrase_position_work().0,
                    expected[row].len()
                );
                crate::fts::preparation_observer::take();
                assert!(
                    reader
                        .positions(row as u32 * 2 + 1)
                        .expect("absent")
                        .is_none()
                );
            }
        }
    }

    #[test]
    fn astra_11_corrupt_position_payload_is_typed() {
        let mut list = PostingList::new();
        for (docid, positions) in [(0, vec![1, 3]), (1, vec![2, 5])] {
            list.push(Posting {
                docid,
                tf: 2,
                positions,
            })
            .expect("posting");
        }
        let encoded = encode(&list, 64, &[]).expect("encoded");
        let clean = PostingsReader::open(encoded.as_bytes()).expect("open");
        assert_eq!(
            clean
                .positions(0)
                .expect("locate")
                .expect("present")
                .decode()
                .expect("clean"),
            vec![1, 3]
        );
        let positions_start = HEADER_LEN + BLOCK_META_LEN + clean.docids.len() + clean.tfs.len();
        assert_eq!(clean.blocks()[0].position_bits, 2);
        let mut corrupt = encoded.as_bytes().to_vec();
        // Delta two becomes zero in the actual packed payload, while all
        // structural lengths and row/frequency data remain intact.
        corrupt[positions_start] &= !(3 << 2);
        let reader = PostingsReader::open(&corrupt).expect("geometry unchanged");
        assert_eq!(
            reader
                .positions(0)
                .expect("locate corrupt row")
                .expect("present")
                .decode(),
            Err(PostingsError::PositionsNotAscending { position: 1 })
        );
        assert_eq!(
            reader
                .positions(1)
                .expect("next row")
                .expect("present")
                .decode()
                .expect("unaffected row"),
            vec![2, 5]
        );
    }
}

/// One block's impact pair: the two extremes that bound every posting in it.
///
/// # Why a pair and not a stored score
///
/// A stored score is a number computed under one set of statistics. BM25's
/// `score / ceiling` is `tf / (tf + k1 * (1 - b + b * len / avgdl))`, so
/// `idf` cancels but `avgdl` and `(k1, b)` do not: a bound sealed when
/// documents were short falls below a true score once `avgdl` rises, and a
/// bound sealed under `beir()` falls below one read under `anserini()`.
/// Measured shortfalls were 15.21% and 19.56%.
///
/// A pair carries no statistics at all. `term_score` is monotone increasing
/// in `tf` and monotone decreasing in `len`, so evaluating it at the block's
/// largest `tf` and shortest document dominates every posting in the block
/// under ANY statistics and ANY `(k1, b)`. The two extremes need not come
/// from the same document; pairing them is looser than the true maximum and
/// is therefore still an upper bound.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BlockImpact {
    /// Largest term frequency in the block.
    pub max_tf: u32,
    /// Smallest document length in the block, saturating DOWN.
    pub min_len: u16,
}

impl BlockImpact {
    /// Reads the pair out of a persisted metadata row.
    #[must_use]
    pub const fn from_meta(meta: &BlockMeta) -> Self {
        Self {
            max_tf: meta.max_tf,
            min_len: meta.min_len,
        }
    }

    /// Returns true when the pair bounds nothing.
    ///
    /// A real block has at least one posting, whose term frequency is at
    /// least one, so a zero `max_tf` can only mean the pair was never
    /// written — a version 1 stream, or a v2 stream missing an entry.
    #[must_use]
    pub const fn is_absent(&self) -> bool {
        self.max_tf == 0
    }

    /// Evaluates the bound under the caller's live scoring constants.
    ///
    /// This is the whole remedy: the bound is computed HERE, at query time,
    /// from statistics that are current and parameters the caller chose.
    #[must_use]
    pub fn bound(&self, scorer: &super::bm25::TermScorer) -> f64 {
        scorer.score(Tf(self.max_tf), DocLen(u32::from(self.min_len)))
    }
}

/// Computes the per-block impact pairs for one term's postings.
///
/// `lengths` is the dense per-row length array; a row beyond it contributes
/// the shortest possible length, which keeps the bound above rather than
/// below the truth.
#[must_use]
pub fn block_impacts(
    list: &PostingList,
    postings_per_block: u16,
    lengths: &[u32],
) -> Vec<BlockImpact> {
    let mut work = super::control::WorkCheck::new(|| Ok::<(), std::convert::Infallible>(()));
    match block_impacts_controlled(list, postings_per_block, lengths, &mut work) {
        Ok(impacts) => impacts,
        Err(never) => match never {},
    }
}

pub(crate) fn block_impacts_controlled<E>(
    list: &PostingList,
    postings_per_block: u16,
    lengths: &[u32],
    work: &mut super::control::WorkCheck<impl FnMut() -> Result<(), E>>,
) -> Result<Vec<BlockImpact>, E> {
    work.check_now()?;
    if postings_per_block == 0 {
        return Ok(Vec::new());
    }
    let chunks = list.postings().chunks(usize::from(postings_per_block));
    let mut impacts = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let mut max_tf = 0_u32;
        let mut min_len = u32::MAX;
        for posting in chunk {
            work.step()?;
            #[cfg(test)]
            ENCODE_IMPACT_PROBES.with(|value| value.set(value.get() + 1));
            max_tf = max_tf.max(posting.tf);
            let length = usize::try_from(posting.docid)
                .ok()
                .and_then(|slot| lengths.get(slot).copied())
                .unwrap_or(1);
            min_len = min_len.min(length);
        }
        impacts.push(BlockImpact {
            max_tf,
            // Saturate DOWN: a shorter document scores higher, keeping
            // this bound at or above the true score.
            min_len: u16::try_from(min_len).unwrap_or(u16::MAX),
        });
    }
    work.check_now()?;
    Ok(impacts)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use super::*;

    #[test]
    fn astra_18_postings_encoding_cancels_inside_positions() {
        let positions = (0..4_096).collect::<Vec<_>>();
        let list = list_from(&[(0, &positions)]);
        let impacts = block_impacts(&list, 64, &[4_096]);
        let reference = encode_v2(&list, 64, &[], &impacts).expect("reference");
        let mut observed = Vec::new();
        for (phase, probes) in [
            ("position gaps", &ENCODE_POSITION_PROBES),
            ("bit widths", &ENCODE_BITS_PROBES),
            ("bit packing", &ENCODE_PACK_PROBES),
        ] {
            probes.with(|value| value.set(0));
            let mut work = super::super::control::WorkCheck::new(|| {
                if probes.with(std::cell::Cell::get) >= 64 {
                    Err(PostingsError::ZeroBlockSize)
                } else {
                    Ok(())
                }
            });
            let result = encode_v2_controlled(&list, 64, &[], &impacts, &mut work);
            let count = probes.with(std::cell::Cell::get);
            println!("{phase}: canceled={}, probes={count}", result.is_err());
            observed.push((
                phase,
                matches!(result, Err(PostingsError::ZeroBlockSize)),
                count,
            ));
        }
        let mut work = super::super::control::WorkCheck::new(|| Ok::<(), PostingsError>(()));
        let clean = encode_v2_controlled(&list, 64, &[], &impacts, &mut work).expect("clean");
        assert_eq!(clean.as_bytes(), reference.as_bytes());
        assert_eq!(
            PostingsReader::open(clean.as_bytes())
                .expect("reader")
                .decode_all()
                .expect("decoded"),
            list
        );
        for (phase, canceled, count) in observed {
            assert!(canceled, "{phase}");
            assert!((64..=128).contains(&count), "{phase}: {count}");
        }
    }

    #[test]
    fn astra_18_block_impacts_cancel_inside_rows() {
        let mut list = PostingList::new();
        for docid in 0..4_096 {
            list.push(Posting {
                docid,
                tf: 1,
                positions: vec![0],
            })
            .expect("posting");
        }
        let lengths = vec![7; 4_096];
        ENCODE_IMPACT_PROBES.with(|value| value.set(0));
        let mut work = super::super::control::WorkCheck::new(|| {
            if ENCODE_IMPACT_PROBES.with(std::cell::Cell::get) >= 64 {
                Err("cancelled")
            } else {
                Ok(())
            }
        });
        let result = block_impacts_controlled(&list, 64, &lengths, &mut work);
        let count = ENCODE_IMPACT_PROBES.with(std::cell::Cell::get);
        println!("impact rows={count}");
        assert_eq!(result, Err("cancelled"));
        assert!((64..=128).contains(&count));
        let mut work = super::super::control::WorkCheck::new(|| Ok::<(), ()>(()));
        assert_eq!(
            block_impacts_controlled(&list, 64, &lengths, &mut work),
            Ok(vec![
                BlockImpact {
                    max_tf: 1,
                    min_len: 7
                };
                64
            ])
        );
    }

    fn list_from(entries: &[(u32, &[u32])]) -> PostingList {
        let mut list = PostingList::new();
        for (docid, positions) in entries {
            list.push(Posting {
                docid: *docid,
                tf: u32::try_from(positions.len()).expect("small"),
                positions: positions.to_vec(),
            })
            .expect("ascending fixture");
        }
        list
    }

    #[test]
    fn a_single_block_round_trips() {
        let list = list_from(&[(0, &[0, 5]), (3, &[2]), (9, &[1, 4, 7])]);
        let encoded = encode(&list, DEFAULT_POSTINGS_PER_BLOCK, &[]).expect("encodes");
        let reader = PostingsReader::open(encoded.as_bytes()).expect("opens");
        assert_eq!(reader.document_frequency(), 3);
        assert_eq!(reader.postings_per_block(), DEFAULT_POSTINGS_PER_BLOCK);
        assert_eq!(reader.decode_all().expect("decodes"), list);
    }

    #[test]
    fn many_blocks_round_trip_across_the_geometry_boundary() {
        for count in [1_usize, 63, 64, 65, 128, 200] {
            let mut list = PostingList::new();
            for index in 0..count {
                let docid = u32::try_from(index * 3).expect("small");
                list.push(Posting {
                    docid,
                    tf: 2,
                    positions: vec![index as u32, index as u32 + 7],
                })
                .expect("ascending");
            }
            let encoded = encode(&list, DEFAULT_POSTINGS_PER_BLOCK, &[]).expect("encodes");
            let reader = PostingsReader::open(encoded.as_bytes()).expect("opens");
            assert_eq!(
                reader.blocks().len(),
                count.div_ceil(usize::from(DEFAULT_POSTINGS_PER_BLOCK)),
                "block count for {count} postings"
            );
            assert_eq!(reader.decode_all().expect("decodes"), list, "count {count}");
        }
    }

    #[test]
    fn the_block_metadata_row_is_exactly_one_quarter_of_a_cache_line() {
        let mut bytes = Vec::new();
        BlockMeta::default().write(&mut bytes);
        assert_eq!(bytes.len(), BLOCK_META_LEN);
        assert_eq!(BLOCK_META_LEN * 4, 128);
    }

    #[test]
    fn an_empty_list_encodes_and_decodes() {
        let list = PostingList::new();
        let encoded = encode(&list, DEFAULT_POSTINGS_PER_BLOCK, &[]).expect("encodes");
        let reader = PostingsReader::open(encoded.as_bytes()).expect("opens");
        assert_eq!(reader.document_frequency(), 0);
        assert!(reader.decode_all().expect("decodes").is_empty());
    }

    #[test]
    fn bit_widths_at_every_boundary_round_trip() {
        for bits in 0..=32_u8 {
            let value = if bits == 0 {
                0
            } else if bits == 32 {
                u32::MAX
            } else {
                (1_u32 << bits) - 1
            };
            let values = vec![value; 64];
            let mut packed = Vec::new();
            pack_bits(&values, bits, &mut packed);
            let mut output = vec![0_u32; 64];
            let consumed = unpack_bits(&packed, bits, 64, &mut output).expect("unpacks");
            assert_eq!(consumed, packed_len(64, bits), "width {bits}");
            assert_eq!(output, values, "width {bits}");
        }
    }

    #[test]
    fn an_oversized_bit_width_is_a_typed_error() {
        let mut output = [0_u32; 4];
        assert_eq!(
            unpack_bits(&[0; 32], 33, 4, &mut output),
            Err(PostingsError::BitWidthTooLarge { bits: 33 })
        );
    }

    #[test]
    fn a_truncated_packed_run_is_a_typed_error() {
        let mut output = [0_u32; 64];
        let error = unpack_bits(&[0; 2], 8, 64, &mut output).expect_err("must refuse");
        assert!(matches!(error, PostingsError::Truncated { .. }));
    }

    #[test]
    fn out_of_order_docids_are_refused_rather_than_sorted() {
        let mut list = PostingList::new();
        list.push(Posting {
            docid: 5,
            tf: 1,
            positions: vec![0],
        })
        .expect("first");
        assert_eq!(
            list.push(Posting {
                docid: 5,
                tf: 1,
                positions: vec![0],
            }),
            Err(PostingsError::DocidsNotAscending { docid: 5 })
        );
    }

    #[test]
    fn out_of_order_positions_are_refused() {
        let mut list = PostingList::new();
        assert_eq!(
            list.push(Posting {
                docid: 0,
                tf: 2,
                positions: vec![4, 4],
            }),
            Err(PostingsError::PositionsNotAscending { position: 4 })
        );
    }

    #[test]
    fn a_zero_frequency_posting_is_refused() {
        let mut list = PostingList::new();
        assert_eq!(
            list.push(Posting {
                docid: 0,
                tf: 0,
                positions: vec![],
            }),
            Err(PostingsError::ZeroTermFrequency)
        );
    }

    #[test]
    fn the_impact_pair_survives_the_persisted_round_trip() {
        // The pair is what makes a bound sound under drift, so it has to
        // come back out of the bytes exactly as it went in.
        let list = list_from(&[(0, &[0, 1, 2]), (5, &[0]), (9, &[0, 3])]);
        let lengths = vec![40_u32, 0, 0, 0, 0, 7, 0, 0, 0, 900];
        let impacts = block_impacts(&list, 2, &lengths);
        assert_eq!(impacts.len(), 2);
        assert_eq!(
            impacts[0],
            BlockImpact {
                max_tf: 3,
                min_len: 7
            }
        );
        assert_eq!(
            impacts[1],
            BlockImpact {
                max_tf: 2,
                min_len: 900
            }
        );

        let encoded = encode_v2(&list, 2, &[], &impacts).expect("encodes");
        let reader = PostingsReader::open(encoded.as_bytes()).expect("opens");
        assert_eq!(reader.version(), POSTINGS_VERSION);
        let read: Vec<BlockImpact> = reader.blocks().iter().map(BlockImpact::from_meta).collect();
        assert_eq!(read, impacts);
        assert_eq!(reader.decode_all().expect("decodes"), list);
    }

    #[test]
    fn a_version_one_stream_reads_back_with_no_impact_pair() {
        // v1 is still readable, and its blocks report an ABSENT pair rather
        // than a zero one that a caller might mistake for a real bound.
        let list = list_from(&[(0, &[0]), (4, &[1])]);
        let encoded = encode(&list, 64, &[]).expect("encodes");
        let reader = PostingsReader::open(encoded.as_bytes()).expect("opens");
        assert_eq!(reader.version(), POSTINGS_VERSION_V1);
        for meta in reader.blocks() {
            assert!(BlockImpact::from_meta(meta).is_absent());
        }
    }

    #[test]
    fn an_empty_impact_slice_stores_an_absent_pair() {
        let list = list_from(&[(0, &[0])]);
        let encoded = encode_v2(&list, 64, &[], &[]).expect("encodes");
        let reader = PostingsReader::open(encoded.as_bytes()).expect("opens");
        assert_eq!(reader.version(), POSTINGS_VERSION);
        assert!(BlockImpact::from_meta(&reader.blocks()[0]).is_absent());
        assert!(block_impacts(&list, 0, &[]).is_empty());
    }

    #[test]
    fn bad_magic_and_version_are_typed_errors() {
        let list = list_from(&[(0, &[0])]);
        let encoded = encode(&list, 64, &[]).expect("encodes");
        let mut bytes = encoded.as_bytes().to_vec();
        bytes[0] = b'X';
        assert_eq!(
            PostingsReader::open(&bytes).err(),
            Some(PostingsError::BadMagic)
        );

        let mut bytes = encoded.as_bytes().to_vec();
        bytes[4] = 9;
        assert_eq!(
            PostingsReader::open(&bytes).err(),
            Some(PostingsError::UnsupportedVersion { found: 9 })
        );
    }

    #[test]
    fn a_zero_block_size_header_is_refused() {
        let list = list_from(&[(0, &[0])]);
        let encoded = encode(&list, 64, &[]).expect("encodes");
        let mut bytes = encoded.as_bytes().to_vec();
        bytes[6] = 0;
        bytes[7] = 0;
        assert_eq!(
            PostingsReader::open(&bytes).err(),
            Some(PostingsError::ZeroBlockSize)
        );
        assert_eq!(
            encode(&list, 0, &[]).err(),
            Some(PostingsError::ZeroBlockSize)
        );
    }

    #[test]
    fn every_truncation_of_a_valid_stream_is_a_typed_error_not_a_panic() {
        let list = list_from(&[(0, &[0, 3]), (7, &[1]), (100, &[2, 9, 40])]);
        let encoded = encode(&list, 2, &[]).expect("encodes");
        let bytes = encoded.as_bytes();
        for cut in 0..bytes.len() {
            let truncated = &bytes[..cut];
            // A prefix that still parses must decode without panic.
            if let Ok(reader) = PostingsReader::open(truncated) {
                for index in 0..reader.blocks().len() {
                    let _ = reader.decode_block(index);
                }
            }
        }
    }
}
