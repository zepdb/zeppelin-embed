//! Operating-system integration wrappers.

/// Darwin-specific durability and virtual-memory probes.
#[cfg(target_os = "macos")]
pub mod darwin;
