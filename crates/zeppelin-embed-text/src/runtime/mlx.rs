use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use mlx_rs::{Array, Device, Stream};

use crate::arch::{bert, gte};
use crate::bundle::{Bundle, TensorDtype};
use crate::runtime::{EmbeddingBatch, ModelRuntime, RuntimeError, RuntimeIdentity};
use crate::tower::{Architecture, TokenBatch, TowerRole, TowerSpec};
use zeppelin_embed::epoch::ComputeUnits;

const MAX_EVAL_ROWS: usize = 32;

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
    /// Returns the hard ceiling for one Metal evaluation command buffer.
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
            if descriptor.dtype != TensorDtype::F32 {
                return Err(RuntimeError::UnsupportedDtype(qualified));
            }
            let raw = bundle
                .tensor_bytes(descriptor)
                .map_err(|error| RuntimeError::Shape(error.to_string()))?;
            let mut values = Vec::with_capacity(raw.len() / 4);
            for bytes in raw.chunks_exact(4) {
                let value = bytes
                    .try_into()
                    .ok()
                    .map(u32::from_le_bytes)
                    .map(f32::from_bits)
                    .ok_or_else(|| {
                        RuntimeError::Shape(format!("tensor {qualified} is truncated"))
                    })?;
                values.push(value);
            }
            let shape = descriptor
                .shape
                .iter()
                .map(|dim| i32::try_from(*dim))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| {
                    RuntimeError::Shape(format!("tensor {qualified} dimension exceeds i32"))
                })?;
            tensors.insert(name.to_owned(), Array::from_slice(&values, &shape));
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
        if tokens.rows <= MAX_EVAL_ROWS {
            let result = self.eval_chunk(tokens);
            checkpoint()?;
            return result.map_err(Into::into);
        }
        let dims = self.tower.embedding.dims as usize;
        let capacity = tokens
            .rows
            .checked_mul(dims)
            .ok_or_else(|| RuntimeError::Shape("embedding output size overflow".to_owned()))?;
        let mut values = Vec::with_capacity(capacity);
        let mut start = 0_usize;
        while start < tokens.rows {
            checkpoint()?;
            let end = start.saturating_add(MAX_EVAL_ROWS).min(tokens.rows);
            let row_start = start.checked_mul(tokens.tokens_per_row).ok_or_else(|| {
                RuntimeError::Shape("token batch chunk offset overflow".to_owned())
            })?;
            let row_end = end
                .checked_mul(tokens.tokens_per_row)
                .ok_or_else(|| RuntimeError::Shape("token batch chunk end overflow".to_owned()))?;
            let token_ids = tokens
                .token_ids
                .get(row_start..row_end)
                .ok_or_else(|| RuntimeError::Shape("token batch chunk is truncated".to_owned()))?
                .to_vec();
            let attention_mask = tokens
                .attention_mask
                .get(row_start..row_end)
                .ok_or_else(|| RuntimeError::Shape("attention chunk is truncated".to_owned()))?
                .to_vec();
            let chunk = TokenBatch::new(
                token_ids,
                attention_mask,
                end.saturating_sub(start),
                tokens.tokens_per_row,
            )
            .map_err(|detail| RuntimeError::Shape(detail.to_owned()))?;
            let result = self.eval_chunk(&chunk);
            checkpoint()?;
            values.extend(result?.into_values());
            start = end;
        }
        EmbeddingBatch::new(values, tokens.rows, dims).map_err(Into::into)
    }
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
        let output = match self.tower.architecture {
            Architecture::Bert => bert::forward(self, tokens),
            Architecture::Gte => gte::forward(self, tokens),
        }?;
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
            Architecture::Gte => {
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
