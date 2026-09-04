use zeppelin_embed::epoch::EmbeddingTower;

/// Architecture id persisted in `.zem` tower metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum Architecture {
    /// BERT and XLM-R-compatible post-norm encoder.
    Bert = 1,
    /// Alibaba GTE `NewModel` encoder with RoPE and gated MLP.
    Gte = 2,
}

/// Token pooling policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Pooling {
    /// Masked arithmetic mean.
    Mean = 1,
    /// First token.
    Cls = 2,
    /// Last non-padding token.
    Last = 3,
}

/// Role assigned to one bundle tower.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TowerRole {
    /// Document ingest tower, or the sole symmetric tower.
    Document = 0,
    /// Query-only tower in an asymmetric pair.
    Query = 1,
}

/// Architecture dimensions required to evaluate one tower.
#[derive(Clone, Debug, PartialEq)]
pub struct ArchitectureConfig {
    /// Number of transformer blocks.
    pub layer_count: u16,
    /// Hidden width.
    pub hidden: u32,
    /// Attention head count.
    pub heads: u16,
    /// Feed-forward width.
    pub intermediate: u32,
    /// Token-type vocabulary width.
    pub type_vocab: u32,
    /// Maximum positional embedding count.
    pub max_positions: u32,
    /// Layer normalization epsilon.
    pub layer_norm_eps: f32,
    /// RoPE base for GTE.
    pub rope_theta: f32,
    /// Dense head output width, or zero when absent.
    pub dense_out: u32,
}

/// Fully declared model tower loaded from bundle metadata.
#[derive(Clone, Debug, PartialEq)]
pub struct TowerSpec {
    /// Slot role.
    pub role: TowerRole,
    /// Core epoch fields.
    pub embedding: EmbeddingTower,
    /// Pooling policy.
    pub pooling: Pooling,
    /// Explicit architecture selector.
    pub architecture: Architecture,
    /// Architecture dimensions.
    pub config: ArchitectureConfig,
}

/// Rectangular padded token batch passed to a runtime.
#[derive(Clone, Debug, PartialEq)]
pub struct TokenBatch {
    /// Row-major token ids.
    pub token_ids: Vec<i32>,
    /// Row-major mask values, one for real tokens and zero for padding.
    pub attention_mask: Vec<f32>,
    /// Batch rows.
    pub rows: usize,
    /// Padded tokens per row.
    pub tokens_per_row: usize,
}

impl TokenBatch {
    /// Validates and constructs a rectangular token batch.
    pub fn new(
        token_ids: Vec<i32>,
        attention_mask: Vec<f32>,
        rows: usize,
        tokens_per_row: usize,
    ) -> Result<Self, &'static str> {
        let expected = rows
            .checked_mul(tokens_per_row)
            .ok_or("token batch shape overflow")?;
        if rows == 0 || tokens_per_row == 0 || token_ids.len() != expected {
            return Err("token ids do not match the declared batch shape");
        }
        if attention_mask.len() != expected {
            return Err("attention mask does not match the declared batch shape");
        }
        Ok(Self {
            token_ids,
            attention_mask,
            rows,
            tokens_per_row,
        })
    }
}
