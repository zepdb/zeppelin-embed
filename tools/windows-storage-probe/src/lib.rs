//! W02 native probe: does Windows support the durable-publication protocol the
//! Zeppelin storage engine needs, on standard-user local NTFS?
//!
//! This package is deliberately outside the product workspace, std-only and
//! dependency-free. It has to build and run before `zeppelin-embed` itself is
//! portable, so the experiment cannot live in a core integration test.
//!
//! Nothing here is a product surface. W05 lifts the validated wrappers into
//! `crates/zeppelin-embed/src/sys/windows.rs` and re-proves them through
//! `tests/windows_storage_protocol.rs` against the real engine.

#![cfg(windows)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing
)]

pub mod protocol;
pub mod win32;

use std::io;
use std::path::{Path, PathBuf};

/// A scratch directory under the process temp directory, removed on drop.
///
/// The probe deliberately does not depend on `tempfile`: it must stay
/// dependency-free, and the directory naming is part of what is under test.
#[derive(Debug)]
pub struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// Creates a uniquely named scratch directory.
    ///
    /// `label` becomes part of the name so a leaked directory identifies the
    /// probe that leaked it.
    pub fn new(label: &str) -> io::Result<Self> {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ze-win-probe-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    /// The scratch root.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // A probe that leaves a handle open will fail this removal; that is
        // information, not something to hide, but it must not abort the run.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
