use super::CatalogError;
use super::work;
use crate::epoch::{ComputeUnits, EmbeddingRuntime, EmbeddingTower, Normalization};
use crate::fts::tokenizer::TokenizerEpoch;
use crate::property_graph::{CanonicalEmbedding, MAX_GRAPH_INPUT_BYTES, StoreInstanceId};

/// Complete borrowed document interpretation. Query towers/alignment are absent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DocumentDeclaration<'a> {
    pub(super) model_id: &'a str,
    pub(super) model_version: &'a str,
    pub(super) weights_digest: &'a [u8],
    pub(super) dims: u32,
    pub(super) normalization: Normalization,
    pub(super) prompt_prefix: &'a str,
    pub(super) max_tokens: u32,
    pub(super) runtime: EmbeddingRuntime,
    pub(super) compute_units: ComputeUnits,
    pub(super) os_build: Option<&'a str>,
}
impl<'a> DocumentDeclaration<'a> {
    /// Borrows every existing tower field without copying or reducing it to a hash.
    pub fn new(tower: &'a EmbeddingTower) -> Result<Self, CatalogError> {
        let value = Self {
            model_id: &tower.model_id,
            model_version: &tower.model_version,
            weights_digest: &tower.weights_digest,
            dims: tower.dims,
            normalization: tower.normalization,
            prompt_prefix: &tower.prompt_prefix,
            max_tokens: tower.max_tokens,
            runtime: tower.runtime,
            compute_units: tower.compute_units,
            os_build: tower.os_build.as_deref(),
        };
        value.validate()?;
        Ok(value)
    }
    pub(crate) fn to_tower(self) -> EmbeddingTower {
        EmbeddingTower {
            model_id: self.model_id.to_owned(),
            model_version: self.model_version.to_owned(),
            weights_digest: self.weights_digest.to_vec(),
            dims: self.dims,
            normalization: self.normalization,
            prompt_prefix: self.prompt_prefix.to_owned(),
            max_tokens: self.max_tokens,
            runtime: self.runtime,
            compute_units: self.compute_units,
            os_build: self.os_build.map(str::to_owned),
        }
    }
    /// Returns declared coordinate count, independent of live vector membership.
    pub const fn dimensions(self) -> u32 {
        self.dims
    }
    /// Refuses a declaration no native vector index could ever carry. The Bit4
    /// path is bounded by `kernels::MAX_DOT_I8_DIMENSION`, so an over-wide
    /// document space is rejected here, at admission, instead of surviving into
    /// storage and failing the first write with a corruption-class quantizer
    /// error (ZE-167, amending ZE-60 against ZE-158's index-per-source rule).
    pub(super) fn validate(self) -> Result<(), CatalogError> {
        let mut bytes = if self.os_build.is_some() {
            55_usize
        } else {
            47_usize
        };
        for length in [
            self.model_id.len(),
            self.model_version.len(),
            self.weights_digest.len(),
            self.prompt_prefix.len(),
            self.os_build.map_or(0, str::len),
        ] {
            bytes = bytes
                .checked_add(length)
                .ok_or(CatalogError::InvalidEmbedding)?;
        }
        if self.dims == 0
            || u64::from(self.dims) > crate::kernels::MAX_DOT_I8_DIMENSION as u64
            || u64::from(self.dims) * 4 > MAX_GRAPH_INPUT_BYTES as u64
            || bytes > MAX_GRAPH_INPUT_BYTES
        {
            return Err(CatalogError::InvalidEmbedding);
        }
        Ok(())
    }
    fn matches(self, other: Self, checkpoint: work::Checkpoint<'_>) -> Result<bool, CatalogError> {
        checkpoint()?;
        if self.dims != other.dims
            || self.normalization != other.normalization
            || self.max_tokens != other.max_tokens
            || self.runtime != other.runtime
            || self.compute_units != other.compute_units
            || self.os_build.is_some() != other.os_build.is_some()
        {
            return Ok(false);
        }
        for (left, right) in [
            (self.model_id.as_bytes(), other.model_id.as_bytes()),
            (
                self.model_version.as_bytes(),
                other.model_version.as_bytes(),
            ),
            (self.weights_digest, other.weights_digest),
            (
                self.prompt_prefix.as_bytes(),
                other.prompt_prefix.as_bytes(),
            ),
            (
                self.os_build.unwrap_or("").as_bytes(),
                other.os_build.unwrap_or("").as_bytes(),
            ),
        ] {
            if work::compare_bytes(left, right, checkpoint)? != std::cmp::Ordering::Equal {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Required graph interpretation: one lexical analyzer and zero or one document space.
/// It imposes neither a timestamp schema nor mandatory text/vector membership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphInterpretation<'a> {
    pub(super) lexical: TokenizerEpoch,
    pub(super) embedding: Option<DocumentDeclaration<'a>>,
}
impl<'a> GraphInterpretation<'a> {
    /// Validates the optional document declaration, borrowing its complete fields.
    pub fn new(
        lexical: TokenizerEpoch,
        document: Option<&'a EmbeddingTower>,
    ) -> Result<Self, CatalogError> {
        Ok(Self {
            lexical,
            embedding: document.map(DocumentDeclaration::new).transpose()?,
        })
    }
    /// Returns the analyzer's existing versioned identity.
    pub const fn lexical(self) -> TokenizerEpoch {
        self.lexical
    }
    /// Returns the sole optional space; no epoch alias controls graph existence.
    pub const fn embedding(self) -> Option<DocumentDeclaration<'a>> {
        self.embedding
    }
    /// Pure compatibility check used before the later coordinator admits replay or
    /// cleanup. Success validates only interpretation, not a whole-store checkpoint.
    pub fn validate_for(
        self,
        declared: GraphInterpretation<'_>,
        checkpoint: &mut dyn FnMut() -> Result<(), CatalogError>,
    ) -> Result<(), CatalogError> {
        checkpoint()?;
        if self.lexical != declared.lexical
            || self.embedding.is_some() != declared.embedding.is_some()
        {
            return Err(CatalogError::InterpretationMismatch);
        }
        if let Some((left, right)) = self.embedding.zip(declared.embedding)
            && !left.matches(right, checkpoint)?
        {
            return Err(CatalogError::InterpretationMismatch);
        }
        Ok(())
    }
    /// Validates optional supplied vector contents against the stored document space.
    /// Absence is always legal, including in a store with a declared space.
    pub fn validate_payload(
        self,
        embedding: Option<CanonicalEmbedding<'_>>,
        checkpoint: &mut dyn FnMut() -> Result<(), CatalogError>,
    ) -> Result<(), CatalogError> {
        checkpoint()?;
        if let Some(embedding) = embedding {
            let expected = self.embedding.ok_or(CatalogError::NoEmbeddingSpace)?;
            if !expected.matches(DocumentDeclaration::new(embedding.document())?, checkpoint)? {
                return Err(CatalogError::InterpretationMismatch);
            }
        }
        Ok(())
    }
}

/// Logical declaration retained by a catalog checkpoint participant. The write
/// coordinator owns coherent generations, WAL cutoffs and allocator publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CatalogDeclaration<'a> {
    /// Existing storage-owned identity; decoding never obtains replacement entropy.
    pub store: StoreInstanceId,
    /// Inclusive full-width node allocator high-water, zero for a fresh store.
    pub node_high_water: u128,
    /// Inclusive full-width relationship allocator high-water.
    pub relationship_high_water: u128,
    /// Exact graph-mode interpretation.
    pub interpretation: GraphInterpretation<'a>,
}
impl CatalogDeclaration<'_> {
    /// Refuses wrong-store or incompatible declarations without touching any files.
    pub fn validate_for(
        self,
        expected_store: StoreInstanceId,
        declared: GraphInterpretation<'_>,
        checkpoint: &mut dyn FnMut() -> Result<(), CatalogError>,
    ) -> Result<(), CatalogError> {
        checkpoint()?;
        if self.store != expected_store {
            return Err(CatalogError::StoreMismatch);
        }
        self.interpretation.validate_for(declared, checkpoint)
    }
}
