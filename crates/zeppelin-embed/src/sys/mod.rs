//! Operating-system integration wrappers.

#[cfg(unix)]
pub(crate) mod memory;

/// Darwin-specific durability and virtual-memory probes.
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod darwin;
