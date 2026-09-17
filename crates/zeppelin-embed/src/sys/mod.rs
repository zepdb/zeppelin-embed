//! Operating-system integration wrappers.

#[cfg(unix)]
pub(crate) mod memory;

/// Windows platform boundary: Win32 handles, mappings and memory probes.
///
/// Public for the same reason [`darwin`] is: the native resource gates measure
/// this process through it. No raw handle crosses the module boundary.
#[cfg(windows)]
pub mod windows;

/// Darwin-specific durability and virtual-memory probes.
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod darwin;
