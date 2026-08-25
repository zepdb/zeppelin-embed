//! Epoch identity and migration.

use crate::fts::tokenizer::TokenizerEpoch;
use crate::manifest::EpochMeta;

/// Stable digest of every field that determines embedding interpretation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EpochId(u64);

/// Output-vector normalization applied by the embedding pipeline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum Normalization {
    /// Leave the model output unchanged.
    None = 0,
    /// Normalize the model output to unit L2 length.
    L2 = 1,
}

/// Inference runtime whose numerical behaviour produced the embedding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum EmbeddingRuntime {
    /// Apple's Core ML runtime.
    CoreMl = 1,
    /// Apple's MLX runtime.
    Mlx = 2,
    /// A host CPU reference implementation.
    CpuReference = 3,
}

/// Hardware classes the inference runtime may use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum ComputeUnits {
    /// CPU execution only.
    Cpu = 1,
    /// CPU and GPU execution.
    CpuAndGpu = 2,
    /// CPU and Neural Engine execution.
    CpuAndNeuralEngine = 3,
    /// Every compute unit available to the runtime.
    All = 4,
}

/// Complete immutable identity of one encoder tower.
///
/// Every field changes the meaning of produced vectors. Mutable store state
/// such as generations, row counts, timestamps, and document counts is
/// deliberately absent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmbeddingTower {
    /// Host-selected model identifier.
    pub model_id: String,
    /// Host-selected model version.
    pub model_version: String,
    /// Opaque digest of the exact model weights.
    pub weights_digest: Vec<u8>,
    /// Output vector dimensions.
    pub dims: u32,
    /// Output normalization behaviour.
    pub normalization: Normalization,
    /// Exact prompt or prefix applied before inference, empty for none.
    pub prompt_prefix: String,
    /// Maximum input token count.
    pub max_tokens: u32,
    /// Inference runtime.
    pub runtime: EmbeddingRuntime,
    /// Compute-unit selection.
    pub compute_units: ComputeUnits,
    /// Optional operating-system build that affects inference behaviour.
    pub os_build: Option<String>,
}

impl EmbeddingTower {
    /// Returns a human-readable model label for diagnostics.
    #[must_use]
    pub fn model_label(&self) -> String {
        format!("{}@{}", self.model_id, self.model_version)
    }
}

/// Complete immutable identity of one embedding interpretation.
///
/// The document and query towers are explicit even when they are identical.
/// This makes an aligned asymmetric pair one epoch instead of two unrelated
/// model declarations. The alignment digest identifies the artifact or recipe
/// that places both tower outputs in the same vector space.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmbeddingEpoch {
    /// Encoder used for persisted document vectors.
    pub document: EmbeddingTower,
    /// Encoder used for query vectors compared with those documents.
    pub query: EmbeddingTower,
    /// Opaque digest of the pairing/alignment artifact, empty for none.
    pub alignment_digest: Vec<u8>,
}

/// Digest-input framing magic. Changing it changes every embedding epoch.
const EPOCH_MAGIC: &[u8; 8] = b"ZEEMBEP2";

impl EpochId {
    /// Computes the identity of an embedding interpretation.
    ///
    /// # The canonical digest input
    ///
    /// The input is hand-written and little-endian throughout. Its exact
    /// persisted-meaning order is: the eight bytes `ZEEMBEP2`; document-tower
    /// fields; query-tower fields; then a u32 byte length and opaque
    /// alignment-digest bytes. Each tower is a u32 byte length plus
    /// UTF-8 model id; a u32 byte length plus UTF-8 model version; a u32 byte
    /// length plus opaque weights-digest bytes; u32 dimensions; the u16
    /// normalization id; a u32 byte length plus UTF-8 prompt prefix; u32
    /// maximum tokens; the u16 runtime id; the u16 compute-units id; then a
    /// one-byte OS-build presence tag (`0` or `1`) followed, when present, by
    /// its u32 byte length and UTF-8 bytes. No mutable store state is ever part
    /// of this input.
    #[must_use]
    pub fn of(epoch: &EmbeddingEpoch) -> Self {
        Self(xxhash_rust::xxh3::xxh3_64(&canonical_digest_input(epoch)))
    }

    /// Returns the raw digest.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }

    pub(crate) const fn from_value(value: u64) -> Self {
        Self(value)
    }

    /// Returns the lowercase hex form used for diagnostics.
    #[must_use]
    pub fn to_hex(self) -> String {
        format!("{:016x}", self.0)
    }
}

impl std::fmt::Display for EpochId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:016x}", self.0)
    }
}

/// Caller-declared embedding and tokenizer interpretation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreEpoch {
    /// Embedding-model interpretation.
    pub embedding: EmbeddingEpoch,
    /// Tokenizer interpretation.
    pub tokenizer: TokenizerEpoch,
}

impl StoreEpoch {
    /// Returns the compact identity pair compared at admission points.
    #[must_use]
    pub fn identity(&self) -> EpochIdentity {
        EpochIdentity {
            embedding: EpochId::of(&self.embedding),
            tokenizer: self.tokenizer,
        }
    }
}

/// Compact persisted and reported store interpretation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EpochIdentity {
    /// Embedding identity digest.
    pub embedding: EpochId,
    /// Tokenizer identity digest.
    pub tokenizer: TokenizerEpoch,
}

impl EpochIdentity {
    /// Reconstructs an identity from one validated manifest registry entry.
    pub fn from_meta(meta: &EpochMeta) -> Result<Self, EpochMetaError> {
        let actual = EpochId::of(&meta.embedding);
        if actual != meta.id {
            return Err(EpochMetaError::EmbeddingDigestMismatch {
                declared: meta.id,
                actual,
            });
        }
        Ok(Self {
            embedding: meta.id,
            tokenizer: meta.tokenizer,
        })
    }
}

impl From<&StoreEpoch> for EpochMeta {
    fn from(epoch: &StoreEpoch) -> Self {
        Self {
            id: epoch.identity().embedding,
            embedding: epoch.embedding.clone(),
            tokenizer: epoch.tokenizer,
        }
    }
}

impl From<StoreEpoch> for EpochMeta {
    fn from(epoch: StoreEpoch) -> Self {
        Self::from(&epoch)
    }
}

/// Malformed persisted epoch metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EpochMetaError {
    /// The stored id did not identify the stored embedding description.
    EmbeddingDigestMismatch {
        /// Id carried by the registry entry.
        declared: EpochId,
        /// Id recomputed from the complete stored embedding epoch.
        actual: EpochId,
    },
}

impl std::fmt::Display for EpochMetaError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmbeddingDigestMismatch { declared, actual } => write!(
                formatter,
                "persisted embedding epoch id {declared} does not match complete identity {actual}"
            ),
        }
    }
}

impl std::error::Error for EpochMetaError {}

/// A store and caller named different interpretation identities.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EpochMismatch {
    /// Identity pinned by the store.
    pub expected: EpochIdentity,
    /// Identity declared by the caller.
    pub declared: EpochIdentity,
}

impl std::fmt::Display for EpochMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (
            self.expected.embedding != self.declared.embedding,
            self.expected.tokenizer != self.declared.tokenizer,
        ) {
            (true, true) => write!(
                formatter,
                "embedding and tokenizer epochs differ: expected ({}, {}), declared ({}, {})",
                self.expected.embedding,
                self.expected.tokenizer,
                self.declared.embedding,
                self.declared.tokenizer
            ),
            (true, false) => write!(
                formatter,
                "embedding epoch differs: expected {}, declared {}",
                self.expected.embedding, self.declared.embedding
            ),
            (false, true) => write!(
                formatter,
                "tokenizer epoch differs: expected {}, declared {}",
                self.expected.tokenizer, self.declared.tokenizer
            ),
            (false, false) => formatter.write_str("embedding and tokenizer epochs match"),
        }
    }
}

impl std::error::Error for EpochMismatch {}

fn canonical_digest_input(epoch: &EmbeddingEpoch) -> Vec<u8> {
    let mut input = Vec::with_capacity(256);
    input.extend_from_slice(EPOCH_MAGIC);
    push_tower(&mut input, &epoch.document);
    push_tower(&mut input, &epoch.query);
    push_bytes(&mut input, &epoch.alignment_digest);
    input
}

fn push_tower(input: &mut Vec<u8>, tower: &EmbeddingTower) {
    push_bytes(input, tower.model_id.as_bytes());
    push_bytes(input, tower.model_version.as_bytes());
    push_bytes(input, &tower.weights_digest);
    push_u32(input, tower.dims);
    push_u16(input, tower.normalization as u16);
    push_bytes(input, tower.prompt_prefix.as_bytes());
    push_u32(input, tower.max_tokens);
    push_u16(input, tower.runtime as u16);
    push_u16(input, tower.compute_units as u16);
    match &tower.os_build {
        Some(build) => {
            input.push(1);
            push_bytes(input, build.as_bytes());
        }
        None => input.push(0),
    }
}

fn push_bytes(buffer: &mut Vec<u8>, value: &[u8]) {
    push_u32(buffer, u32::try_from(value.len()).unwrap_or(u32::MAX));
    buffer.extend_from_slice(value);
}

fn push_u16(buffer: &mut Vec<u8>, value: u16) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(buffer: &mut Vec<u8>, value: u32) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::manifest::EpochMeta;

    fn tower(model_id: &str, prompt_prefix: &str, weight: u8) -> EmbeddingTower {
        EmbeddingTower {
            model_id: model_id.to_owned(),
            model_version: "1.2.3".to_owned(),
            weights_digest: vec![0x10, 0x20, 0x30, weight],
            dims: 384,
            normalization: Normalization::L2,
            prompt_prefix: prompt_prefix.to_owned(),
            max_tokens: 512,
            runtime: EmbeddingRuntime::CoreMl,
            compute_units: ComputeUnits::CpuAndNeuralEngine,
            os_build: Some("25A100".to_owned()),
        }
    }

    fn baseline() -> EmbeddingEpoch {
        EmbeddingEpoch {
            document: tower("document-embedding", "search_document: ", 0x40),
            query: tower("query-embedding", "search_query: ", 0x41),
            alignment_digest: vec![0xaa, 0xbb, 0xcc],
        }
    }

    fn tower_variants(base: &EmbeddingTower) -> Vec<EmbeddingTower> {
        let mut variants = Vec::new();
        let mut changed = base.clone();
        changed.model_id = "changed-model".to_owned();
        variants.push(changed);

        let mut changed = base.clone();
        changed.model_version = "9.9.9".to_owned();
        variants.push(changed);

        let mut changed = base.clone();
        changed.weights_digest = vec![0xff];
        variants.push(changed);

        let mut changed = base.clone();
        changed.dims = 768;
        variants.push(changed);

        let mut changed = base.clone();
        changed.normalization = Normalization::None;
        variants.push(changed);

        let mut changed = base.clone();
        changed.prompt_prefix = "changed: ".to_owned();
        variants.push(changed);

        let mut changed = base.clone();
        changed.max_tokens = 1_024;
        variants.push(changed);

        let mut changed = base.clone();
        changed.runtime = EmbeddingRuntime::Mlx;
        variants.push(changed);

        let mut changed = base.clone();
        changed.compute_units = ComputeUnits::All;
        variants.push(changed);

        let mut changed = base.clone();
        changed.os_build = None;
        variants.push(changed);
        variants
    }

    #[test]
    fn changing_any_interpretation_critical_field_changes_the_epoch_id() {
        let base = baseline();
        let mut variants = Vec::new();
        for tower in tower_variants(&base.document) {
            let mut changed = base.clone();
            changed.document = tower;
            variants.push(changed);
        }
        for tower in tower_variants(&base.query) {
            let mut changed = base.clone();
            changed.query = tower;
            variants.push(changed);
        }
        let mut changed = base.clone();
        changed.alignment_digest = vec![0xaa, 0xbb, 0xcd];
        variants.push(changed);

        let baseline_id = EpochId::of(&base);
        let mut ids = BTreeSet::from([baseline_id]);
        for variant in &variants {
            let id = EpochId::of(variant);
            assert_ne!(id, baseline_id);
            ids.insert(id);
        }
        assert_eq!(ids.len(), variants.len() + 1);
    }

    #[test]
    fn mutable_state_never_changes_the_epoch_id() {
        struct MutableState {
            document_count: u64,
            generation: u64,
            row_count: u64,
            timestamp: u64,
        }

        let epoch = baseline();
        let before = EpochId::of(&epoch);
        let mut state = MutableState {
            document_count: 0,
            generation: 0,
            row_count: 0,
            timestamp: 0,
        };
        state.document_count = 9_001;
        state.generation = 73;
        state.row_count = 8_999;
        state.timestamp = 1_800_000_000;

        assert_eq!(EpochId::of(&epoch), before);
        assert_eq!(
            state.document_count + state.generation + state.row_count + state.timestamp,
            1_800_018_073
        );
    }

    #[test]
    fn the_embedding_epoch_digest_input_is_byte_exact() {
        assert_eq!(
            render_hex(&canonical_digest_input(&baseline())),
            include_str!("../tests/fixtures/format/embedding_epoch_digest_input_v2.hex")
        );
    }

    #[test]
    fn a_persisted_embedding_digest_mismatch_is_a_typed_error() {
        let embedding = baseline();
        let actual = EpochId::of(&embedding);
        let error = EpochIdentity::from_meta(&EpochMeta {
            id: EpochId::from_value(actual.value() ^ 1),
            embedding,
            tokenizer: crate::fts::tokenizer::TokenizerConfig::text_default().epoch(),
        })
        .expect_err("mismatched embedding digest must fail");

        assert!(matches!(
            error,
            EpochMetaError::EmbeddingDigestMismatch { .. }
        ));
    }

    fn render_hex(bytes: &[u8]) -> String {
        let mut rendered = String::new();
        for (index, byte) in bytes.iter().enumerate() {
            if index > 0 && index % 16 == 0 {
                rendered.push('\n');
            } else if index > 0 {
                rendered.push(' ');
            }
            rendered.push_str(&format!("{byte:02x}"));
        }
        rendered.push('\n');
        rendered
    }
}
