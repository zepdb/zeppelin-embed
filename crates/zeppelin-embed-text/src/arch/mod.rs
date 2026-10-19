//! Explicit registered transformer architectures.

pub(crate) mod bert;
// GTE and ArcticQuery share RoPE attention and post-norm gated MLP blocks.
pub(crate) mod gte;
