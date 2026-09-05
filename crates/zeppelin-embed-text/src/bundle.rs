use std::collections::BTreeMap;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::runtime::RuntimeError;
use crate::tokenizer::{ModelTokenizer, TokenizerKind};
use crate::tower::{Architecture, ArchitectureConfig, Pooling, TokenBatch, TowerRole, TowerSpec};
use zeppelin_embed::epoch::{ComputeUnits, EmbeddingRuntime, EmbeddingTower, Normalization};

const MAGIC: &[u8; 8] = b"ZEMB0001";
const LAYOUT_VERSION: u32 = 1;
const DIGEST_BYTES: usize = 16;

/// Typed `.zem` load failure.
#[derive(Debug)]
pub enum BundleError {
    /// The artifact could not be opened or mapped.
    Io {
        /// Artifact path.
        path: PathBuf,
        /// Operating-system failure.
        source: std::io::Error,
    },
    /// A bounded field was malformed.
    Format(String),
    /// The whole-file digest did not match the trailer.
    BundleDigest,
    /// One tensor's declared digest did not match its bytes.
    TensorDigest {
        /// Tensor name.
        name: String,
    },
    /// A tower's epoch digest did not match its exact tensor bytes.
    WeightsDigest {
        /// Tower model id.
        model_id: String,
    },
    /// The architecture registry has no implementation for this id.
    UnknownArchitecture {
        /// Persisted numeric id.
        id: u16,
    },
    /// A persisted enum id is unsupported.
    Unsupported {
        /// Field name.
        field: &'static str,
        /// Numeric id.
        id: u64,
    },
}

impl std::fmt::Display for BundleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(formatter, "bundle I/O at {}: {source}", path.display())
            }
            Self::Format(detail) => write!(formatter, "invalid .zem bundle: {detail}"),
            Self::BundleDigest => write!(formatter, ".zem bundle digest mismatch"),
            Self::TensorDigest { name } => write!(formatter, "tensor {name} digest mismatch"),
            Self::WeightsDigest { model_id } => {
                write!(formatter, "tower {model_id} weights digest mismatch")
            }
            Self::UnknownArchitecture { id } => write!(formatter, "unknown architecture id {id}"),
            Self::Unsupported { field, id } => write!(formatter, "unsupported {field} id {id}"),
        }
    }
}

impl std::error::Error for BundleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Format(_)
            | Self::BundleDigest
            | Self::TensorDigest { .. }
            | Self::WeightsDigest { .. }
            | Self::UnknownArchitecture { .. }
            | Self::Unsupported { .. } => None,
        }
    }
}

struct MappedFile {
    pointer: *mut libc::c_void,
    length: usize,
}

// SAFETY: this mapping is immutable for its complete lifetime.
unsafe impl Send for MappedFile {}
// SAFETY: every shared view is immutable.
unsafe impl Sync for MappedFile {}

impl MappedFile {
    fn open(path: &Path) -> Result<Self, BundleError> {
        let file = File::open(path).map_err(|source| BundleError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let length = usize::try_from(
            file.metadata()
                .map_err(|source| BundleError::Io {
                    path: path.to_path_buf(),
                    source,
                })?
                .len(),
        )
        .map_err(|_| BundleError::Format("bundle length exceeds address space".to_owned()))?;
        if length < MAGIC.len() + DIGEST_BYTES {
            return Err(BundleError::Format("bundle is truncated".to_owned()));
        }
        // SAFETY: `file` remains open for the mmap call, the length is its
        // current non-zero length, and the returned mapping is read-only.
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if pointer == libc::MAP_FAILED {
            return Err(BundleError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::last_os_error(),
            });
        }
        Ok(Self { pointer, length })
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: `pointer` names a live read-only mapping of exactly
        // `length` bytes until this owner drops.
        unsafe { std::slice::from_raw_parts(self.pointer.cast::<u8>(), self.length) }
    }
}

impl Drop for MappedFile {
    fn drop(&mut self) {
        // SAFETY: this owner created this exact mapping and drops it once.
        let _ = unsafe { libc::munmap(self.pointer, self.length) };
    }
}

/// Persisted tensor scalar representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TensorDtype {
    /// IEEE 754 little-endian f32.
    F32 = 0,
    /// IEEE 754 little-endian f16.
    F16 = 1,
    /// bfloat16.
    Bf16 = 2,
    /// Signed int8.
    Int8 = 3,
    /// Packed int4 plus separately named scales.
    Int4 = 4,
}

/// One validated tensor descriptor into the immutable mapping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TensorDescriptor {
    /// Stable model tensor name.
    pub name: String,
    /// Scalar representation.
    pub dtype: TensorDtype,
    /// Row-major dimensions.
    pub shape: Vec<u32>,
    /// Byte offset in the bundle.
    pub offset: usize,
    /// Byte length.
    pub length: usize,
    /// Xxh3-64 over the exact tensor bytes.
    pub digest: u64,
}

/// Validated immutable model bundle.
pub struct Bundle {
    mapping: Arc<MappedFile>,
    document: Arc<TowerSpec>,
    query: Arc<TowerSpec>,
    tokenizer: ModelTokenizer,
    tensors: BTreeMap<String, TensorDescriptor>,
    alignment_digest: Vec<u8>,
    digest: u128,
    alpha: f64,
}

impl Bundle {
    /// Opens, validates, and pre-faults the query tower of one `.zem` file.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BundleError> {
        let path = path.as_ref();
        let mapping = Arc::new(MappedFile::open(path)?);
        let bytes = mapping.bytes();
        let digest_start = bytes
            .len()
            .checked_sub(DIGEST_BYTES)
            .ok_or_else(|| BundleError::Format("missing bundle digest".to_owned()))?;
        let expected_digest = bytes
            .get(digest_start..)
            .and_then(|value| value.try_into().ok())
            .map(u128::from_le_bytes)
            .ok_or_else(|| BundleError::Format("bundle digest is truncated".to_owned()))?;
        let actual_digest = xxhash_rust::xxh3::xxh3_128(
            bytes
                .get(..digest_start)
                .ok_or_else(|| BundleError::Format("bundle body is missing".to_owned()))?,
        );
        if actual_digest != expected_digest {
            return Err(BundleError::BundleDigest);
        }
        let mut cursor = Cursor::new(
            bytes
                .get(..digest_start)
                .ok_or_else(|| BundleError::Format("bundle body is missing".to_owned()))?,
        );
        if cursor.take(MAGIC.len())? != MAGIC {
            return Err(BundleError::Format("magic mismatch".to_owned()));
        }
        let version = cursor.u32()?;
        if version != LAYOUT_VERSION {
            return Err(BundleError::Unsupported {
                field: "layout version",
                id: u64::from(version),
            });
        }
        let header_len = usize::try_from(cursor.u32()?)
            .map_err(|_| BundleError::Format("header length overflow".to_owned()))?;
        let tower_count = cursor.u8()?;
        cursor.zeroes(7)?;
        if tower_count != 1 && tower_count != 2 {
            return Err(BundleError::Format(format!(
                "tower count {tower_count}, expected one or two"
            )));
        }
        let first = Arc::new(parse_tower(&mut cursor)?);
        let second = if tower_count == 2 {
            Some(Arc::new(parse_tower(&mut cursor)?))
        } else {
            None
        };
        let (document, query) = bind_towers(first, second)?;
        let tokenizer = parse_tokenizer(&mut cursor)?;
        let alpha = cursor.f64()?;
        if !alpha.is_finite() || !(0.0..=1.0).contains(&alpha) {
            return Err(BundleError::Format(
                "hybrid alpha is outside 0..=1".to_owned(),
            ));
        }
        let tensor_count = usize::try_from(cursor.u32()?)
            .map_err(|_| BundleError::Format("tensor count overflow".to_owned()))?;
        let mut tensors = BTreeMap::new();
        let mut tensor_order = Vec::with_capacity(tensor_count);
        for _ in 0..tensor_count {
            let tensor = parse_tensor(&mut cursor, digest_start, header_len)?;
            tensor_order.push(tensor.name.clone());
            if tensors.insert(tensor.name.clone(), tensor).is_some() {
                return Err(BundleError::Format("duplicate tensor name".to_owned()));
            }
        }
        let alignment_digest = cursor.bytes()?;
        if cursor.position() > header_len || header_len > digest_start {
            return Err(BundleError::Format("header length is invalid".to_owned()));
        }
        cursor.take(header_len.saturating_sub(cursor.position()))?;

        for tensor in tensors.values() {
            let tensor_bytes = bytes
                .get(tensor.offset..tensor.offset.saturating_add(tensor.length))
                .ok_or_else(|| {
                    BundleError::Format(format!("tensor {} is out of bounds", tensor.name))
                })?;
            if xxhash_rust::xxh3::xxh3_64(tensor_bytes) != tensor.digest {
                return Err(BundleError::TensorDigest {
                    name: tensor.name.clone(),
                });
            }
        }
        verify_weights_digest(&document, &tensor_order, &tensors, bytes)?;
        if !Arc::ptr_eq(&document, &query) {
            verify_weights_digest(&query, &tensor_order, &tensors, bytes)?;
        }
        let bundle = Self {
            mapping,
            document,
            query,
            tokenizer,
            tensors,
            alignment_digest,
            digest: actual_digest,
            alpha,
        };
        bundle.prefault_query()?;
        Ok(bundle)
    }

    /// Returns the document slot declaration.
    #[must_use]
    pub fn document_tower(&self) -> &TowerSpec {
        &self.document
    }

    /// Returns the query slot declaration.
    #[must_use]
    pub fn query_tower(&self) -> &TowerSpec {
        &self.query
    }

    /// Returns true when one tower object serves both roles.
    #[must_use]
    pub fn is_symmetric(&self) -> bool {
        Arc::ptr_eq(&self.document, &self.query)
    }

    /// Returns the model tokenizer.
    #[must_use]
    pub(crate) const fn tokenizer(&self) -> &ModelTokenizer {
        &self.tokenizer
    }

    /// Returns one validated tensor descriptor.
    #[must_use]
    pub fn tensor(&self, name: &str) -> Option<&TensorDescriptor> {
        self.tensors.get(name)
    }

    /// Returns the exact validated bytes for one descriptor.
    pub fn tensor_bytes(&self, tensor: &TensorDescriptor) -> Result<&[u8], BundleError> {
        self.mapping
            .bytes()
            .get(tensor.offset..tensor.offset.saturating_add(tensor.length))
            .ok_or_else(|| BundleError::Format(format!("tensor {} is out of bounds", tensor.name)))
    }

    /// Returns the alignment artifact digest.
    #[must_use]
    pub fn alignment_digest(&self) -> &[u8] {
        &self.alignment_digest
    }

    /// Returns the complete bundle digest.
    #[must_use]
    pub const fn digest(&self) -> u128 {
        self.digest
    }

    /// Returns the bundle-carried hybrid alpha.
    #[must_use]
    pub const fn hybrid_alpha(&self) -> f64 {
        self.alpha
    }

    /// Applies the query tower's exact persisted prefix to caller text.
    #[must_use]
    pub fn query_input(&self, text: &str) -> String {
        format!("{}{text}", self.query.embedding.prompt_prefix)
    }

    /// Applies the bundle prefix and tokenizer exactly as `query_text` does.
    #[doc(hidden)]
    pub fn tokenize_query(&self, text: &str) -> Result<TokenBatch, RuntimeError> {
        self.tokenizer.encode_batch(
            &[self.query_input(text)],
            self.query.embedding.max_tokens as usize,
        )
    }

    /// Tokenizes one query and pads it to exactly `width` tokens.
    ///
    /// A CoreML program is exported at a fixed sequence length, so it
    /// needs an exact width rather than the batch-longest padding the
    /// MLX path uses.
    pub fn tokenize_query_padded(
        &self,
        text: &str,
        width: usize,
    ) -> Result<TokenBatch, RuntimeError> {
        self.tokenize_query(text)?.padded_to(width)
    }

    /// Applies the query prefix and tokenizer to a query batch.
    #[doc(hidden)]
    pub fn tokenize_queries(&self, texts: &[&str]) -> Result<TokenBatch, RuntimeError> {
        let inputs = texts
            .iter()
            .map(|text| self.query_input(text))
            .collect::<Vec<_>>();
        self.tokenizer
            .encode_batch(&inputs, self.query.embedding.max_tokens as usize)
    }

    /// Tokenizes already formed document chunks with the ingest tokenizer and
    /// document limit. Chunk formation remains the responsibility of ingestion.
    #[doc(hidden)]
    pub fn tokenize_document_chunks(&self, texts: &[String]) -> Result<TokenBatch, RuntimeError> {
        self.tokenizer
            .encode_batch(texts, self.document.embedding.max_tokens as usize)
    }

    fn prefault_query(&self) -> Result<(), BundleError> {
        for tensor in self
            .tensors
            .values()
            .filter(|tensor| tensor.name.starts_with("query/") || self.is_symmetric())
        {
            let bytes = self.tensor_bytes(tensor)?;
            let mut offset = 0_usize;
            while offset < bytes.len() {
                let _ = bytes.get(offset).copied();
                offset = offset.saturating_add(4096);
            }
        }
        Ok(())
    }
}

fn verify_weights_digest(
    tower: &TowerSpec,
    tensor_order: &[String],
    tensors: &BTreeMap<String, TensorDescriptor>,
    bundle: &[u8],
) -> Result<(), BundleError> {
    let prefix = match tower.role {
        TowerRole::Document => "document/",
        TowerRole::Query => "query/",
    };
    let mut digest = xxhash_rust::xxh3::Xxh3::new();
    for name in tensor_order.iter().filter(|name| name.starts_with(prefix)) {
        let tensor = tensors
            .get(name)
            .ok_or_else(|| BundleError::Format(format!("tensor {name} disappeared")))?;
        let bytes = bundle
            .get(tensor.offset..tensor.offset.saturating_add(tensor.length))
            .ok_or_else(|| BundleError::Format(format!("tensor {name} is out of bounds")))?;
        digest.update(bytes);
    }
    if tower.embedding.weights_digest.as_slice() != digest.digest128().to_le_bytes() {
        return Err(BundleError::WeightsDigest {
            model_id: tower.embedding.model_id.clone(),
        });
    }
    Ok(())
}

fn bind_towers(
    first: Arc<TowerSpec>,
    second: Option<Arc<TowerSpec>>,
) -> Result<(Arc<TowerSpec>, Arc<TowerSpec>), BundleError> {
    let Some(second) = second else {
        if first.role != TowerRole::Document {
            return Err(BundleError::Format(
                "a symmetric bundle must name its tower document".to_owned(),
            ));
        }
        return Ok((Arc::clone(&first), first));
    };
    match (first.role, second.role) {
        (TowerRole::Document, TowerRole::Query) => Ok((first, second)),
        (TowerRole::Query, TowerRole::Document) => Ok((second, first)),
        _ => Err(BundleError::Format(
            "a two-tower bundle must contain one document and one query tower".to_owned(),
        )),
    }
}

fn parse_tower(cursor: &mut Cursor<'_>) -> Result<TowerSpec, BundleError> {
    let role = match cursor.u8()? {
        0 => TowerRole::Document,
        1 => TowerRole::Query,
        id => {
            return Err(BundleError::Unsupported {
                field: "tower role",
                id: u64::from(id),
            });
        }
    };
    let model_id = cursor.string()?;
    let model_version = cursor.string()?;
    let weights_digest = cursor.bytes()?;
    let dims = cursor.u32()?;
    let normalization = match cursor.u16()? {
        0 => Normalization::None,
        1 => Normalization::L2,
        id => {
            return Err(BundleError::Unsupported {
                field: "normalization",
                id: u64::from(id),
            });
        }
    };
    let prompt_prefix = cursor.string()?;
    let max_tokens = cursor.u32()?;
    let runtime = match cursor.u16()? {
        2 => EmbeddingRuntime::Mlx,
        id => {
            return Err(BundleError::Unsupported {
                field: "runtime",
                id: u64::from(id),
            });
        }
    };
    let compute_units = match cursor.u16()? {
        1 => ComputeUnits::Cpu,
        2 => ComputeUnits::CpuAndGpu,
        3 => ComputeUnits::CpuAndNeuralEngine,
        4 => ComputeUnits::All,
        id => {
            return Err(BundleError::Unsupported {
                field: "compute units",
                id: u64::from(id),
            });
        }
    };
    let os_build = match cursor.u8()? {
        0 => None,
        1 => Some(cursor.string()?),
        id => {
            return Err(BundleError::Unsupported {
                field: "os build presence",
                id: u64::from(id),
            });
        }
    };
    let pooling = match cursor.u8()? {
        1 => Pooling::Mean,
        2 => Pooling::Cls,
        3 => Pooling::Last,
        id => {
            return Err(BundleError::Unsupported {
                field: "pooling",
                id: u64::from(id),
            });
        }
    };
    let architecture = match cursor.u16()? {
        1 => Architecture::Bert,
        2 => Architecture::Gte,
        id => return Err(BundleError::UnknownArchitecture { id }),
    };
    let config = ArchitectureConfig {
        layer_count: cursor.u16()?,
        hidden: cursor.u32()?,
        heads: cursor.u16()?,
        intermediate: cursor.u32()?,
        type_vocab: cursor.u32()?,
        max_positions: cursor.u32()?,
        layer_norm_eps: cursor.f32()?,
        rope_theta: cursor.f32()?,
        dense_out: cursor.u32()?,
    };
    if dims == 0 || max_tokens == 0 || config.hidden == 0 || config.heads == 0 {
        return Err(BundleError::Format(
            "tower dimensions must be non-zero".to_owned(),
        ));
    }
    if config.hidden % u32::from(config.heads) != 0 {
        return Err(BundleError::Format(
            "hidden width is not divisible by heads".to_owned(),
        ));
    }
    Ok(TowerSpec {
        role,
        embedding: EmbeddingTower {
            model_id,
            model_version,
            weights_digest,
            dims,
            normalization,
            prompt_prefix,
            max_tokens,
            runtime,
            compute_units,
            os_build,
        },
        pooling,
        architecture,
        config,
    })
}

fn parse_tokenizer(cursor: &mut Cursor<'_>) -> Result<ModelTokenizer, BundleError> {
    let kind = match cursor.u8()? {
        1 => TokenizerKind::WordPiece,
        2 => TokenizerKind::Unigram,
        id => {
            return Err(BundleError::Unsupported {
                field: "tokenizer",
                id: u64::from(id),
            });
        }
    };
    let flags = cursor.u8()?;
    if flags & !0b11 != 0 {
        return Err(BundleError::Unsupported {
            field: "tokenizer flags",
            id: u64::from(flags),
        });
    }
    cursor.zeroes(2)?;
    let pad = cursor.u32()?;
    let unk = cursor.u32()?;
    let cls = cursor.u32()?;
    let sep = cursor.u32()?;
    let count = usize::try_from(cursor.u32()?)
        .map_err(|_| BundleError::Format("vocabulary count overflow".to_owned()))?;
    let mut vocabulary = Vec::with_capacity(count);
    let mut scores = Vec::with_capacity(count);
    for _ in 0..count {
        vocabulary.push(cursor.string()?);
        scores.push(cursor.f32()?);
    }
    ModelTokenizer::new(
        kind,
        vocabulary,
        scores,
        pad,
        unk,
        cls,
        sep,
        flags & 1 != 0,
        flags & 2 != 0,
    )
    .map_err(|detail| BundleError::Format(detail.to_owned()))
}

fn parse_tensor(
    cursor: &mut Cursor<'_>,
    body_len: usize,
    header_len: usize,
) -> Result<TensorDescriptor, BundleError> {
    let name = cursor.string()?;
    let dtype = match cursor.u8()? {
        0 => TensorDtype::F32,
        1 => TensorDtype::F16,
        2 => TensorDtype::Bf16,
        3 => TensorDtype::Int8,
        4 => TensorDtype::Int4,
        id => {
            return Err(BundleError::Unsupported {
                field: "tensor dtype",
                id: u64::from(id),
            });
        }
    };
    let rank = usize::from(cursor.u8()?);
    cursor.zeroes(2)?;
    if rank == 0 || rank > 8 {
        return Err(BundleError::Format(format!(
            "tensor {name} rank is invalid"
        )));
    }
    let mut shape = Vec::with_capacity(rank);
    for _ in 0..rank {
        shape.push(cursor.u32()?);
    }
    let offset = usize::try_from(cursor.u64()?)
        .map_err(|_| BundleError::Format(format!("tensor {name} offset overflow")))?;
    let length = usize::try_from(cursor.u64()?)
        .map_err(|_| BundleError::Format(format!("tensor {name} length overflow")))?;
    let digest = cursor.u64()?;
    let scalar_bytes = match dtype {
        TensorDtype::F32 => 4_usize,
        TensorDtype::F16 | TensorDtype::Bf16 => 2,
        TensorDtype::Int8 => 1,
        TensorDtype::Int4 => 0,
    };
    if scalar_bytes != 0 {
        let expected = shape
            .iter()
            .try_fold(scalar_bytes, |bytes, dim| bytes.checked_mul(*dim as usize));
        if expected != Some(length) {
            return Err(BundleError::Format(format!(
                "tensor {name} shape/length mismatch"
            )));
        }
    }
    let end = offset
        .checked_add(length)
        .ok_or_else(|| BundleError::Format(format!("tensor {name} range overflow")))?;
    if offset < header_len || end > body_len {
        return Err(BundleError::Format(format!(
            "tensor {name} range is outside the data section"
        )));
    }
    Ok(TensorDescriptor {
        name,
        dtype,
        shape,
        offset,
        length,
        digest,
    })
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    const fn position(&self) -> usize {
        self.position
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], BundleError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| BundleError::Format("cursor overflow".to_owned()))?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| BundleError::Format("bundle metadata is truncated".to_owned()))?;
        self.position = end;
        Ok(value)
    }

    fn zeroes(&mut self, length: usize) -> Result<(), BundleError> {
        if self.take(length)?.iter().any(|byte| *byte != 0) {
            return Err(BundleError::Format(
                "reserved bytes are non-zero".to_owned(),
            ));
        }
        Ok(())
    }

    fn u8(&mut self) -> Result<u8, BundleError> {
        self.take(1)?
            .first()
            .copied()
            .ok_or_else(|| BundleError::Format("u8 is truncated".to_owned()))
    }

    fn u16(&mut self) -> Result<u16, BundleError> {
        self.take(2)?
            .try_into()
            .ok()
            .map(u16::from_le_bytes)
            .ok_or_else(|| BundleError::Format("u16 is truncated".to_owned()))
    }

    fn u32(&mut self) -> Result<u32, BundleError> {
        self.take(4)?
            .try_into()
            .ok()
            .map(u32::from_le_bytes)
            .ok_or_else(|| BundleError::Format("u32 is truncated".to_owned()))
    }

    fn u64(&mut self) -> Result<u64, BundleError> {
        self.take(8)?
            .try_into()
            .ok()
            .map(u64::from_le_bytes)
            .ok_or_else(|| BundleError::Format("u64 is truncated".to_owned()))
    }

    fn f32(&mut self) -> Result<f32, BundleError> {
        self.u32().map(f32::from_bits)
    }

    fn f64(&mut self) -> Result<f64, BundleError> {
        self.u64().map(f64::from_bits)
    }

    fn bytes(&mut self) -> Result<Vec<u8>, BundleError> {
        let length = usize::try_from(self.u32()?)
            .map_err(|_| BundleError::Format("byte field length overflow".to_owned()))?;
        Ok(self.take(length)?.to_vec())
    }

    fn string(&mut self) -> Result<String, BundleError> {
        String::from_utf8(self.bytes()?)
            .map_err(|_| BundleError::Format("string field is not UTF-8".to_owned()))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn metadata_cursor_reads_every_scalar_and_refuses_truncation_and_nonzero_reservations() {
        let mut bytes = Vec::new();
        bytes.push(7);
        bytes.extend_from_slice(&513_u16.to_le_bytes());
        bytes.extend_from_slice(&65_537_u32.to_le_bytes());
        bytes.extend_from_slice(&9_000_000_001_u64.to_le_bytes());
        bytes.extend_from_slice(&1.25_f32.to_le_bytes());
        bytes.extend_from_slice(&2.5_f64.to_le_bytes());
        bytes.extend_from_slice(&3_u32.to_le_bytes());
        bytes.extend_from_slice(b"abc");
        bytes.extend_from_slice(&2_u32.to_le_bytes());
        bytes.extend_from_slice(&[4, 5]);
        bytes.extend_from_slice(&[0, 0]);
        let mut cursor = Cursor::new(&bytes);
        assert_eq!(cursor.u8().expect("u8"), 7);
        assert_eq!(cursor.u16().expect("u16"), 513);
        assert_eq!(cursor.u32().expect("u32"), 65_537);
        assert_eq!(cursor.u64().expect("u64"), 9_000_000_001);
        assert_eq!(cursor.f32().expect("f32"), 1.25);
        assert_eq!(cursor.f64().expect("f64"), 2.5);
        assert_eq!(cursor.string().expect("string"), "abc");
        assert_eq!(cursor.bytes().expect("bytes"), [4, 5]);
        cursor.zeroes(2).expect("reserved zeroes");
        assert_eq!(cursor.position(), bytes.len());
        assert!(cursor.u8().is_err());

        assert!(Cursor::new(&[1]).u16().is_err());
        assert!(Cursor::new(&[1, 0]).u32().is_err());
        assert!(Cursor::new(&[1, 0, 0, 0]).u64().is_err());
        assert!(Cursor::new(&[1]).zeroes(1).is_err());
        let invalid_utf8 = [1, 0, 0, 0, 0xff];
        assert!(Cursor::new(&invalid_utf8).string().is_err());
    }

    #[test]
    fn bundle_errors_are_typed_and_descriptive() {
        let io = BundleError::Io {
            path: PathBuf::from("missing.zem"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "absent"),
        };
        assert!(io.source().is_some());
        assert!(io.to_string().contains("missing.zem"));
        for error in [
            BundleError::Format("bad".to_owned()),
            BundleError::BundleDigest,
            BundleError::TensorDigest {
                name: "tensor".to_owned(),
            },
            BundleError::WeightsDigest {
                model_id: "model".to_owned(),
            },
            BundleError::UnknownArchitecture { id: 99 },
            BundleError::Unsupported {
                field: "runtime",
                id: 88,
            },
        ] {
            assert!(error.source().is_none());
            assert!(!error.to_string().is_empty());
        }
    }
}
