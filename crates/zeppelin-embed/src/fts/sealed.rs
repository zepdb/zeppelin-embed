//! The sealed lexical segment: the persisted format, on the query path.
//!
//! # What changed, and why it is the whole game
//!
//! Until this module existed, the persisted posting format was dead code.
//! `PostingsReader` was referenced only by tests, `kernels::postings::unpack`
//! and `prefix_sum` had no production caller at all, and every score was
//! computed by walking `Vec<Posting>` out of a `BTreeMap`. Every latency
//! number the engine had ever produced measured a B-tree, not a format.
//!
//! A `Posting` is 32 bytes of struct plus a heap allocation for its
//! positions, and scoring reads 8 of those 32 bytes. The sealed form is
//! frame-of-reference bit-packed: a docid delta at 171,000 documents needs
//! roughly a byte, a term frequency two or three bits. The same posting
//! costs about 2 bytes of traffic instead of 32, it decodes 64 at a time
//! through a vector kernel, and — because each block carries a skip key and
//! an impact pair — whole blocks can be refused without being read.
//!
//! # The active segment is still a `SegmentIndex`
//!
//! [`SegmentIndex`] remains the builder: it accepts documents, it is small
//! by construction, and phrase and snippet queries still read its positions
//! directly. [`SealedSegment::seal`] is the one-way transition, and
//! `LexicalIndex` holds only sealed segments, so there is exactly ONE
//! scoring path rather than two that must be proven to agree.
//!
//! # Positions are not decoded here
//!
//! They are written — the format has carried them since task 13, and
//! retrofitting them would be a break — but the BM25 path never touches
//! them. Decoding positions to score a term is pure waste, and it was the
//! reason `decode_block` materialized a `Vec<Posting>` per block.
//!
//! # Bounds
//!
//! Each block stores `(max_tf, min_len)` over its own field's length array.
//! A query that weights several fields needs a bound over the MERGED stream,
//! and [`TermStream::block_bound`] builds one that is sound for any weight
//! table:
//!
//! ```text
//! merged tf  T(r) = floor(sum_f tf_f(r) * w_f / 1000)
//!                <= sum_f ceil(max_tf_f * w_f / 1000)          = Tmax
//! merged len L(r) = floor(sum_f len_f(r) * w_f / 1000)
//!                >= floor(min_len_f * w_f / 1000) for r in f    >= Lmin
//! ```
//!
//! `term_score` rises with `tf` and falls with `len`, so
//! `term_score(Tmax, Lmin)` dominates every row in the blocks under
//! consideration. In the single-field unit-weight case — the shape every
//! BEIR number is produced with — `Tmax` and `Lmin` are exactly the stored
//! pair, so the bound is the tight one.

use super::bm25::TermScorer;
use super::index::{FieldId, SegmentIndex};
use super::postings::{
    BLOCK_META_LEN, BlockImpact, BlockMeta, DEFAULT_POSTINGS_PER_BLOCK, HEADER_LEN, PostingList,
    PostingsError, PostingsReader, block_impacts, encode_v2,
};
use super::search::FieldWeights;
use crate::kernels::postings::{prefix_sum, unpack};

/// One field's dense analyzed token counts, indexed by row.
#[derive(Clone, Debug)]
struct FieldLengths {
    field: FieldId,
    lengths: Vec<u32>,
}

/// Where one `(term, field)` posting list lives inside the blob.
///
/// Stream boundaries are resolved once, when the segment is sealed and the
/// bytes are validated. The query path then addresses a block by arithmetic
/// and never re-validates, which is what a reader over a mapped region does
/// after its own open.
#[derive(Clone, Copy, Debug)]
struct ListSpan {
    field: FieldId,
    term_start: u32,
    term_len: u32,
    meta_start: u32,
    docids_start: u32,
    tfs_start: u32,
    block_count: u32,
    doc_freq: u32,
    /// Largest term frequency across every block of the list.
    ///
    /// Computed once at seal from the same metadata rows the blocks carry,
    /// so `overall_impact` answers in O(1) instead of walking every row's
    /// 32 bytes on each query.
    overall_max_tf: u32,
    /// Smallest document length across every block of the list.
    overall_min_len: u16,
    /// Documents holding this term in ANY field of the segment.
    union_doc_freq: u32,
    /// Fields of this segment in which the term occurs at all.
    field_count: u32,
}

/// An immutable segment holding its postings in the persisted layout.
#[derive(Clone, Debug, Default)]
pub struct SealedSegment {
    /// Concatenated term bytes, ascending; a span points into this.
    terms: Vec<u8>,
    /// One entry per `(term, field)`, ascending by term then field.
    spans: Vec<ListSpan>,
    /// Every encoded posting list, back to back.
    blob: Vec<u8>,
    lengths: Vec<FieldLengths>,
    /// Each row's length summed over every field, precomputed at seal.
    ///
    /// The unit-weight multi-field scorer -- the shape every BEIR run uses
    /// -- asks for exactly this, and it does not depend on the query. Built
    /// once here rather than once per query.
    total_lengths: Vec<u32>,
    row_count: u32,
    postings_per_block: u16,
}

impl SealedSegment {
    /// Seals an active segment into the persisted layout.
    ///
    /// # Errors
    ///
    /// Returns [`PostingsError`] when the encoder rejects the geometry or
    /// the bytes it just produced do not validate. Both are broken-writer
    /// conditions rather than bad input, and they fail loudly here rather
    /// than becoming a decode fault at query time.
    pub fn seal(segment: &SegmentIndex) -> Result<Self, PostingsError> {
        let per_block = DEFAULT_POSTINGS_PER_BLOCK;
        let mut sealed = Self {
            terms: Vec::new(),
            spans: Vec::new(),
            blob: Vec::new(),
            lengths: Vec::new(),
            total_lengths: Vec::new(),
            row_count: segment.row_count(),
            postings_per_block: per_block,
        };
        for field in segment.fields() {
            let lengths = segment.field_lengths(field).unwrap_or(&[]).to_vec();
            sealed.lengths.push(FieldLengths { field, lengths });
        }

        let rows = usize::try_from(sealed.row_count).unwrap_or(0);
        sealed.total_lengths = vec![0_u32; rows];
        for entry in &sealed.lengths {
            for (slot, length) in entry.lengths.iter().enumerate() {
                if let Some(total) = sealed.total_lengths.get_mut(slot) {
                    *total = total.saturating_add(*length);
                }
            }
        }

        // Union document frequencies are computed HERE, over the in-memory
        // sorted lists the encoder is reading anyway. The previous form
        // sealed first and then re-decoded every block of every
        // multi-field term to count the union — a full decode pass over
        // bytes written milliseconds earlier.
        let mut group_term: Option<&[u8]> = None;
        let mut group_start = 0_usize;
        let mut group_lists: Vec<&PostingList> = Vec::new();
        for (key, list) in segment.postings() {
            if list.is_empty() {
                continue;
            }
            if group_term != Some(key.term.as_slice()) {
                finish_union_group(&mut sealed.spans, group_start, &group_lists);
                group_term = Some(key.term.as_slice());
                group_start = sealed.spans.len();
                group_lists.clear();
            }
            group_lists.push(list);
            let field_lengths = sealed
                .lengths
                .iter()
                .find(|entry| entry.field == key.field)
                .map_or(&[][..], |entry| entry.lengths.as_slice());
            let impacts = block_impacts(list, per_block, field_lengths);
            let encoded = encode_v2(list, per_block, &[], &impacts)?;
            let bytes = encoded.as_bytes();
            // Validate once, here, and resolve the stream boundaries the
            // query path will address by arithmetic.
            let reader = PostingsReader::open(bytes)?;
            let base = u32::try_from(sealed.blob.len()).unwrap_or(u32::MAX);
            let block_count = u32::try_from(reader.blocks().len()).unwrap_or(u32::MAX);
            let meta_start = base.saturating_add(HEADER_LEN_U32);
            let streams = meta_start.saturating_add(block_count.saturating_mul(META_LEN_U32));
            let docid_bytes = reader
                .blocks()
                .iter()
                .map(|meta| stream_end(meta.docids_offset, meta.count, meta.docid_bits))
                .max()
                .unwrap_or(0);
            let mut overall_max_tf = 0_u32;
            let mut overall_min_len = u16::MAX;
            for meta in reader.blocks() {
                let impact = BlockImpact::from_meta(meta);
                overall_max_tf = overall_max_tf.max(impact.max_tf);
                overall_min_len = overall_min_len.min(impact.min_len);
            }

            let term_start = u32::try_from(sealed.terms.len()).unwrap_or(u32::MAX);
            sealed.terms.extend_from_slice(&key.term);
            sealed.spans.push(ListSpan {
                field: key.field,
                term_start,
                term_len: u32::try_from(key.term.len()).unwrap_or(u32::MAX),
                meta_start,
                docids_start: streams,
                tfs_start: streams.saturating_add(docid_bytes),
                block_count,
                doc_freq: reader.document_frequency(),
                overall_max_tf,
                overall_min_len,
                // Filled in once every field of this term is known.
                union_doc_freq: 0,
                field_count: 0,
            });
            sealed.blob.extend_from_slice(bytes);
        }
        finish_union_group(&mut sealed.spans, group_start, &group_lists);
        Ok(sealed)
    }

    /// Returns the term bytes of one span.
    fn term_of(&self, index: usize) -> &[u8] {
        let Some(span) = self.spans.get(index) else {
            return &[];
        };
        let start = usize::try_from(span.term_start).unwrap_or(usize::MAX);
        let len = usize::try_from(span.term_len).unwrap_or(0);
        self.terms
            .get(start..start.saturating_add(len))
            .unwrap_or(&[])
    }

    /// Returns the number of rows.
    #[must_use]
    pub const fn row_count(&self) -> u32 {
        self.row_count
    }

    /// Returns true when the segment holds no rows.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.row_count == 0
    }

    /// Returns the postings-per-block geometry every list was written with.
    #[must_use]
    pub const fn postings_per_block(&self) -> u16 {
        self.postings_per_block
    }

    /// Returns one field's dense length array, if the field was ever set.
    #[must_use]
    pub fn field_lengths(&self, field: FieldId) -> Option<&[u32]> {
        self.lengths
            .iter()
            .find(|entry| entry.field == field)
            .map(|entry| entry.lengths.as_slice())
    }

    /// Returns every row's length summed over every field.
    ///
    /// Precomputed at seal; see the field's own documentation.
    #[must_use]
    pub fn total_lengths(&self) -> &[u32] {
        &self.total_lengths
    }

    /// Returns the fields this segment recorded lengths for, ascending.
    pub fn fields(&self) -> impl Iterator<Item = FieldId> + '_ {
        self.lengths.iter().map(|entry| entry.field)
    }

    /// Returns the analyzed token count of one row's field.
    #[must_use]
    pub fn field_length(&self, row: u32, field: FieldId) -> u32 {
        self.field_lengths(field)
            .and_then(|lengths| {
                usize::try_from(row)
                    .ok()
                    .and_then(|slot| lengths.get(slot).copied())
            })
            .unwrap_or(0)
    }

    /// Returns the analyzed token count of one row across every field.
    #[must_use]
    pub fn document_length(&self, row: u32) -> u32 {
        let Ok(slot) = usize::try_from(row) else {
            return 0;
        };
        self.lengths
            .iter()
            .filter_map(|entry| entry.lengths.get(slot).copied())
            .fold(0_u32, u32::saturating_add)
    }

    /// Returns the segment's total analyzed token count.
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.lengths
            .iter()
            .flat_map(|entry| entry.lengths.iter())
            .map(|length| u64::from(*length))
            .sum()
    }

    /// Returns the index of one term's first span, and how many it has.
    fn term_span_range(&self, term: &[u8]) -> (usize, usize) {
        let start = self.spans.partition_point(|span| {
            let bytes_start = usize::try_from(span.term_start).unwrap_or(usize::MAX);
            let len = usize::try_from(span.term_len).unwrap_or(0);
            self.terms
                .get(bytes_start..bytes_start.saturating_add(len))
                .unwrap_or(&[])
                < term
        });
        let mut end = start;
        while end < self.spans.len() && self.term_of(end) == term {
            end = end.saturating_add(1);
        }
        (start, end)
    }

    /// Returns the documents in this segment holding `term` in any of
    /// `fields`.
    ///
    /// A document carrying the term in two fields counts once, which is what
    /// makes this a *document* frequency rather than a posting count.
    #[must_use]
    pub fn document_frequency(&self, term: &[u8], fields: &[FieldId]) -> u32 {
        let (start, end) = self.term_span_range(term);
        let mut matched: Option<usize> = None;
        let mut count = 0_usize;
        for index in start..end {
            let Some(span) = self.spans.get(index) else {
                continue;
            };
            if fields.contains(&span.field) {
                matched = matched.or(Some(index));
                count = count.saturating_add(1);
            }
        }
        match count {
            0 => 0,
            1 => matched
                .and_then(|index| self.spans.get(index))
                .map_or(0, |span| span.doc_freq),
            _ => {
                let total = u32::try_from(count).unwrap_or(u32::MAX);
                let all_fields = self
                    .spans
                    .get(start)
                    .is_some_and(|span| span.field_count == total);
                if all_fields {
                    // The precomputed union already answers this exactly.
                    return self.spans.get(start).map_or(0, |span| span.union_doc_freq);
                }
                // A strict subset of the term's fields: rare, and exact.
                let mut cursors: Vec<ListCursor<'_>> = Vec::new();
                for index in start..end {
                    let Some(span) = self.spans.get(index) else {
                        continue;
                    };
                    if fields.contains(&span.field)
                        && let Some(cursor) = self.cursor_at(index, 1_000)
                    {
                        cursors.push(cursor);
                    }
                }
                union_count(&mut cursors)
            }
        }
    }

    /// Opens a cursor over the span at `index`, scaled by `weight`.
    fn cursor_at(&self, index: usize, weight: u64) -> Option<ListCursor<'_>> {
        let span = self.spans.get(index)?;
        let per_block = usize::from(self.postings_per_block);
        Some(ListCursor {
            meta: self.slice_from(span.meta_start),
            docids: self.slice_from(span.docids_start),
            tfs: self.slice_from(span.tfs_start),
            block_count: usize::try_from(span.block_count).unwrap_or(0),
            per_block,
            weight,
            block: NO_BLOCK,
            slot: 0,
            count: 0,
            impact: BlockImpact::default(),
            tfs_block: NO_BLOCK,
            tf_offset: 0,
            tf_bits: 0,
            overall: BlockImpact {
                max_tf: span.overall_max_tf,
                min_len: span.overall_min_len,
            },
            decoded_docids: vec![0; per_block],
            decoded_tfs: vec![0; per_block],
            blocks_decoded: 0,
            blocks_skipped: 0,
            tf_blocks_decoded: 0,
        })
    }

    /// Returns the blob from `offset` onwards, or an empty slice.
    fn slice_from(&self, offset: u32) -> &[u8] {
        usize::try_from(offset)
            .ok()
            .and_then(|start| self.blob.get(start..))
            .unwrap_or(&[])
    }
}

/// Header bytes preceding the block metadata rows, as a `u32`.
const HEADER_LEN_U32: u32 = HEADER_LEN as u32;

/// Bytes in one metadata row, as a `u32`.
const META_LEN_U32: u32 = BLOCK_META_LEN as u32;

/// Sentinel for "no block is loaded".
const NO_BLOCK: usize = usize::MAX;

/// Returns the end offset of one block's packed run within its stream.
fn stream_end(offset: u32, count: u16, bits: u8) -> u32 {
    let packed = u32::from(count).saturating_mul(u32::from(bits)).div_ceil(8);
    offset.saturating_add(packed)
}

/// Stamps one term's spans with its union document frequency.
///
/// `start..spans.len()` are the spans the seal loop just pushed for one
/// term, in field order; `lists` are the same posting lists, still in
/// memory. A single-field term's union is its own document frequency; a
/// multi-field term's is one linear merge over already-sorted lists.
fn finish_union_group(spans: &mut [ListSpan], start: usize, lists: &[&PostingList]) {
    let end = spans.len();
    if end <= start {
        return;
    }
    let count = u32::try_from(end.saturating_sub(start)).unwrap_or(u32::MAX);
    let union = if count == 1 {
        spans.get(start).map_or(0, |span| span.doc_freq)
    } else {
        union_postings(lists)
    };
    if let Some(range) = spans.get_mut(start..end) {
        for span in range {
            span.union_doc_freq = union;
            span.field_count = count;
        }
    }
}

/// Counts the distinct docids across in-memory sorted posting lists.
fn union_postings(lists: &[&PostingList]) -> u32 {
    let mut positions = vec![0_usize; lists.len()];
    let mut distinct = 0_u32;
    loop {
        let mut lowest: Option<u32> = None;
        for (slot, list) in lists.iter().enumerate() {
            let position = positions.get(slot).copied().unwrap_or(usize::MAX);
            if let Some(posting) = list.postings().get(position) {
                lowest = Some(lowest.map_or(posting.docid, |low| low.min(posting.docid)));
            }
        }
        let Some(low) = lowest else {
            break;
        };
        distinct = distinct.saturating_add(1);
        for (slot, list) in lists.iter().enumerate() {
            let Some(position) = positions.get_mut(slot) else {
                continue;
            };
            if list
                .postings()
                .get(*position)
                .is_some_and(|posting| posting.docid == low)
            {
                *position = position.saturating_add(1);
            }
        }
    }
    distinct
}

/// Counts the distinct rows across a set of cursors by linear merge.
fn union_count(cursors: &mut [ListCursor<'_>]) -> u32 {
    for cursor in cursors.iter_mut() {
        cursor.reset();
    }
    let mut distinct = 0_u32;
    loop {
        let Some(row) = cursors.iter().filter_map(ListCursor::current).min() else {
            break;
        };
        distinct = distinct.saturating_add(1);
        for cursor in cursors.iter_mut() {
            if cursor.current() == Some(row) {
                cursor.advance();
            }
        }
    }
    distinct
}

/// A decoding cursor over one `(term, field)` posting list.
///
/// Two scratch buffers are allocated when the cursor opens and reused for
/// every block, so decode never allocates. Term frequencies are scaled by
/// the field's weight as they are decoded.
#[derive(Clone, Debug)]
pub struct ListCursor<'segment> {
    meta: &'segment [u8],
    docids: &'segment [u8],
    tfs: &'segment [u8],
    block_count: usize,
    per_block: usize,
    weight: u64,
    block: usize,
    slot: usize,
    count: usize,
    /// The loaded block's impact pair, cached when the block is decoded so
    /// a bound read costs no metadata parse.
    impact: BlockImpact,
    /// Which block the tf scratch buffer currently holds.
    ///
    /// Term frequencies are decoded LAZILY: positioning a cursor decodes
    /// docids only, and the tf run of a block is unpacked the first time a
    /// posting in it is actually scored. A traversal that lands in a block
    /// and jumps on without scoring never touches the tf bytes.
    tfs_block: usize,
    /// The loaded block's tf stream offset, stashed at load.
    tf_offset: u32,
    /// The loaded block's tf bit width, stashed at load.
    tf_bits: u8,
    /// The whole list's impact pair, computed at seal and copied here.
    overall: BlockImpact,
    decoded_docids: Vec<u32>,
    decoded_tfs: Vec<u32>,
    blocks_decoded: u64,
    blocks_skipped: u64,
    tf_blocks_decoded: u64,
}

impl ListCursor<'_> {
    /// Reads one metadata row.
    fn meta_at(&self, index: usize) -> Option<BlockMeta> {
        let start = index.checked_mul(BLOCK_META_LEN)?;
        let row = self.meta.get(start..start.checked_add(BLOCK_META_LEN)?)?;
        BlockMeta::read(row).ok()
    }

    /// Reads one block's skip key without touching the rest of its row.
    fn last_docid(&self, index: usize) -> Option<u32> {
        let start = index.checked_mul(BLOCK_META_LEN)?;
        let bytes = self.meta.get(start..start.checked_add(4)?)?;
        let mut buffer = [0_u8; 4];
        buffer.copy_from_slice(bytes);
        Some(u32::from_le_bytes(buffer))
    }

    /// Returns one block's stored impact pair.
    fn impact_at(&self, index: usize) -> BlockImpact {
        self.meta_at(index)
            .map_or_else(BlockImpact::default, |meta| BlockImpact::from_meta(&meta))
    }

    /// Decodes block `index` into the scratch buffers.
    ///
    /// This is the production caller of `kernels::postings::unpack` and
    /// `prefix_sum`. Positions are deliberately not decoded.
    fn load(&mut self, index: usize) -> bool {
        if self.block == index {
            return true;
        }
        let Some(meta) = self.meta_at(index) else {
            return false;
        };
        let count = usize::from(meta.count).min(self.per_block);
        let docid_start = usize::try_from(meta.docids_offset).unwrap_or(usize::MAX);
        let Some(packed) = self.docids.get(docid_start..) else {
            return false;
        };
        // Block zero's first delta is the absolute id, which is the same as
        // a gap from zero; every later block resumes from its predecessor.
        let base = if index == 0 {
            0
        } else {
            self.last_docid(index.saturating_sub(1)).unwrap_or(0)
        };
        let Some(target) = self.decoded_docids.get_mut(..count) else {
            return false;
        };
        if unpack(packed, meta.docid_bits, count, target).is_none() {
            return false;
        }
        prefix_sum(target, base);

        // The tf run is NOT decoded here; its geometry is stashed and the
        // decode happens on the first `current_tf` against this block.
        self.tf_offset = meta.tfs_offset;
        self.tf_bits = meta.tf_bits;
        self.block = index;
        self.count = count;
        self.impact = BlockImpact::from_meta(&meta);
        self.blocks_decoded = self.blocks_decoded.saturating_add(1);
        true
    }

    /// Rewinds to the first posting.
    ///
    /// A cursor already sitting in block zero is rewound without decoding
    /// it again: a re-decode would charge the traversal for work it did not
    /// ask for, and `blocks_decoded` is a gated counter.
    pub fn reset(&mut self) {
        self.slot = 0;
        if self.block == 0 {
            return;
        }
        self.block = NO_BLOCK;
        self.count = 0;
        if self.block_count > 0 {
            self.load(0);
        }
    }

    /// Returns the document id at the cursor.
    #[must_use]
    pub fn current(&self) -> Option<u32> {
        if self.block == NO_BLOCK {
            return None;
        }
        self.decoded_docids.get(self.slot).copied()
    }

    /// Unpacks the loaded block's tf run, once per block, on demand.
    fn ensure_tfs(&mut self) -> bool {
        if self.tfs_block == self.block {
            return true;
        }
        let start = usize::try_from(self.tf_offset).unwrap_or(usize::MAX);
        let Some(packed) = self.tfs.get(start..) else {
            return false;
        };
        let Some(target) = self.decoded_tfs.get_mut(..self.count) else {
            return false;
        };
        if unpack(packed, self.tf_bits, self.count, target).is_none() {
            return false;
        }
        self.tfs_block = self.block;
        self.tf_blocks_decoded = self.tf_blocks_decoded.saturating_add(1);
        true
    }

    /// Returns the unscaled term frequency at the cursor.
    ///
    /// The first call against a block pays that block's tf decode; see
    /// [`Self::ensure_tfs`].
    #[must_use]
    pub fn current_tf(&mut self) -> Option<u32> {
        if self.block == NO_BLOCK || !self.ensure_tfs() {
            return None;
        }
        self.decoded_tfs.get(self.slot).copied()
    }

    /// Returns the weight-scaled term frequency contribution at the cursor.
    ///
    /// Scaled in `u64` and left unrounded; the merge divides by 1,000 once,
    /// after summing, exactly as the in-memory merge did.
    #[must_use]
    pub fn current_weighted_tf(&mut self) -> Option<u64> {
        self.current_tf()
            .map(|tf| u64::from(tf).saturating_mul(self.weight))
    }

    /// Advances one posting, loading the next block when the current ends.
    pub fn advance(&mut self) {
        if self.block == NO_BLOCK {
            return;
        }
        self.slot = self.slot.saturating_add(1);
        if self.slot < self.count {
            return;
        }
        let next = self.block.saturating_add(1);
        if next >= self.block_count || !self.load(next) {
            self.block = NO_BLOCK;
            return;
        }
        self.slot = 0;
    }

    /// Advances to the first posting at or after `row`.
    ///
    /// Skip keys ascend, so the first block that can hold `row` is a binary
    /// search over the metadata rows. Only the four skip-key bytes of each
    /// row are read while searching; nothing between here and the target is
    /// decoded, which is where block skipping actually pays.
    pub fn seek(&mut self, row: u32) {
        if self.block == NO_BLOCK {
            return;
        }
        if self.current().is_some_and(|current| current >= row) {
            return;
        }
        if self.last_docid(self.block).is_some_and(|last| last < row) {
            let mut low = self.block.saturating_add(1);
            let mut high = self.block_count;
            while low < high {
                let middle = low.saturating_add(high.saturating_sub(low) / 2);
                if self.last_docid(middle).is_some_and(|last| last < row) {
                    low = middle.saturating_add(1);
                } else {
                    high = middle;
                }
            }
            let hopped = low.saturating_sub(self.block.saturating_add(1));
            self.blocks_skipped = self
                .blocks_skipped
                .saturating_add(u64::try_from(hopped).unwrap_or(u64::MAX));
            if low >= self.block_count || !self.load(low) {
                self.block = NO_BLOCK;
                return;
            }
            self.slot = 0;
        }
        loop {
            if self.block == NO_BLOCK {
                return;
            }
            // The landing position inside the decoded block is a binary
            // search, not a walk: the block is sorted and already paid for.
            let found = self
                .decoded_docids
                .get(self.slot..self.count)
                .map_or(0, |within| within.partition_point(|&docid| docid < row));
            let landed = self.slot.saturating_add(found);
            if landed < self.count {
                self.slot = landed;
                return;
            }
            // Every remaining docid sits below `row`: step off the block's
            // end, which rolls into the next block or exhausts the run.
            self.slot = self.count.saturating_sub(1);
            self.advance();
        }
    }

    /// Returns true when the cursor has passed the last posting.
    #[must_use]
    pub const fn exhausted(&self) -> bool {
        self.block == NO_BLOCK
    }

    /// Returns the impact pair of the block the cursor sits in.
    ///
    /// Cached when the block was loaded; no metadata row is re-parsed.
    #[must_use]
    pub const fn current_impact(&self) -> BlockImpact {
        if self.block == NO_BLOCK {
            return BlockImpact {
                max_tf: 0,
                min_len: 0,
            };
        }
        self.impact
    }

    /// Returns the last document id of the block the cursor sits in.
    ///
    /// The skip key of the current block, read from its four metadata
    /// bytes without decoding anything.
    #[must_use]
    pub fn current_block_last(&self) -> Option<u32> {
        if self.block == NO_BLOCK {
            return None;
        }
        self.last_docid(self.block)
    }

    /// Returns the impact pair of the block that could contain `row`.
    ///
    /// `None` when this run provably cannot contribute to `row`: the
    /// cursor has already advanced past it, or every remaining block ends
    /// below it. Only skip keys and one metadata row are read; nothing is
    /// decoded. This is what lets a traversal bound a candidate it has not
    /// paid to visit.
    #[must_use]
    pub fn impact_for(&self, row: u32) -> Option<BlockImpact> {
        if self.block == NO_BLOCK {
            return None;
        }
        if self.current().is_some_and(|current| current > row) {
            // Postings ascend and cursors only move forward, so a cursor
            // past `row` contributes exactly nothing to it.
            return None;
        }
        if self.last_docid(self.block).is_some_and(|last| last >= row) {
            return Some(self.current_impact());
        }
        let mut low = self.block.saturating_add(1);
        let mut high = self.block_count;
        while low < high {
            let middle = low.saturating_add(high.saturating_sub(low) / 2);
            if self.last_docid(middle).is_some_and(|last| last < row) {
                low = middle.saturating_add(1);
            } else {
                high = middle;
            }
        }
        if low >= self.block_count {
            return None;
        }
        Some(self.impact_at(low))
    }

    /// Returns the impact pair dominating every block of the list.
    ///
    /// Computed once at seal; reading it costs nothing per query.
    #[must_use]
    pub const fn overall_impact(&self) -> BlockImpact {
        self.overall
    }
}

/// One query term's merged postings across the weighted fields of a segment.
///
/// The merge is lazy: nothing is materialized, so a pruned traversal that
/// skips a block never decodes it. That is the difference between reading a
/// format and copying one.
#[derive(Clone, Debug)]
pub struct TermStream<'segment> {
    runs: Vec<ListCursor<'segment>>,
    weights: Vec<u64>,
    /// True when the stream is one run at unit weight — the shape most
    /// terms take, since a term absent from a field contributes no run.
    /// The merge machinery is then the identity, and `refresh` skips it.
    unit_single: bool,
    /// True when every weight is at least unit, so any present row's
    /// merged tf is at least one and the head can be declared valid
    /// WITHOUT computing its tf. That is what makes the tf decode lazy
    /// end to end: a traversal that skips the row never pays for it.
    heads_always_valid: bool,
    head: Option<u32>,
    /// The merged tf of the head row, memoized on first demand.
    head_tf: Option<u32>,
}

impl<'segment> TermStream<'segment> {
    /// Opens the stream for `term` over the weighted fields of `segment`.
    ///
    /// Returns `None` when no weighted field carries the term.
    #[must_use]
    pub fn open(
        segment: &'segment SealedSegment,
        term: &[u8],
        weights: &FieldWeights,
    ) -> Option<Self> {
        let (start, end) = segment.term_span_range(term);
        let mut runs: Vec<ListCursor<'segment>> = Vec::new();
        let mut scales: Vec<u64> = Vec::new();
        for index in start..end {
            let span = segment.spans.get(index)?;
            let weight = u64::from(weights.weight(span.field));
            if weight == 0 {
                continue;
            }
            let cursor = segment.cursor_at(index, weight)?;
            runs.push(cursor);
            scales.push(weight);
        }
        if runs.is_empty() {
            return None;
        }
        let unit_single = runs.len() == 1 && scales.first().copied() == Some(1_000);
        let heads_always_valid = scales.iter().all(|weight| *weight >= 1_000);
        let mut stream = Self {
            runs,
            weights: scales,
            unit_single,
            heads_always_valid,
            head: None,
            head_tf: None,
        };
        stream.reset();
        Some(stream)
    }

    /// Rewinds every run to the first posting.
    pub fn reset(&mut self) {
        for run in &mut self.runs {
            run.reset();
        }
        self.refresh();
    }

    /// Recomputes the merged head, skipping rows whose weighted tf rounds
    /// to zero — exactly what the in-memory merge filtered out.
    ///
    /// When every weight is at least unit the zero-tf filter cannot fire,
    /// so the head is declared from docids alone and its tf is left for
    /// [`Self::current_tf`] to compute if the row is ever scored.
    fn refresh(&mut self) {
        self.head_tf = None;
        if self.heads_always_valid {
            self.head = if self.unit_single {
                self.runs.first().and_then(ListCursor::current)
            } else {
                self.runs.iter().filter_map(ListCursor::current).min()
            };
            return;
        }
        loop {
            let Some(row) = self.runs.iter().filter_map(ListCursor::current).min() else {
                self.head = None;
                return;
            };
            let mut total = 0_u64;
            for run in &mut self.runs {
                if run.current() == Some(row) {
                    total = total.saturating_add(run.current_weighted_tf().unwrap_or(0));
                }
            }
            let tf = u32::try_from(total / 1_000).unwrap_or(u32::MAX);
            if tf > 0 {
                self.head = Some(row);
                self.head_tf = Some(tf);
                return;
            }
            self.step(row);
        }
    }

    /// Advances every run sitting on `row`.
    fn step(&mut self, row: u32) {
        for run in &mut self.runs {
            if run.current() == Some(row) {
                run.advance();
            }
        }
    }

    /// Returns the merged row at the head.
    #[must_use]
    pub const fn current_row(&self) -> Option<u32> {
        self.head
    }

    /// Returns the merged weighted tf at the head, computing it on first
    /// demand. This is the only place a traversal pays a tf decode.
    #[must_use]
    pub fn current_tf(&mut self) -> Option<u32> {
        let row = self.head?;
        if let Some(tf) = self.head_tf {
            return Some(tf);
        }
        let mut total = 0_u64;
        for run in &mut self.runs {
            if run.current() == Some(row) {
                total = total.saturating_add(run.current_weighted_tf().unwrap_or(0));
            }
        }
        let tf = u32::try_from(total / 1_000).unwrap_or(u32::MAX);
        self.head_tf = Some(tf);
        Some(tf)
    }

    /// Advances past the current row.
    pub fn advance(&mut self) {
        let Some(row) = self.head else {
            return;
        };
        self.step(row);
        self.refresh();
    }

    /// Advances to the first merged row at or after `row`.
    pub fn seek(&mut self, row: u32) {
        for run in &mut self.runs {
            run.seek(row);
        }
        self.refresh();
    }

    /// Returns true when every run is spent.
    #[must_use]
    pub const fn exhausted(&self) -> bool {
        self.head.is_none()
    }

    /// Blocks this stream decoded.
    #[must_use]
    pub fn blocks_decoded(&self) -> u64 {
        self.runs
            .iter()
            .map(|run| run.blocks_decoded)
            .fold(0_u64, u64::saturating_add)
    }

    /// Blocks this stream jumped over without decoding.
    #[must_use]
    pub fn blocks_skipped(&self) -> u64 {
        self.runs
            .iter()
            .map(|run| run.blocks_skipped)
            .fold(0_u64, u64::saturating_add)
    }

    /// Blocks whose tf run this stream actually unpacked.
    ///
    /// At most [`Self::blocks_decoded`]; strictly less whenever a landed
    /// block was jumped past without any of its postings being scored.
    #[must_use]
    pub fn tf_blocks_decoded(&self) -> u64 {
        self.runs
            .iter()
            .map(|run| run.tf_blocks_decoded)
            .fold(0_u64, u64::saturating_add)
    }

    /// Postings this stream can produce, summed over its runs.
    ///
    /// Read from the block metadata, so it costs no decode.
    #[must_use]
    pub fn posting_count(&self) -> usize {
        self.runs
            .iter()
            .map(|run| {
                run.block_count
                    .saturating_sub(1)
                    .saturating_mul(run.per_block)
                    .saturating_add(
                        run.meta_at(run.block_count.saturating_sub(1))
                            .map_or(0, |meta| usize::from(meta.count)),
                    )
            })
            .fold(0_usize, usize::saturating_add)
    }

    /// The bound over the blocks the runs currently sit in.
    ///
    /// See the module docs for why `(Tmax, Lmin)` dominates every merged row
    /// under any weight table.
    #[must_use]
    pub fn block_bound(&self, scorer: &TermScorer) -> f64 {
        self.bound_from(scorer, |run| {
            (!run.exhausted()).then(|| run.current_impact())
        })
    }

    /// The bound over the blocks that could contain `row`, without decode.
    ///
    /// Each run contributes the impact pair of the block its skip keys say
    /// could hold `row`; a run that has already passed `row`, or whose
    /// remaining blocks all end below it, contributes nothing — which is
    /// exact rather than conservative, because postings ascend. Costs one
    /// binary search over skip keys per run and no decode, so a candidate
    /// can be refused before any cursor is paid to move.
    #[must_use]
    pub fn bound_for(&self, row: u32, scorer: &TermScorer) -> f64 {
        self.bound_from(scorer, |run| run.impact_for(row))
    }

    /// The largest row for which [`Self::block_bound`] still dominates.
    ///
    /// # Why a traversal needs this
    ///
    /// `block_bound` is built from the impact pairs of the blocks the runs
    /// are sitting in *right now*. It bounds any row up to the first block
    /// boundary any run crosses, and no further: past that, one run has
    /// left the block whose pair the bound was taken from.
    ///
    /// That makes this the horizon a block-max traversal may jump to. When
    /// a pivot's bound cannot reach the top-k threshold, every row from the
    /// pivot to this horizon is provably unreachable too, so the whole span
    /// can be skipped instead of one posting.
    #[must_use]
    pub fn block_horizon(&self) -> Option<u32> {
        self.runs
            .iter()
            .filter(|run| !run.exhausted())
            .filter_map(ListCursor::current_block_last)
            .min()
    }

    /// The bound over every block of every run: the term's upper bound.
    #[must_use]
    pub fn upper_bound(&self, scorer: &TermScorer) -> f64 {
        self.bound_from(scorer, |run| {
            (!run.exhausted()).then(|| run.overall_impact())
        })
    }

    /// Combines per-run impact pairs into one merged bound.
    ///
    /// A run whose picker returns `None` contributes nothing: it is spent,
    /// or it provably cannot reach the rows under consideration.
    fn bound_from(
        &self,
        scorer: &TermScorer,
        pick: impl Fn(&ListCursor<'segment>) -> Option<BlockImpact>,
    ) -> f64 {
        let mut max_tf = 0_u64;
        let mut min_len = u64::MAX;
        for (run, weight) in self.runs.iter().zip(self.weights.iter()) {
            let Some(impact) = pick(run) else {
                continue;
            };
            // Round the weighted maximum UP and the weighted minimum DOWN:
            // both directions push the bound above the truth, never below.
            max_tf = max_tf.saturating_add(
                u64::from(impact.max_tf)
                    .saturating_mul(*weight)
                    .div_ceil(1_000),
            );
            min_len = min_len.min(u64::from(impact.min_len).saturating_mul(*weight) / 1_000);
        }
        if min_len == u64::MAX {
            return 0.0;
        }
        scorer.score(
            super::bm25::Tf(u32::try_from(max_tf).unwrap_or(u32::MAX)),
            super::bm25::DocLen(u32::try_from(min_len).unwrap_or(u32::MAX)),
        )
    }
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
    use crate::fts::index::{DEFAULT_FIELD, Document};
    use crate::fts::postings::{Posting, PostingList};
    use crate::fts::tokenizer::{Analyzer, Profile};

    fn analyzer() -> Analyzer {
        Analyzer::new(Profile::Code.config()).expect("valid config")
    }

    fn segment_of(texts: &[String]) -> SegmentIndex {
        let analyzer = analyzer();
        let mut segment = SegmentIndex::new();
        for text in texts {
            segment
                .push_document(&analyzer, &Document::with_text(text))
                .expect("indexable");
        }
        segment
    }

    #[test]
    fn a_sealed_cursor_reproduces_every_posting_of_the_active_segment() {
        // Enough documents to span several blocks, with a term whose gaps
        // vary so the docid bit width changes from block to block.
        let texts: Vec<String> = (0..500)
            .map(|index| {
                if index % 3 == 0 {
                    format!("alpha filler{index}")
                } else {
                    format!("beta filler{index}")
                }
            })
            .collect();
        let active = segment_of(&texts);
        let sealed = SealedSegment::seal(&active).expect("seals");

        for term in [b"alpha".as_slice(), b"beta".as_slice()] {
            let list = active.posting_list(term, DEFAULT_FIELD).expect("present");
            let mut cursor = sealed
                .cursor_at(sealed.term_span_range(term).0, 1_000)
                .expect("span");
            cursor.reset();
            for posting in list.postings() {
                assert_eq!(cursor.current(), Some(posting.docid));
                assert_eq!(
                    cursor.current_weighted_tf(),
                    Some(u64::from(posting.tf) * 1_000)
                );
                cursor.advance();
            }
            assert!(cursor.exhausted(), "the cursor must end with the list");
        }
    }

    #[test]
    fn a_landed_block_pays_its_tf_decode_only_when_scored() {
        // Positioning decodes docids; term frequencies stay packed until a
        // posting is actually scored. A traversal that lands in a block
        // and jumps on without scoring must never touch the tf bytes.
        let texts: Vec<String> = (0..400).map(|index| format!("alpha d{index}")).collect();
        let sealed = SealedSegment::seal(&segment_of(&texts)).expect("seals");
        let mut stream = TermStream::open(
            &sealed,
            b"alpha",
            &crate::fts::search::FieldWeights::flat(&[DEFAULT_FIELD]),
        )
        .expect("stream");
        assert_eq!(
            stream.tf_blocks_decoded(),
            0,
            "opening must not touch tf bytes"
        );
        stream.seek(200);
        stream.seek(383);
        assert!(stream.blocks_decoded() >= 3, "three landings were paid");
        assert_eq!(
            stream.tf_blocks_decoded(),
            0,
            "positioning must not touch tf bytes"
        );
        assert_eq!(stream.current_row(), Some(383));
        assert_eq!(stream.current_tf(), Some(1));
        assert_eq!(
            stream.tf_blocks_decoded(),
            1,
            "scoring pays exactly the one block it reads"
        );
    }

    #[test]
    fn seeking_jumps_whole_blocks_without_decoding_them() {
        let texts: Vec<String> = (0..400).map(|index| format!("alpha d{index}")).collect();
        let sealed = SealedSegment::seal(&segment_of(&texts)).expect("seals");
        let mut cursor = sealed
            .cursor_at(sealed.term_span_range(b"alpha").0, 1_000)
            .expect("span");
        cursor.reset();
        let decoded_before = cursor.blocks_decoded;
        cursor.seek(383);
        assert_eq!(cursor.current(), Some(383));
        assert!(
            cursor.blocks_skipped >= 4,
            "a 400-posting list at 64 per block must skip at least four"
        );
        assert_eq!(
            cursor.blocks_decoded,
            decoded_before + 1,
            "a seek must decode exactly the block it lands in"
        );
    }

    #[test]
    fn seeking_past_the_end_exhausts_the_cursor() {
        let texts: Vec<String> = (0..100).map(|index| format!("alpha d{index}")).collect();
        let sealed = SealedSegment::seal(&segment_of(&texts)).expect("seals");
        let mut cursor = sealed
            .cursor_at(sealed.term_span_range(b"alpha").0, 1_000)
            .expect("span");
        cursor.reset();
        cursor.seek(9_999);
        assert!(cursor.exhausted());
        assert_eq!(cursor.current(), None);
        assert_eq!(cursor.current_weighted_tf(), None);
    }

    #[test]
    fn an_empty_segment_seals_and_answers_nothing() {
        let sealed = SealedSegment::seal(&SegmentIndex::new()).expect("seals");
        assert!(sealed.is_empty());
        assert_eq!(sealed.row_count(), 0);
        assert_eq!(sealed.total_tokens(), 0);
        assert_eq!(sealed.document_length(0), 0);
        assert_eq!(sealed.field_length(0, DEFAULT_FIELD), 0);
        assert!(sealed.field_lengths(DEFAULT_FIELD).is_none());
        assert_eq!(sealed.document_frequency(b"alpha", &[DEFAULT_FIELD]), 0);
        assert!(
            TermStream::open(&sealed, b"alpha", &FieldWeights::flat(&[DEFAULT_FIELD])).is_none()
        );
    }

    #[test]
    fn a_zero_weight_field_contributes_no_run() {
        let texts: Vec<String> = (0..4).map(|index| format!("alpha d{index}")).collect();
        let sealed = SealedSegment::seal(&segment_of(&texts)).expect("seals");
        let weights = FieldWeights::new(&[(DEFAULT_FIELD, 0)]);
        assert!(TermStream::open(&sealed, b"alpha", &weights).is_none());
    }

    #[test]
    fn a_bound_over_one_field_is_the_stored_impact_pair() {
        let texts: Vec<String> = (0..300).map(|index| format!("alpha d{index}")).collect();
        let active = segment_of(&texts);
        let sealed = SealedSegment::seal(&active).expect("seals");
        let stats = crate::fts::bm25::CorpusStats::new(300, 600).expect("stats");
        let scorer = TermScorer::new(
            crate::fts::bm25::Df(300),
            &stats,
            crate::fts::bm25::Bm25Params::default(),
        );
        let stream = TermStream::open(&sealed, b"alpha", &FieldWeights::flat(&[DEFAULT_FIELD]))
            .expect("stream");
        let impact = stream.runs[0].current_impact();
        assert!((stream.block_bound(&scorer) - impact.bound(&scorer)).abs() < 1e-15);
    }

    #[test]
    fn a_traversed_list_costs_a_couple_of_bytes_per_posting() {
        // The contamination-immune half of the decode claim. An in-memory
        // `Posting` is 32 bytes of struct plus a heap allocation for its
        // positions, and scoring reads 8 of those 32 with a 32-byte stride.
        // This pins what the sealed docid and term-frequency streams cost
        // instead, for the lists a query actually walks.
        //
        // Deterministic: the corpus is generated, the bit widths follow
        // from the gaps, and no clock is involved.
        //
        // Singleton lists are measured separately and deliberately. A term
        // occurring once still carries a whole 32-byte metadata row, so a
        // Zipf tail of hapax terms dominates any whole-index average — but
        // no query traverses those lists, and averaging them in would hide
        // the number that decides query speed behind one that does not.
        let texts: Vec<String> = (0..20_000)
            .map(|index| format!("alpha t{} filler{index}", index % 97))
            .collect();
        let sealed = SealedSegment::seal(&segment_of(&texts)).expect("seals");

        let mut long_postings = 0_usize;
        let mut long_bytes = 0_usize;
        let mut short_lists = 0_usize;
        for index in 0..sealed.spans.len() {
            let Some(span) = sealed.spans.get(index).copied() else {
                continue;
            };
            let cursor = sealed.cursor_at(index, 1_000).expect("span");
            if cursor.block_count <= 1 {
                short_lists = short_lists.saturating_add(1);
                continue;
            }
            long_postings =
                long_postings.saturating_add(usize::try_from(span.doc_freq).unwrap_or(0));
            // The docid stream, the term-frequency stream, and the metadata
            // rows they are addressed through: everything the BM25 path
            // touches, and nothing it does not. Positions are excluded
            // because scoring never decodes them.
            let last = cursor.block_count.saturating_sub(1);
            let meta = cursor.meta_at(last).expect("last block");
            let tf_end = usize::try_from(meta.tfs_offset).unwrap_or(0)
                + (usize::from(meta.count) * usize::from(meta.tf_bits)).div_ceil(8);
            long_bytes = long_bytes
                .saturating_add(usize::try_from(span.tfs_start - span.docids_start).unwrap_or(0))
                .saturating_add(tf_end)
                .saturating_add(cursor.block_count.saturating_mul(BLOCK_META_LEN));
        }

        let per_posting = long_bytes as f64 / long_postings as f64;
        println!(
            "SEALED DENSITY traversed_postings={long_postings} bytes={long_bytes} \
             per_posting={per_posting:.3} single_block_lists={short_lists}"
        );
        assert!(
            per_posting < 4.0,
            "a traversed list costs {per_posting:.3} bytes per posting; the \
             in-memory form cost a 32-byte stride plus an allocation, and a \
             regression here is a decode regression the wall clock would only \
             confirm later"
        );
    }

    #[test]
    fn every_decoded_width_falls_in_the_narrow_kernel_path() {
        // R5 of the roofline protocol, as a standing gate rather than a
        // number in a comment. The decode kernels have three arms and only
        // one of them is worth tuning, so which widths actually occur is a
        // load-bearing fact. It is a counter, so it is safe to measure on a
        // contended machine and it carries a zero flake budget.
        //
        // It also closes K3, the width-above-25 scalar cliff, by
        // measurement: nothing comes near it. That is a result, not a gap.
        for (label, texts) in [
            (
                "zipf100k",
                (0..100_000)
                    .map(|index| {
                        let mut words: Vec<String> = Vec::new();
                        for term in 0..12_usize {
                            if index % (term + 1) == 0 {
                                words.push(format!("t{term}"));
                            }
                        }
                        if words.is_empty() {
                            words.push(String::from("filler"));
                        }
                        words.join(" ")
                    })
                    .collect::<Vec<String>>(),
            ),
            (
                "textish",
                (0..50_000)
                    .map(|index| format!("alpha t{} u{} filler{index}", index % 97, index % 1_009))
                    .collect::<Vec<String>>(),
            ),
        ] {
            let sealed = SealedSegment::seal(&segment_of(&texts)).expect("seals");
            let mut histogram = [0_u64; 33];
            let mut postings = 0_u64;
            let mut widest = 0_u8;
            for index in 0..sealed.spans.len() {
                let cursor = sealed.cursor_at(index, 1_000).expect("span");
                // Only the lists a query traverses; a singleton list is one
                // block nobody walks.
                if cursor.block_count <= 1 {
                    continue;
                }
                for block in 0..cursor.block_count {
                    let meta = cursor.meta_at(block).expect("meta");
                    postings = postings.saturating_add(u64::from(meta.count));
                    for bits in [meta.docid_bits, meta.tf_bits] {
                        widest = widest.max(bits);
                        if let Some(slot) = histogram.get_mut(usize::from(bits)) {
                            *slot = slot.saturating_add(u64::from(meta.count));
                        }
                    }
                }
            }
            let mut shape: Vec<String> = Vec::new();
            for (bits, count) in histogram.iter().enumerate() {
                if *count > 0 {
                    shape.push(format!(
                        "{bits}b={:.1}%",
                        *count as f64 * 50.0 / postings as f64
                    ));
                }
            }
            println!(
                "WIDTHS {label} postings={postings} widest={widest} {}",
                shape.join(" ")
            );
            assert!(postings > 0, "{label} produced no traversable list");
            assert!(
                widest <= crate::kernels::postings::NARROW_MAX_BITS,
                "{label} decodes at {widest} bits, outside the narrow kernel \
                 path the histogram was used to justify. Re-measure the \
                 histogram and re-decide which arm to tune; do not widen \
                 this bound."
            );
        }
    }

    #[test]
    fn a_list_whose_lengths_are_absent_still_seals() {
        // `block_impacts` falls back to the shortest possible length, so a
        // field with no recorded lengths yields a sound rather than absent
        // pair.
        let mut list = PostingList::new();
        list.push(Posting {
            docid: 0,
            tf: 2,
            positions: vec![0, 1],
        })
        .expect("ascending");
        let impacts = crate::fts::postings::block_impacts(&list, 64, &[]);
        assert_eq!(impacts[0].max_tf, 2);
        assert_eq!(impacts[0].min_len, 1);
    }
}
