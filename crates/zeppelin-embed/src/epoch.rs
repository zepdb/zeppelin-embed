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

/// Complete immutable identity of one embedding interpretation.
///
/// Every field changes the meaning of produced vectors. Mutable store state
/// such as generations, row counts, timestamps, and document counts is
/// deliberately absent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmbeddingEpoch {
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

impl EmbeddingEpoch {
    /// Returns the human-readable model label persisted in `EpochMeta.model`.
    #[must_use]
    pub fn model_label(&self) -> String {
        format!("{}@{}", self.model_id, self.model_version)
    }
}

/// Digest-input framing magic. Changing it changes every embedding epoch.
const EPOCH_MAGIC: &[u8; 8] = b"ZEEMBEP1";

impl EpochId {
    /// Computes the identity of an embedding interpretation.
    ///
    /// # The canonical digest input
    ///
    /// The input is hand-written and little-endian throughout. Its exact
    /// persisted-meaning order is: the eight bytes `ZEEMBEP1`; a u32 byte
    /// length and UTF-8 model id; a u32 byte length and UTF-8 model version;
    /// a u32 byte length and opaque weights-digest bytes; u32 dimensions;
    /// the u16 normalization id; a u32 byte length and UTF-8 prompt prefix;
    /// u32 maximum tokens; the u16 runtime id; the u16 compute-units id; then
    /// a one-byte OS-build presence tag (`0` or `1`) followed, when present,
    /// by its u32 byte length and UTF-8 bytes. No mutable store state is ever
    /// part of this input.
    #[must_use]
    pub fn of(epoch: &EmbeddingEpoch) -> Self {
        Self(xxhash_rust::xxh3::xxh3_64(&canonical_digest_input(epoch)))
    }

    /// Returns the raw digest.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
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
    /// Reconstructs an identity from the already-frozen manifest registry.
    ///
    /// Malformed tokenizer text is rejected rather than interpreted as zero.
    pub fn from_meta(meta: &EpochMeta) -> Result<Self, EpochMetaError> {
        let tokenizer = parse_tokenizer(&meta.tokenizer)?;
        Ok(Self {
            embedding: EpochId(meta.id),
            tokenizer,
        })
    }
}

impl From<&StoreEpoch> for EpochMeta {
    fn from(epoch: &StoreEpoch) -> Self {
        Self {
            id: epoch.identity().embedding.value(),
            model: epoch.embedding.model_label(),
            tokenizer: epoch.tokenizer.to_hex(),
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
    /// The tokenizer field was not exactly sixteen lowercase hexadecimal bytes.
    MalformedTokenizer {
        /// Rejected persisted field.
        value: String,
    },
}

impl std::fmt::Display for EpochMetaError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MalformedTokenizer { value } => write!(
                formatter,
                "persisted tokenizer epoch {value:?} is not sixteen lowercase hexadecimal digits"
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
    let mut input = Vec::with_capacity(128);
    input.extend_from_slice(EPOCH_MAGIC);
    push_bytes(&mut input, epoch.model_id.as_bytes());
    push_bytes(&mut input, epoch.model_version.as_bytes());
    push_bytes(&mut input, &epoch.weights_digest);
    push_u32(&mut input, epoch.dims);
    push_u16(&mut input, epoch.normalization as u16);
    push_bytes(&mut input, epoch.prompt_prefix.as_bytes());
    push_u32(&mut input, epoch.max_tokens);
    push_u16(&mut input, epoch.runtime as u16);
    push_u16(&mut input, epoch.compute_units as u16);
    match &epoch.os_build {
        Some(build) => {
            input.push(1);
            push_bytes(&mut input, build.as_bytes());
        }
        None => input.push(0),
    }
    input
}

fn parse_tokenizer(value: &str) -> Result<TokenizerEpoch, EpochMetaError> {
    if value.len() != 16
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(EpochMetaError::MalformedTokenizer {
            value: value.to_owned(),
        });
    }
    u64::from_str_radix(value, 16)
        .map(TokenizerEpoch::from_value)
        .map_err(|_| EpochMetaError::MalformedTokenizer {
            value: value.to_owned(),
        })
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

    fn baseline() -> EmbeddingEpoch {
        EmbeddingEpoch {
            model_id: "text-embedding".to_owned(),
            model_version: "1.2.3".to_owned(),
            weights_digest: vec![0x10, 0x20, 0x30, 0x40],
            dims: 384,
            normalization: Normalization::L2,
            prompt_prefix: "search_document: ".to_owned(),
            max_tokens: 512,
            runtime: EmbeddingRuntime::CoreMl,
            compute_units: ComputeUnits::CpuAndNeuralEngine,
            os_build: Some("25A100".to_owned()),
        }
    }

    #[test]
    fn changing_any_interpretation_critical_field_changes_the_epoch_id() {
        let base = baseline();
        let mut variants = Vec::new();

        let mut changed = base.clone();
        changed.model_id = "text-embedding-next".to_owned();
        variants.push(changed);

        let mut changed = base.clone();
        changed.model_version = "1.2.4".to_owned();
        variants.push(changed);

        let mut changed = base.clone();
        changed.weights_digest = vec![0x10, 0x20, 0x30, 0x41];
        variants.push(changed);

        let mut changed = base.clone();
        changed.dims = 768;
        variants.push(changed);

        let mut changed = base.clone();
        changed.normalization = Normalization::None;
        variants.push(changed);

        let mut changed = base.clone();
        changed.prompt_prefix = "query: ".to_owned();
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
            include_str!("../tests/fixtures/format/embedding_epoch_digest_input_v1.hex")
        );
    }

    #[test]
    fn a_malformed_persisted_tokenizer_field_is_a_typed_error_not_a_default() {
        let error = EpochIdentity::from_meta(&EpochMeta {
            id: 7,
            model: "model@version".to_owned(),
            tokenizer: "NOT-LOWERCASE-HEX".to_owned(),
        })
        .expect_err("malformed tokenizer text must fail");

        assert!(matches!(error, EpochMetaError::MalformedTokenizer { .. }));
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
