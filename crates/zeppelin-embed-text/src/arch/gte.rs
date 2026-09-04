use mlx_rs::Array;
use mlx_rs::ops::indexing::TryIndexOp;

use crate::arch::bert::{attention, attention_bias, finish, layer_norm, linear, mlx};
use crate::runtime::RuntimeError;
use crate::runtime::mlx::MlxRuntime;
use crate::tower::TokenBatch;

pub(crate) fn forward(runtime: &MlxRuntime, tokens: &TokenBatch) -> Result<Array, RuntimeError> {
    let stream = runtime.stream()?;
    let batch = i32::try_from(tokens.rows)
        .map_err(|_| RuntimeError::Shape("batch rows exceed i32".to_owned()))?;
    let sequence = i32::try_from(tokens.tokens_per_row)
        .map_err(|_| RuntimeError::Shape("token count exceeds i32".to_owned()))?;
    let hidden = i32::try_from(runtime.tower().config.hidden)
        .map_err(|_| RuntimeError::Shape("hidden width exceeds i32".to_owned()))?;
    let ids = Array::from_slice(&tokens.token_ids, &[batch, sequence]);
    let mut states = runtime
        .tensor("embeddings.word_embeddings.weight")?
        .take_axis_device(&ids, 0, stream)
        .map_err(mlx)?;
    if runtime.tower().config.type_vocab > 0 {
        let type_ids = Array::from_slice(&vec![0_i32; tokens.token_ids.len()], &[batch, sequence]);
        let type_states = runtime
            .tensor("embeddings.token_type_embeddings.weight")?
            .take_axis_device(&type_ids, 0, stream)
            .map_err(mlx)?;
        states = states.add_device(&type_states, stream).map_err(mlx)?;
    }
    states = layer_norm(
        &states,
        runtime.tensor("embeddings.LayerNorm.weight")?,
        runtime.tensor("embeddings.LayerNorm.bias")?,
        runtime.tower().config.layer_norm_eps,
        stream,
    )?;
    let mask = attention_bias(tokens, batch, sequence, stream)?;
    let heads = i32::from(runtime.tower().config.heads);
    let head_dim = hidden
        .checked_div(heads)
        .ok_or_else(|| RuntimeError::Shape("attention head count is zero".to_owned()))?;
    for layer in 0..runtime.tower().config.layer_count {
        let base = format!("encoder.layer.{layer}");
        let qkv = linear(runtime, &states, &format!("{base}.attention.qkv_proj"))?;
        let query = qkv
            .try_index_device((.., .., 0..hidden), stream)
            .map_err(mlx)?;
        let key = qkv
            .try_index_device((.., .., hidden..hidden.saturating_mul(2)), stream)
            .map_err(mlx)?;
        let value = qkv
            .try_index_device(
                (.., .., hidden.saturating_mul(2)..hidden.saturating_mul(3)),
                stream,
            )
            .map_err(mlx)?;
        let query = apply_rope(query, batch, sequence, heads, head_dim, runtime)?;
        let key = apply_rope(key, batch, sequence, heads, head_dim, runtime)?;
        let attended = attention(
            query, key, value, batch, sequence, hidden, heads, &mask, stream,
        )?;
        let projected = linear(runtime, &attended, &format!("{base}.attention.o_proj"))?;
        let residual = states.add_device(&projected, stream).map_err(mlx)?;
        states = layer_norm(
            &residual,
            runtime.tensor(&format!("{base}.attn_ln.weight"))?,
            runtime.tensor(&format!("{base}.attn_ln.bias"))?,
            runtime.tower().config.layer_norm_eps,
            stream,
        )?;
        let up_gate = linear(runtime, &states, &format!("{base}.mlp.up_gate_proj"))?;
        let intermediate = i32::try_from(runtime.tower().config.intermediate)
            .map_err(|_| RuntimeError::Shape("intermediate width exceeds i32".to_owned()))?;
        let up = up_gate
            .try_index_device((.., .., 0..intermediate), stream)
            .map_err(mlx)?;
        let gate = up_gate
            .try_index_device(
                (.., .., intermediate..intermediate.saturating_mul(2)),
                stream,
            )
            .map_err(mlx)?;
        let gate = mlx_rs::nn::gelu(&gate).map_err(mlx)?;
        let gated = gate.multiply_device(&up, stream).map_err(mlx)?;
        let down = linear(runtime, &gated, &format!("{base}.mlp.down_proj"))?;
        let residual = states.add_device(&down, stream).map_err(mlx)?;
        states = layer_norm(
            &residual,
            runtime.tensor(&format!("{base}.mlp_ln.weight"))?,
            runtime.tensor(&format!("{base}.mlp_ln.bias"))?,
            runtime.tower().config.layer_norm_eps,
            stream,
        )?;
    }
    finish(runtime, states, tokens, batch, sequence, hidden)
}

fn apply_rope(
    value: Array,
    batch: i32,
    sequence: i32,
    heads: i32,
    head_dim: i32,
    runtime: &MlxRuntime,
) -> Result<Array, RuntimeError> {
    let stream = runtime.stream()?;
    value
        .reshape_device(&[batch, sequence, heads, head_dim], stream)
        .and_then(|value| value.transpose_axes_device(&[0, 2, 1, 3], stream))
        .and_then(|value| {
            mlx_rs::fast::rope_device(
                value,
                head_dim,
                false,
                Some(runtime.tower().config.rope_theta),
                1.0,
                0,
                None,
                stream,
            )
        })
        .and_then(|value| value.transpose_axes_device(&[0, 2, 1, 3], stream))
        .and_then(|value| value.reshape_device(&[batch, sequence, heads * head_dim], stream))
        .map_err(mlx)
}
