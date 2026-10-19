use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use mlx_rs::{Array, Device, Stream};

use crate::arch::{bert, gte};
use crate::bundle::{Bundle, TensorDtype};
use crate::runtime::{EmbeddingBatch, ModelRuntime, RuntimeError, RuntimeIdentity};
use crate::tower::{Architecture, TokenBatch, TowerRole, TowerSpec};
use zeppelin_embed::epoch::ComputeUnits;

const MAX_EVAL_ROWS: usize = 32;
// ZE-525 native FP16 probes on M3 Max under GPU contention: at 4,096 tokens,
// warm evaluations took up to ~0.53 s; a cold 8 x 512 evaluation took ~2.96 s.
// Throughput/token was flat above this budget, with contention-dependent
// timeouts at larger shapes. An over-budget row must run alone.
const MAX_EVAL_TOKENS: usize = 4_096;

/// One tower evaluated on a single owned MLX GPU stream.
pub struct MlxRuntime {
    tower: TowerSpec,
    tensors: BTreeMap<String, Array>,
    stream: Option<Stream>,
    gpu: bool,
}

fn mlx_calls() -> &'static Mutex<()> {
    static CALLS: OnceLock<Mutex<()>> = OnceLock::new();
    CALLS.get_or_init(|| Mutex::new(()))
}

fn lock_mlx_calls_controlled<E: From<RuntimeError>>(
    checkpoint: &mut impl FnMut() -> Result<(), E>,
) -> Result<std::sync::MutexGuard<'static, ()>, E> {
    checkpoint()?;
    let guard = loop {
        match mlx_calls().try_lock() {
            Ok(guard) => break guard,
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(RuntimeError::Mlx("MLX call mutex is poisoned".to_owned()).into());
            }
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        checkpoint()?;
        std::thread::park_timeout(std::time::Duration::from_millis(1));
    };
    checkpoint()?;
    Ok(guard)
}

#[cfg(all(test, feature = "test-support"))]
#[allow(clippy::expect_used)]
mod astra_18_tests {
    use super::*;
    use std::sync::{Barrier, mpsc};
    use std::time::Duration;
    use zeppelin_embed::lifecycle::{Deadline, ManualMonotonicClock, QueryControl, QueryError};

    #[test]
    fn astra_18_mlx_mutex_wait_observes_original_deadline() {
        let clock = Arc::new(ManualMonotonicClock::new());
        let deadline = Deadline::after_with_test_clock(Duration::from_secs(1), clock.clone())
            .expect("original deadline");
        let control = QueryControl::Deadline(deadline);
        let entered = Barrier::new(2);
        let resume = Barrier::new(2);
        let held = mlx_calls()
            .lock()
            .expect("hold actual process-wide MLX lock");
        let before_unlock = std::thread::scope(|scope| {
            let (send, receive) = mpsc::sync_channel(1);
            let control = &control;
            let entered = &entered;
            let resume = &resume;
            let worker = scope.spawn(move || {
                let mut first = true;
                let result = lock_mlx_calls_controlled(&mut || {
                    let observed = control.checkpoint().map_err(crate::TextError::Query);
                    if first {
                        first = false;
                        entered.wait();
                        resume.wait();
                    }
                    observed
                });
                send.send(matches!(
                    result,
                    Err(crate::TextError::Query(QueryError::Timeout {
                        partial: false
                    }))
                ))
                .expect("admission result");
            });
            entered.wait();
            clock.advance(Duration::from_secs(2));
            resume.wait();
            let before_unlock = receive.recv_timeout(Duration::from_secs(2));
            drop(held);
            worker
                .join()
                .expect("join even when the watchdog observes RED");
            before_unlock
        });
        println!("MLX deadline returned while holder retained mutex: {before_unlock:?}");
        assert_eq!(
            before_unlock,
            Ok(true),
            "expired admission must not wait for MLX evaluation to release the lock"
        );
        let clean = lock_mlx_calls_controlled(&mut || Ok::<(), RuntimeError>(()));
        assert!(
            clean.is_ok(),
            "uncanceled admission still acquires the same mutex"
        );
    }
}

impl MlxRuntime {
    /// Returns the row ceiling; the padded-token budget can split earlier.
    #[doc(hidden)]
    #[must_use]
    pub const fn max_eval_rows() -> usize {
        MAX_EVAL_ROWS
    }

    /// Loads one tower's tensors into MLX-managed unified memory.
    pub fn load(bundle: Arc<Bundle>, role: TowerRole) -> Result<Self, RuntimeError> {
        Self::load_for_compute(bundle, role, ComputeUnits::CpuAndGpu)
    }

    /// Loads one tower on an explicit MLX CPU or GPU stream.
    #[doc(hidden)]
    pub fn load_for_compute(
        bundle: Arc<Bundle>,
        role: TowerRole,
        compute_units: ComputeUnits,
    ) -> Result<Self, RuntimeError> {
        let _call = mlx_calls()
            .lock()
            .map_err(|_| RuntimeError::Mlx("MLX call mutex is poisoned".to_owned()))?;
        let tower = match role {
            TowerRole::Document => bundle.document_tower(),
            TowerRole::Query => bundle.query_tower(),
        }
        .clone();
        let prefix = match role {
            TowerRole::Document => "document/",
            TowerRole::Query if bundle.is_symmetric() => "document/",
            TowerRole::Query => "query/",
        };
        let mut tensors = BTreeMap::new();
        for name in required_tensor_names(&tower) {
            let qualified = format!("{prefix}{name}");
            let descriptor = bundle
                .tensor(&qualified)
                .ok_or_else(|| RuntimeError::MissingTensor(qualified.clone()))?;
            if !matches!(descriptor.dtype, TensorDtype::F32 | TensorDtype::F16) {
                return Err(RuntimeError::UnsupportedDtype(qualified));
            }
            let raw = bundle
                .tensor_bytes(descriptor)
                .map_err(|error| RuntimeError::Shape(error.to_string()))?;
            let shape = descriptor
                .shape
                .iter()
                .map(|dim| i32::try_from(*dim))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| {
                    RuntimeError::Shape(format!("tensor {qualified} dimension exceeds i32"))
                })?;
            let array = match descriptor.dtype {
                TensorDtype::F16 => {
                    // Use aligned native-endian storage, without another crate.
                    let values = raw
                        .chunks_exact(2)
                        .map(|bytes| {
                            bytes.try_into().map(u16::from_le_bytes).map_err(|_| {
                                RuntimeError::Shape(format!("tensor {qualified} is truncated"))
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    // SAFETY: Bundle validated shape * 2 == raw.len(); values
                    // holds aligned IEEE half bits and MLX copies the buffer.
                    unsafe {
                        Array::from_raw_data(values.as_ptr().cast(), &shape, mlx_rs::Dtype::Float16)
                    }
                }
                TensorDtype::F32 => {
                    let values = raw
                        .chunks_exact(4)
                        .map(|bytes| {
                            bytes.try_into().map(f32::from_le_bytes).map_err(|_| {
                                RuntimeError::Shape(format!("tensor {qualified} is truncated"))
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    Array::from_slice(&values, &shape)
                }
                _ => return Err(RuntimeError::UnsupportedDtype(qualified)),
            };
            tensors.insert(name.to_owned(), array);
        }
        let (device, gpu) = match compute_units {
            ComputeUnits::Cpu => (Device::cpu(), false),
            ComputeUnits::CpuAndGpu => (Device::gpu(), true),
            ComputeUnits::CpuAndNeuralEngine | ComputeUnits::All => {
                return Err(RuntimeError::Mlx(
                    "MLX does not target the Neural Engine".to_owned(),
                ));
            }
        };
        let stream = Stream::new_with_device(&device);
        Ok(Self {
            tower,
            tensors,
            stream: Some(stream),
            gpu,
        })
    }

    pub(crate) fn tower(&self) -> &TowerSpec {
        &self.tower
    }

    pub(crate) fn tensor(&self, name: &str) -> Result<&Array, RuntimeError> {
        self.tensors
            .get(name)
            .ok_or_else(|| RuntimeError::MissingTensor(name.to_owned()))
    }

    pub(crate) fn stream(&self) -> Result<&Stream, RuntimeError> {
        self.stream
            .as_ref()
            .ok_or_else(|| RuntimeError::Mlx("MLX stream is closed".to_owned()))
    }

    /// Query admission may stop while another runtime owns the process-wide
    /// lock. Once foreign evaluation starts, retain that guard until it ends.
    pub(crate) fn embed_batch_controlled<E: From<RuntimeError>>(
        &mut self,
        tokens: &TokenBatch,
        checkpoint: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<EmbeddingBatch, E> {
        let _call = lock_mlx_calls_controlled(checkpoint)?;
        self.embed_batch_locked(tokens, checkpoint)
    }

    fn embed_batch_locked<E: From<RuntimeError>>(
        &mut self,
        tokens: &TokenBatch,
        checkpoint: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<EmbeddingBatch, E> {
        checkpoint()?;
        let dims = self.tower.embedding.dims as usize;
        let capacity = tokens
            .rows
            .checked_mul(dims)
            .ok_or_else(|| RuntimeError::Shape("embedding output size overflow".to_owned()))?;
        let mut values = Vec::with_capacity(capacity);
        let mut start = 0_usize;
        while start < tokens.rows {
            checkpoint()?;
            let chunk = next_eval_chunk(tokens, start)?;
            let end = start.saturating_add(chunk.rows);
            let result = self.eval_chunk(&chunk);
            checkpoint()?;
            values.extend(result?.into_values());
            start = end;
        }
        EmbeddingBatch::new(values, tokens.rows, dims).map_err(Into::into)
    }
}

/// Greedily retain caller order, padding only to this evaluation's longest row.
fn next_eval_chunk(tokens: &TokenBatch, start: usize) -> Result<TokenBatch, RuntimeError> {
    let expected = tokens
        .rows
        .checked_mul(tokens.tokens_per_row)
        .ok_or_else(|| RuntimeError::Shape("token batch shape overflow".to_owned()))?;
    if tokens.tokens_per_row == 0 || start >= tokens.rows || tokens.token_ids.len() != expected {
        return Err(RuntimeError::Shape(
            "token batch chunk is truncated".to_owned(),
        ));
    }
    if tokens.attention_mask.len() != expected {
        return Err(RuntimeError::Shape(
            "attention chunk is truncated".to_owned(),
        ));
    }
    let mut rows = 0;
    let mut width = 1;
    for mask in tokens
        .attention_mask
        .chunks_exact(tokens.tokens_per_row)
        .skip(start)
        .take(MAX_EVAL_ROWS)
    {
        // Keep masked holes and token positions; remove only trailing padding.
        let row_width = mask
            .iter()
            .rposition(|value| *value != 0.0)
            .map_or(1, |position| position + 1);
        let next_width = width.max(row_width);
        if rows > 0 && rows + 1 > MAX_EVAL_TOKENS / next_width {
            break;
        }
        rows += 1;
        width = next_width;
    }
    let mut token_ids = Vec::with_capacity(rows * width);
    let mut attention_mask = Vec::with_capacity(rows * width);
    for (ids, mask) in tokens
        .token_ids
        .chunks_exact(tokens.tokens_per_row)
        .zip(tokens.attention_mask.chunks_exact(tokens.tokens_per_row))
        .skip(start)
        .take(rows)
    {
        token_ids.extend(ids.iter().take(width).copied());
        attention_mask.extend(mask.iter().take(width).copied());
    }
    TokenBatch::new(token_ids, attention_mask, rows, width)
        .map_err(|detail| RuntimeError::Shape(detail.to_owned()))
}

impl ModelRuntime for MlxRuntime {
    fn embed_batch(&mut self, tokens: &TokenBatch) -> Result<EmbeddingBatch, RuntimeError> {
        let _call = mlx_calls()
            .lock()
            .map_err(|_| RuntimeError::Mlx("MLX call mutex is poisoned".to_owned()))?;
        self.embed_batch_locked(tokens, &mut || Ok::<(), RuntimeError>(()))
    }

    fn warm(&mut self) -> Result<(), RuntimeError> {
        let tokens = TokenBatch::new(vec![0], vec![1.0], 1, 1)
            .map_err(|detail| RuntimeError::Shape(detail.to_owned()))?;
        let _ = self.embed_batch(&tokens)?;
        Ok(())
    }

    fn identity(&self) -> RuntimeIdentity {
        RuntimeIdentity {
            name: "mlx-c",
            gpu: self.gpu,
        }
    }
}

impl MlxRuntime {
    fn eval_chunk(&mut self, tokens: &TokenBatch) -> Result<EmbeddingBatch, RuntimeError> {
        #[cfg(test)]
        let started = {
            assert!(
                tokens.rows == 1 || tokens.rows * tokens.tokens_per_row <= MAX_EVAL_TOKENS,
                "only a single row may exceed the evaluation token budget"
            );
            std::time::Instant::now()
        };
        let output = match self.tower.architecture {
            Architecture::Bert => bert::forward(self, tokens),
            Architecture::Gte | Architecture::ArcticQuery => gte::forward(self, tokens),
        }?;
        // The public embedding buffer is f32, even when tower computation is
        // f16. Convert only the final output, retaining half-precision weights.
        let output = output
            .as_dtype_device(mlx_rs::Dtype::Float32, self.stream()?)
            .map_err(|error| RuntimeError::Mlx(error.to_string()))?;
        // CLS pooling and MRL truncation can leave gaps between logical
        // rows. mlx-rs's slice accessor reads consecutive memory and does
        // not gather by stride. Flatten our unit-inner-stride output on
        // this runtime's stream before copying it to the host.
        let output = output
            .reshape_device(&[-1], self.stream()?)
            .map_err(|error| RuntimeError::Mlx(error.to_string()))?;
        output
            .eval()
            .map_err(|error| RuntimeError::Mlx(error.to_string()))?;
        // With one retained coordinate, flattening may still produce a
        // strided vector. Gather its logical elements without arithmetic
        // (including preserving signed zero) before using the slice accessor.
        let output = if output.strides() == [1] {
            output
        } else {
            let count = i32::try_from(output.size())
                .map_err(|_| RuntimeError::Shape("output size exceeds i32".to_owned()))?;
            let indices = Array::from_slice(&(0..count).collect::<Vec<_>>(), &[count]);
            output
                .take_axis_device(&indices, 0, self.stream()?)
                .map_err(|error| RuntimeError::Mlx(error.to_string()))?
        };
        let values = output
            .try_as_slice::<f32>()
            .map_err(|error| RuntimeError::Mlx(error.to_string()))?
            .to_vec();
        #[cfg(test)]
        eprintln!(
            "ZE-525 eval rows={} padded_length={} padded_tokens={} completed_ms={:.3}",
            tokens.rows,
            tokens.tokens_per_row,
            tokens.rows * tokens.tokens_per_row,
            started.elapsed().as_secs_f64() * 1000.0
        );
        EmbeddingBatch::new(values, tokens.rows, self.tower.embedding.dims as usize)
    }
}

impl Drop for MlxRuntime {
    fn drop(&mut self) {
        let _call = match mlx_calls().lock() {
            Ok(call) => call,
            Err(poisoned) => poisoned.into_inner(),
        };
        self.tensors.clear();
        self.stream.take();
    }
}

fn required_tensor_names(tower: &TowerSpec) -> Vec<String> {
    let mut names = vec!["embeddings.word_embeddings.weight".to_owned()];
    if tower.architecture == Architecture::ArcticQuery {
        if tower.config.embedding_rank != tower.config.hidden {
            names.push("embeddings.embedding_projection.weight".to_owned());
        }
        if tower.config.output_dimension != tower.config.hidden {
            names.push("output_projection.weight".to_owned());
        }
    }
    if tower.config.type_vocab > 0 {
        names.push("embeddings.token_type_embeddings.weight".to_owned());
    }
    if tower.architecture == Architecture::Bert {
        names.push("embeddings.position_embeddings.weight".to_owned());
    }
    names.push("embeddings.LayerNorm.weight".to_owned());
    names.push("embeddings.LayerNorm.bias".to_owned());
    for layer in 0..tower.config.layer_count {
        match tower.architecture {
            Architecture::Bert => {
                for suffix in [
                    "attention.self.query.weight",
                    "attention.self.query.bias",
                    "attention.self.key.weight",
                    "attention.self.key.bias",
                    "attention.self.value.weight",
                    "attention.self.value.bias",
                    "attention.output.dense.weight",
                    "attention.output.dense.bias",
                    "attention.output.LayerNorm.weight",
                    "attention.output.LayerNorm.bias",
                    "intermediate.dense.weight",
                    "intermediate.dense.bias",
                    "output.dense.weight",
                    "output.dense.bias",
                    "output.LayerNorm.weight",
                    "output.LayerNorm.bias",
                ] {
                    names.push(format!("encoder.layer.{layer}.{suffix}"));
                }
            }
            Architecture::Gte | Architecture::ArcticQuery => {
                for suffix in [
                    "attention.qkv_proj.weight",
                    "attention.qkv_proj.bias",
                    "attention.o_proj.weight",
                    "attention.o_proj.bias",
                    "attn_ln.weight",
                    "attn_ln.bias",
                    "mlp.up_gate_proj.weight",
                    "mlp.down_proj.weight",
                    "mlp.down_proj.bias",
                    "mlp_ln.weight",
                    "mlp_ln.bias",
                ] {
                    names.push(format!("encoder.layer.{layer}.{suffix}"));
                }
            }
        }
    }
    if tower.config.dense_out > 0 {
        names.push("dense.weight".to_owned());
        names.push("dense.bias".to_owned());
    }
    names
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod eval_budget_tests {
    use super::*;

    #[test]
    fn eval_splits_bound_padded_tokens_and_preserve_rows() {
        let tokens = TokenBatch::new(
            (0..32).flat_map(|row| vec![row; 512]).collect(),
            vec![1.0; 32 * 512],
            32,
            512,
        )
        .expect("32 full 512-token rows");
        let mut start = 0;
        let mut ids = Vec::new();
        let mut shapes = Vec::new();
        while start < tokens.rows {
            let chunk = next_eval_chunk(&tokens, start).expect("bounded split");
            shapes.push((chunk.rows, chunk.tokens_per_row));
            start += chunk.rows;
            ids.extend(chunk.token_ids);
        }
        assert_eq!(shapes, vec![(8, 512); 4]);
        assert_eq!(ids, tokens.token_ids, "splitting preserves caller order");

        let mut mixed = tokens;
        // First eight rows fill the budget. The remaining rows need only 16
        // positions, including a masked hole that must not shift later tokens.
        for row in 8..32 {
            mixed.attention_mask[row * 512 + 16..(row + 1) * 512].fill(0.0);
            mixed.attention_mask[row * 512 + 3] = 0.0;
        }
        let first = next_eval_chunk(&mixed, 0).expect("long split");
        let rest = next_eval_chunk(&mixed, first.rows).expect("trimmed split");
        assert_eq!((first.rows, first.tokens_per_row), (8, 512));
        assert_eq!((rest.rows, rest.tokens_per_row), (24, 16));
        for (row, ids) in rest.token_ids.chunks_exact(16).enumerate() {
            assert_eq!(ids, vec![(row + 8) as i32; 16]);
        }
        assert_eq!(rest.attention_mask[..16], mixed.attention_mask[4096..4112]);
        let short =
            TokenBatch::new(vec![2; 33 * 128], vec![1.0; 33 * 128], 33, 128).expect("short rows");
        assert_eq!(next_eval_chunk(&short, 0).expect("row ceiling").rows, 32);
        for rows in [1, 2] {
            let over_budget = TokenBatch::new(
                (0..rows)
                    .flat_map(|row| vec![row as i32 + 2; 4097])
                    .collect(),
                vec![1.0; rows * 4097],
                rows,
                4097,
            )
            .expect("over-budget rows");
            for start in 0..rows {
                let chunk = next_eval_chunk(&over_budget, start).expect("long row runs alone");
                assert_eq!((chunk.rows, chunk.tokens_per_row), (1, 4097));
                assert_eq!(chunk.token_ids, vec![start as i32 + 2; 4097]);
            }
        }
    }

    #[test]
    #[ignore = "requires ZE_TEXT_BUNDLE pointing to the release FP16 bundle and native Metal"]
    fn release_fp16_max_length_batch_completes_with_eval_timings() {
        let path = std::env::var_os("ZE_TEXT_BUNDLE").expect("set ZE_TEXT_BUNDLE");
        let bundle = Arc::new(Bundle::open(path).expect("release bundle"));
        let max_tokens = bundle.document_tower().embedding.max_tokens as usize;
        assert_eq!(
            bundle
                .tensor("document/embeddings.word_embeddings.weight")
                .expect("document weights")
                .dtype,
            TensorDtype::F16
        );
        let dims = bundle.document_tower().embedding.dims as usize;
        let mut runtime =
            MlxRuntime::load(bundle, TowerRole::Document).expect("FP16 document tower");
        for (rows, width) in [(32, 1024), (2, max_tokens)] {
            let tokens = TokenBatch::new(
                (0..rows)
                    .flat_map(|row| vec![row as i32 + 2; width])
                    .collect(),
                vec![1.0; rows * width],
                rows,
                width,
            )
            .expect("worst-case batch with distinct rows");
            // eval_chunk asserts the actual shape's budget (allowing single
            // rows), then prints wall time after forced eval and the host copy.
            for _ in 0..3 {
                let output = runtime.embed_batch(&tokens).expect("long-row evaluation");
                assert_eq!((output.rows(), output.dims()), (rows, dims));
                assert!(output.values().iter().all(|value| value.is_finite()));
            }
        }
    }
}
