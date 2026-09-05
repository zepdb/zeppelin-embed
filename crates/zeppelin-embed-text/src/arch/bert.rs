use mlx_rs::ops::indexing::TryIndexOp;
use mlx_rs::{Array, Stream};

use crate::runtime::RuntimeError;
use crate::runtime::mlx::MlxRuntime;
use crate::tower::{Pooling, TokenBatch};

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
    let positions = (0..tokens.rows)
        .flat_map(|_| 0..tokens.tokens_per_row)
        .map(i32::try_from)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| RuntimeError::Shape("position exceeds i32".to_owned()))?;
    let position_ids = Array::from_slice(&positions, &[batch, sequence]);
    let position_states = runtime
        .tensor("embeddings.position_embeddings.weight")?
        .take_axis_device(&position_ids, 0, stream)
        .map_err(mlx)?;
    states = states.add_device(&position_states, stream).map_err(mlx)?;
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
    for layer in 0..runtime.tower().config.layer_count {
        let base = format!("encoder.layer.{layer}");
        let query = linear(runtime, &states, &format!("{base}.attention.self.query"))?;
        let key = linear(runtime, &states, &format!("{base}.attention.self.key"))?;
        let value = linear(runtime, &states, &format!("{base}.attention.self.value"))?;
        let attended = attention(
            query,
            key,
            value,
            batch,
            sequence,
            hidden,
            i32::from(runtime.tower().config.heads),
            &mask,
            stream,
        )?;
        let projected = linear(
            runtime,
            &attended,
            &format!("{base}.attention.output.dense"),
        )?;
        let residual = states.add_device(&projected, stream).map_err(mlx)?;
        states = layer_norm(
            &residual,
            runtime.tensor(&format!("{base}.attention.output.LayerNorm.weight"))?,
            runtime.tensor(&format!("{base}.attention.output.LayerNorm.bias"))?,
            runtime.tower().config.layer_norm_eps,
            stream,
        )?;
        let intermediate = linear(runtime, &states, &format!("{base}.intermediate.dense"))?;
        let intermediate = mlx_rs::nn::gelu(&intermediate).map_err(mlx)?;
        let output = linear(runtime, &intermediate, &format!("{base}.output.dense"))?;
        let residual = states.add_device(&output, stream).map_err(mlx)?;
        states = layer_norm(
            &residual,
            runtime.tensor(&format!("{base}.output.LayerNorm.weight"))?,
            runtime.tensor(&format!("{base}.output.LayerNorm.bias"))?,
            runtime.tower().config.layer_norm_eps,
            stream,
        )?;
    }
    finish(runtime, states, tokens, batch, sequence, hidden)
}

pub(crate) fn linear(
    runtime: &MlxRuntime,
    input: &Array,
    base: &str,
) -> Result<Array, RuntimeError> {
    let stream = runtime.stream()?;
    let weight = runtime.tensor(&format!("{base}.weight"))?;
    let output = input.matmul_device(weight.t(), stream).map_err(mlx)?;
    match runtime.tensor(&format!("{base}.bias")) {
        Ok(bias) => output.add_device(bias, stream).map_err(mlx),
        Err(RuntimeError::MissingTensor(_)) => Ok(output),
        Err(error) => Err(error),
    }
}

pub(crate) fn layer_norm(
    input: &Array,
    weight: &Array,
    bias: &Array,
    epsilon: f32,
    stream: &Stream,
) -> Result<Array, RuntimeError> {
    mlx_rs::fast::layer_norm_device(input, Some(weight), Some(bias), epsilon, stream).map_err(mlx)
}

pub(crate) fn attention_bias(
    tokens: &TokenBatch,
    batch: i32,
    sequence: i32,
    stream: &Stream,
) -> Result<Array, RuntimeError> {
    let mask = Array::from_slice(&tokens.attention_mask, &[batch, 1, 1, sequence]);
    let one = Array::from_f32(1.0);
    let scale = Array::from_f32(1.0e9);
    mask.subtract_device(&one, stream)
        .and_then(|value| value.multiply_device(&scale, stream))
        .map_err(mlx)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn attention(
    query: Array,
    key: Array,
    value: Array,
    batch: i32,
    sequence: i32,
    hidden: i32,
    heads: i32,
    mask: &Array,
    stream: &Stream,
) -> Result<Array, RuntimeError> {
    let head_dim = hidden
        .checked_div(heads)
        .ok_or_else(|| RuntimeError::Shape("attention head count is zero".to_owned()))?;
    let query = query
        .reshape_device(&[batch, sequence, heads, head_dim], stream)
        .and_then(|value| value.transpose_axes_device(&[0, 2, 1, 3], stream))
        .map_err(mlx)?;
    let key = key
        .reshape_device(&[batch, sequence, heads, head_dim], stream)
        .and_then(|value| value.transpose_axes_device(&[0, 2, 1, 3], stream))
        .map_err(mlx)?;
    let value = value
        .reshape_device(&[batch, sequence, heads, head_dim], stream)
        .and_then(|value| value.transpose_axes_device(&[0, 2, 1, 3], stream))
        .map_err(mlx)?;
    mlx_rs::fast::scaled_dot_product_attention_device(
        &query,
        &key,
        &value,
        1.0 / (head_dim as f32).sqrt(),
        mask,
        stream,
    )
    .and_then(|value| value.transpose_axes_device(&[0, 2, 1, 3], stream))
    .and_then(|value| value.reshape_device(&[batch, sequence, hidden], stream))
    .map_err(mlx)
}

pub(crate) fn finish(
    runtime: &MlxRuntime,
    states: Array,
    tokens: &TokenBatch,
    batch: i32,
    sequence: i32,
    hidden: i32,
) -> Result<Array, RuntimeError> {
    let stream = runtime.stream()?;
    let mut pooled = match runtime.tower().pooling {
        Pooling::Cls => states.try_index_device((.., 0, ..), stream).map_err(mlx)?,
        Pooling::Mean => {
            let mask = Array::from_slice(&tokens.attention_mask, &[batch, sequence, 1]);
            let sums = states
                .multiply_device(&mask, stream)
                .and_then(|value| value.sum_axis_device(1, false, stream))
                .map_err(mlx)?;
            let counts = mask.sum_axis_device(1, false, stream).map_err(mlx)?;
            sums.divide_device(&counts, stream).map_err(mlx)?
        }
        Pooling::Last => {
            let mut selectors = vec![0.0_f32; tokens.attention_mask.len()];
            for row in 0..tokens.rows {
                let start = row.saturating_mul(tokens.tokens_per_row);
                let length = tokens
                    .attention_mask
                    .get(start..start.saturating_add(tokens.tokens_per_row))
                    .map(|mask| mask.iter().filter(|value| **value > 0.0).count())
                    .unwrap_or(0);
                if length > 0
                    && let Some(selector) = selectors.get_mut(start.saturating_add(length - 1))
                {
                    *selector = 1.0;
                }
            }
            let selector = Array::from_slice(&selectors, &[batch, sequence, 1]);
            states
                .multiply_device(&selector, stream)
                .and_then(|value| value.sum_axis_device(1, false, stream))
                .map_err(mlx)?
        }
    };
    if runtime.tower().config.dense_out > 0 {
        pooled = linear(runtime, &pooled, "dense")?;
    }
    let output_width = if runtime.tower().config.dense_out > 0 {
        i32::try_from(runtime.tower().config.dense_out)
            .map_err(|_| RuntimeError::Shape("dense width exceeds i32".to_owned()))?
    } else {
        hidden
    };
    let dims = i32::try_from(runtime.tower().embedding.dims)
        .map_err(|_| RuntimeError::Shape("output dimensions exceed i32".to_owned()))?;
    if dims > output_width {
        return Err(RuntimeError::Shape(
            "declared output dimensions exceed architecture output".to_owned(),
        ));
    }
    if dims < output_width {
        pooled = pooled
            .try_index_device((.., 0..dims), stream)
            .map_err(mlx)?;
    }
    Ok(pooled)
}

pub(crate) fn mlx(error: mlx_rs::error::Exception) -> RuntimeError {
    RuntimeError::Mlx(error.to_string())
}
