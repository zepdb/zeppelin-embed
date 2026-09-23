//! Bounded/accounted lexical preparation for one native graph fragment.

mod memory;

use super::control::{BuildPolicy, CapacityCharge, GuardedString, GuardedVec, sort_by_policy};
use super::postings::{OwnedPosting, Posting, PostingsError};
use super::sealed::{GraphSealed, SealedSegment, SealedSegmentError};
use super::tokenizer::{Analyzer, TokenizerEpoch, TokenizerError};
use crate::property_graph::query::resources::MemoryError;
use crate::property_graph::query::resources::QueryMemory;
use crate::property_graph::query::runtime::{RuntimeContext, RuntimeError};
use crate::property_graph::storage::memory::StorageMemory;
use crate::property_graph::storage::tree::directory::{TreeError, TreeResources};
use memory::{Charge, PreparationPolicy, QueryPolicy};

/// Typed failure of graph lexical preparation or validation.
#[derive(Debug)]
pub enum GraphLexicalError {
    /// The existing tokenizer rejected the input.
    Tokenizer(TokenizerError),
    /// The existing posting codec rejected a list.
    Postings(PostingsError),
    /// The existing sealed-region codec rejected the region.
    Region(SealedSegmentError),
    /// Authentic graph storage/query control rejected work or capacity.
    Resource(TreeError),
    /// The builder was poisoned by an earlier failed operation.
    Failed,
}

impl std::fmt::Display for GraphLexicalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tokenizer(error) => error.fmt(formatter),
            Self::Postings(error) => error.fmt(formatter),
            Self::Region(error) => error.fmt(formatter),
            Self::Resource(error) => error.fmt(formatter),
            Self::Failed => formatter.write_str("graph lexical builder already failed"),
        }
    }
}

impl std::error::Error for GraphLexicalError {}

impl From<TokenizerError> for GraphLexicalError {
    fn from(error: TokenizerError) -> Self {
        Self::Tokenizer(error)
    }
}

impl From<PostingsError> for GraphLexicalError {
    fn from(error: PostingsError) -> Self {
        Self::Postings(error)
    }
}

impl From<SealedSegmentError> for GraphLexicalError {
    fn from(error: SealedSegmentError) -> Self {
        Self::Region(error)
    }
}

impl From<TreeError> for GraphLexicalError {
    fn from(error: TreeError) -> Self {
        Self::Resource(error)
    }
}

fn map_query_memory(error: MemoryError) -> GraphLexicalError {
    GraphLexicalError::Resource(TreeError::Runtime(RuntimeError::Memory(error)))
}

struct TermEntry<'m> {
    term: GuardedString<'m, Charge<'m>>,
    postings: GuardedVec<'m, OwnedPosting<'m, Charge<'m>>, Charge<'m>>,
}

/// Incremental producer for one dense graph lexical fragment.
pub struct GraphLexicalBuilder<'a, 'm> {
    analyzer: &'a Analyzer,
    memory: &'m StorageMemory<'m>,
    epoch: TokenizerEpoch,
    terms: GuardedVec<'m, TermEntry<'m>, Charge<'m>>,
    row_lengths: GuardedVec<'m, u32, Charge<'m>>,
    total_tokens: u64,
    failed: bool,
    _charge: Charge<'m>,
}

impl<'a, 'm> GraphLexicalBuilder<'a, 'm> {
    /// Starts one fragment using the supplied analyzer and storage owner.
    ///
    /// # Errors
    /// Returns a typed resource error when the owner or initial descriptor
    /// cannot be admitted.
    pub fn new(
        analyzer: &'a Analyzer,
        memory: &'m StorageMemory<'m>,
        resources: &mut TreeResources<'_>,
    ) -> Result<Self, GraphLexicalError> {
        let mut policy = PreparationPolicy::new(memory, resources)?;
        let terms = GuardedVec::with_capacity(&mut policy, 0)?;
        let row_lengths = GuardedVec::with_capacity(&mut policy, 0)?;
        let charge = policy.owner.reserve(std::mem::size_of::<Self>())?;
        Ok(Self {
            analyzer,
            memory,
            epoch: analyzer.epoch(),
            terms,
            row_lengths,
            total_tokens: 0,
            failed: false,
            _charge: charge,
        })
    }

    /// Analyzes one document and returns its dense lexical row, or `None`
    /// when analysis produces no surviving tokens.
    ///
    /// # Errors
    /// Returns a typed tokenizer, region, or resource error. Any error makes
    /// this builder terminal.
    pub fn push_text(
        &mut self,
        text: &str,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<u32>, GraphLexicalError> {
        if self.failed {
            return Err(GraphLexicalError::Failed);
        }
        let result = self.push_text_inner(text, resources);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn push_text_inner(
        &mut self,
        text: &str,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<u32>, GraphLexicalError> {
        let mut policy = PreparationPolicy::new(self.memory, resources)?;
        let tokens = self.analyzer.analyze_with_policy(text, &mut policy)?;
        if tokens.is_empty() {
            return Ok(None);
        }
        let row = u32::try_from(self.row_lengths.len()).map_err(|_| {
            GraphLexicalError::Region(SealedSegmentError::Geometry("row count exceeds u32"))
        })?;
        let length = tokens.as_slice().iter().try_fold(0_u32, |maximum, token| {
            policy.step(1)?;
            Ok::<u32, GraphLexicalError>(maximum.max(token.position().saturating_add(1)))
        })?;
        if length == 0 {
            return Err(GraphLexicalError::Region(SealedSegmentError::Geometry(
                "graph lexical row length is zero",
            )));
        }

        for index in 0..tokens.len() {
            policy.step(1)?;
            let Some(token) = tokens.get(index) else {
                continue;
            };
            let mut seen = false;
            for prior in tokens.as_slice().iter().take(index) {
                if compare_text(prior.term(), token.term(), &mut policy)?
                    == std::cmp::Ordering::Equal
                {
                    seen = true;
                    break;
                }
            }
            if seen {
                continue;
            }
            let mut count = 0_usize;
            for candidate in tokens.as_slice() {
                if compare_text(candidate.term(), token.term(), &mut policy)?
                    == std::cmp::Ordering::Equal
                {
                    count = count.saturating_add(1);
                }
            }
            let mut positions = GuardedVec::with_capacity(&mut policy, count)?;
            for candidate in tokens.as_slice() {
                if compare_text(candidate.term(), token.term(), &mut policy)?
                    == std::cmp::Ordering::Equal
                {
                    positions.push(candidate.position(), &mut policy)?;
                }
            }
            let existing = find_term(self.terms.as_slice(), token.term(), &mut policy)?;
            let term_index = if let Some(found) = existing {
                found
            } else {
                let term = GuardedString::copy_from(&mut policy, token.term())?;
                let postings = GuardedVec::with_capacity(&mut policy, 0)?;
                self.terms.push(TermEntry { term, postings }, &mut policy)?;
                self.terms.len().saturating_sub(1)
            };
            let (positions, positions_charge) = positions.into_parts();
            let posting = OwnedPosting {
                posting: Posting {
                    docid: row,
                    tf: u32::try_from(positions.len()).unwrap_or(u32::MAX),
                    positions,
                },
                _positions_charge: positions_charge,
                marker: std::marker::PhantomData,
            };
            let Some(entry) = self.terms.get_mut(term_index) else {
                return Err(GraphLexicalError::Region(SealedSegmentError::Geometry(
                    "graph term disappeared",
                )));
            };
            entry.postings.push(posting, &mut policy)?;
        }
        self.row_lengths.push(length, &mut policy)?;
        self.total_tokens =
            self.total_tokens
                .checked_add(u64::from(length))
                .ok_or(GraphLexicalError::Region(SealedSegmentError::Geometry(
                    "total token count overflow",
                )))?;
        policy.checkpoint()?;
        Ok(Some(row))
    }

    /// Seals the accumulated rows into the frozen lexical region layout.
    ///
    /// # Errors
    /// Returns a typed codec or resource error, or [`GraphLexicalError::Failed`]
    /// after an earlier failed push.
    pub fn finish(
        mut self,
        resources: &mut TreeResources<'_>,
    ) -> Result<PreparedGraphLexical<'m>, GraphLexicalError> {
        if self.failed {
            return Err(GraphLexicalError::Failed);
        }
        let mut policy = PreparationPolicy::new(self.memory, resources)?;
        sort_by_policy(
            self.terms.as_mut_slice(),
            &mut policy,
            |left, right, policy| compare_text(left.term.as_str(), right.term.as_str(), policy),
        )?;
        let sealed = GraphSealed::seal_policy(
            self.row_lengths.as_slice(),
            self.terms
                .as_slice()
                .iter()
                .map(|entry| (entry.term.as_str(), entry.postings.as_slice())),
            &mut policy,
        )?;
        let region = sealed.encode_region_policy(&mut policy)?;
        let decoded_charge = policy
            .owner
            .reserve(std::mem::size_of::<DecodedGraphLexical<'m>>())?;
        let prepared_charge = policy
            .owner
            .reserve(std::mem::size_of::<PreparedGraphLexical<'m>>())?;
        let decoded = DecodedGraphLexical {
            sealed,
            epoch: self.epoch,
            total_tokens: self.total_tokens,
            _charge: decoded_charge,
        };
        let (region, region_charge) = region.into_parts();
        Ok(PreparedGraphLexical {
            region,
            decoded,
            region_charge,
            _charge: prepared_charge,
        })
    }
}

fn find_term<'m, P: BuildPolicy<'m>>(
    terms: &[TermEntry<'m>],
    needle: &str,
    policy: &mut P,
) -> Result<Option<usize>, P::Error> {
    for (index, entry) in terms.iter().enumerate() {
        policy.step(1)?;
        if compare_text(entry.term.as_str(), needle, policy)? == std::cmp::Ordering::Equal {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

fn compare_text<'m, P: BuildPolicy<'m>>(
    left: &str,
    right: &str,
    policy: &mut P,
) -> Result<std::cmp::Ordering, P::Error> {
    for (left, right) in left.as_bytes().iter().zip(right.as_bytes()) {
        policy.step(1)?;
        let order = left.cmp(right);
        if order != std::cmp::Ordering::Equal {
            return Ok(order);
        }
    }
    Ok(left.len().cmp(&right.len()))
}

/// One encoded graph lexical region paired with its validated decoded view.
pub struct PreparedGraphLexical<'m> {
    region: Vec<u8>,
    decoded: DecodedGraphLexical<'m>,
    region_charge: Charge<'m>,
    _charge: Charge<'m>,
}

impl<'m> PreparedGraphLexical<'m> {
    /// Returns the encoded frozen region bytes.
    pub fn region(&self) -> &[u8] {
        &self.region
    }
    /// Returns the decoded view retained by this prepared owner.
    pub const fn decoded(&self) -> &DecodedGraphLexical<'m> {
        &self.decoded
    }
    /// Returns the exact retained descriptor and heap-capacity charge.
    pub fn owned_bytes(&self) -> usize {
        self.region_charge
            .bytes()
            .saturating_add(self.decoded.owned_bytes())
            .saturating_add(self._charge.bytes())
    }
}

/// Query text analyzed by the store's analyzer under the exact query memory.
///
/// The term sequence is the analyzer's token order, duplicates included,
/// exactly as a flat `TermQuery` built from `Analyzer::analyze`.
pub(crate) struct GraphQueryTerms<'m> {
    tokens: GuardedVec<'m, super::tokenizer::ControlledToken<'m, Charge<'m>>, Charge<'m>>,
}

impl GraphQueryTerms<'_> {
    pub(crate) fn len(&self) -> usize {
        self.tokens.len()
    }

    pub(crate) fn term(&self, index: usize) -> Option<&str> {
        self.tokens.get(index).map(|token| token.term())
    }
}

/// Analyzes one lexical query argument with bounded, accounted work.
///
/// # Errors
/// Returns a typed tokenizer, lifecycle, work, or query-memory error.
pub(crate) fn analyze_query<'m>(
    analyzer: &Analyzer,
    text: &str,
    memory: &'m QueryMemory<'m>,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<GraphQueryTerms<'m>, GraphLexicalError> {
    let mut policy = QueryPolicy::new(memory, context)?;
    let tokens = analyzer.analyze_with_policy(text, &mut policy)?;
    policy.checkpoint()?;
    Ok(GraphQueryTerms { tokens })
}

/// A validated graph lexical region with all retained capacity accounted.
pub struct DecodedGraphLexical<'m> {
    sealed: GraphSealed<'m, Charge<'m>>,
    epoch: TokenizerEpoch,
    total_tokens: u64,
    _charge: Charge<'m>,
}

impl<'m> DecodedGraphLexical<'m> {
    /// Decodes under a storage-preparation owner.
    ///
    /// The epoch belongs to the already-validated enclosing descriptor; the
    /// bare region does not authenticate it.
    ///
    /// # Errors
    /// Returns a typed codec or storage resource error.
    pub fn decode_prepare(
        bytes: &[u8],
        epoch: TokenizerEpoch,
        memory: &'m StorageMemory<'m>,
        resources: &mut TreeResources<'_>,
    ) -> Result<Self, GraphLexicalError> {
        let mut policy = PreparationPolicy::new(memory, resources)?;
        let (sealed, total_tokens) = GraphSealed::decode_policy(bytes, &mut policy)?;
        let charge = policy.owner.reserve(std::mem::size_of::<Self>())?;
        Ok(Self {
            sealed,
            epoch,
            total_tokens,
            _charge: charge,
        })
    }

    /// Decodes under the exact query memory and runtime owner.
    ///
    /// The epoch belongs to the already-validated enclosing descriptor; the
    /// bare region does not authenticate it.
    ///
    /// # Errors
    /// Returns a typed codec, lifecycle, work, or query-memory error.
    pub fn decode_query(
        bytes: &[u8],
        epoch: TokenizerEpoch,
        memory: &'m QueryMemory<'m>,
        context: &mut RuntimeContext<'_, '_, '_>,
    ) -> Result<Self, GraphLexicalError> {
        let mut policy = QueryPolicy::new(memory, context)?;
        let (sealed, total_tokens) = GraphSealed::decode_policy(bytes, &mut policy)?;
        let charge = policy.owner.reserve(std::mem::size_of::<Self>())?;
        Ok(Self {
            sealed,
            epoch,
            total_tokens,
            _charge: charge,
        })
    }

    /// Returns the enclosing descriptor's tokenizer epoch.
    pub const fn epoch(&self) -> TokenizerEpoch {
        self.epoch
    }
    /// Returns the dense lexical row count.
    pub const fn row_count(&self) -> u32 {
        self.sealed.sealed().row_count()
    }
    /// Returns max-position-plus-one for every dense row.
    pub fn row_lengths(&self) -> &[u32] {
        self.sealed
            .sealed()
            .field_lengths(super::index::DEFAULT_FIELD)
            .unwrap_or(&[])
    }
    /// Returns the checked sum of row lengths.
    pub const fn total_tokens(&self) -> u64 {
        self.total_tokens
    }
    /// Returns the validated existing FTS representation.
    pub const fn sealed(&self) -> &SealedSegment {
        self.sealed.sealed()
    }
    /// Returns the exact retained descriptor and heap-capacity charge.
    pub fn owned_bytes(&self) -> usize {
        self.sealed
            .owned_bytes()
            .saturating_add(self._charge.bytes())
    }
}

#[cfg(test)]
mod tests;
