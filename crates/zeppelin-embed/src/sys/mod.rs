//! Operating-system integration wrappers.

#[cfg(unix)]
pub(crate) mod memory;

/// Darwin-specific durability and virtual-memory probes.
#[cfg(target_os = "macos")]
pub mod darwin;
