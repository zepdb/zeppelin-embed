//! Checked immutable graph containers. These codecs do not admit a GraphStore,
//! publish roots, replay a WAL, or authorize cleanup.

pub mod allocation;
pub mod artifact;
pub mod tree;
