//! Exact feature-campaign oracle boundary.
//!
//! Family-specific DTOs, expected-value algorithms, and comparators live in
//! the independent `zeppelin-embed-adversarial-oracle` package. This harness
//! module only re-exports the stable evidence record; it deliberately has no
//! reusable comparison or planted-observation path.

pub use zeppelin_embed_adversarial_oracle::OracleRecord;
