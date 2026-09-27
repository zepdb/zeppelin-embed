//! Fallible physical identity allocation and exclusive immutable-object creation.

use super::artifact::{self, ArtifactId, ArtifactIdentity, Block, ContainerKind};
use crate::property_graph::{GraphGeneration, StoreInstanceId};
use crate::vfs::Vfs;
use std::path::{Path, PathBuf};

/// Injectable OS nonce source. Tests can force entropy failure and collisions.
pub trait EntropyProvider {
    /// Fills every nonce byte or returns the original entropy error.
    fn fill_nonce(&mut self, output: &mut [u8; 16]) -> std::io::Result<()>;
}

/// Native operating-system entropy; no timestamps, counters or retry fallback.
#[derive(Default)]
pub struct OsEntropy;
impl EntropyProvider for OsEntropy {
    fn fill_nonce(&mut self, output: &mut [u8; 16]) -> std::io::Result<()> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            // SAFETY: output owns sixteen writable bytes for this call; this is
            // below getentropy's 256-byte limit. No Rust reference escapes.
            let result = unsafe { libc::getentropy(output.as_mut_ptr().cast(), output.len()) };
            if result == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        }
        #[cfg(windows)]
        {
            crate::sys::windows::fill_entropy(output)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
        {
            let _ = output;
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "native graph OS entropy is unsupported on this platform",
            ))
        }
    }
}

/// Allocates the identity for a fresh root. Reopening never calls this helper.
pub fn fresh_store_identity(entropy: &mut dyn EntropyProvider) -> std::io::Result<StoreInstanceId> {
    StoreInstanceId::new(nonce(entropy)?)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn nonce(entropy: &mut dyn EntropyProvider) -> std::io::Result<u128> {
    let mut bytes = [0_u8; 16];
    entropy.fill_nonce(&mut bytes)?;
    let value = u128::from_le_bytes(bytes);
    if value == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "entropy returned reserved zero identity",
        ));
    }
    Ok(value)
}

/// Distinguishes invalid framing, entropy failure, and filesystem failures.
#[derive(Debug)]
pub enum AllocationError {
    /// Input/framing rejection before object creation.
    Format(crate::format::frame::FormatError),
    /// Entropy source failed or returned a reserved zero nonce.
    Entropy(std::io::Error),
    /// The candidate path already exists and is never owned by this attempt.
    /// It must not enter an abort-cleanup inventory as an owned allocation.
    Collision {
        /// Rejected nonce naming someone else's published/orphan bytes.
        artifact: ArtifactId,
        /// Original AlreadyExists refusal.
        source: std::io::Error,
    },
    /// The exclusive create failed and may have left a newly created prefix.
    /// This identifies a candidate for later recovery classification, not cleanup authority.
    CreateFailed {
        /// Candidate nonce used by the failed call.
        artifact: ArtifactId,
        /// Original filesystem error.
        source: std::io::Error,
    },
}
impl AllocationError {
    /// Identifies a failed exclusive-create attempt for the coordinator's inventory.
    /// This does not grant ownership or permission to remove that path.
    pub const fn attempted_artifact(&self) -> Option<ArtifactId> {
        match self {
            Self::Collision { artifact, .. } | Self::CreateFailed { artifact, .. } => {
                Some(*artifact)
            }
            Self::Format(_) | Self::Entropy(_) => None,
        }
    }
}
impl std::fmt::Display for AllocationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Format(e) => e.fmt(f),
            Self::Entropy(e) => write!(f, "graph entropy: {e}"),
            Self::Collision { artifact, source } => write!(
                f,
                "graph object {:032x} collision; not owned: {source}",
                artifact.get()
            ),
            Self::CreateFailed { artifact, source } => write!(
                f,
                "graph object {:032x} creation failed: {source}",
                artifact.get()
            ),
        }
    }
}
impl std::error::Error for AllocationError {}

/// Physical nonce and exclusive-create participant. It does not sync, publish,
/// or delete; the sole write coordinator owns durability and orphan cleanup.
pub struct ArtifactAllocator<'a> {
    /// Selected VFS must implement atomic exclusive creation.
    pub filesystem: &'a dyn Vfs,
    /// Existing object directory, owned by the write coordinator.
    pub directory: &'a Path,
    /// Persisted store identity, never replaced during reopen.
    pub store: StoreInstanceId,
    /// Explicit production/test nonce source.
    pub entropy: &'a mut dyn EntropyProvider,
}
impl ArtifactAllocator<'_> {
    /// Preflights caller-reserved bytes, draws one nonce, validates the encoded
    /// object and creates it exclusively. A collision aborts without retry.
    pub fn create(
        &mut self,
        generation: GraphGeneration,
        creation_serial: u64,
        blocks: &[Block<'_>],
        output: &mut [u8],
    ) -> Result<ArtifactIdentity, AllocationError> {
        let length = artifact::encoded_len(ContainerKind::Object, blocks)
            .map_err(AllocationError::Format)?;
        if output.len() < length {
            return Err(AllocationError::Format(
                crate::format::frame::FormatError::new(
                    "native graph allocation",
                    crate::format::frame::FormatCheck::Length,
                    "output reservation too small",
                ),
            ));
        }
        let artifact = ArtifactId::new(nonce(self.entropy).map_err(AllocationError::Entropy)?)
            .map_err(AllocationError::Format)?;
        let identity = ArtifactIdentity {
            store: self.store,
            artifact,
            generation,
            creation_serial,
        };
        let used = artifact::encode_into(ContainerKind::Object, identity, blocks, output)
            .map_err(AllocationError::Format)?;
        let bytes = output.get(..used).ok_or_else(|| {
            AllocationError::Format(crate::format::frame::FormatError::new(
                "native graph allocation",
                crate::format::frame::FormatCheck::Length,
                "invalid encoded object extent",
            ))
        })?;
        artifact::decode(ContainerKind::Object, Some((self.store, artifact)), bytes)
            .map_err(AllocationError::Format)?;
        self.filesystem
            .create_new(&artifact_path(self.directory, artifact), bytes)
            .map_err(|source| {
                if source.kind() == std::io::ErrorKind::AlreadyExists {
                    AllocationError::Collision { artifact, source }
                } else {
                    AllocationError::CreateFailed { artifact, source }
                }
            })?;
        Ok(identity)
    }
}

/// Canonical private object filename. IDs cannot choose another path component.
pub fn artifact_path(directory: &Path, artifact: ArtifactId) -> PathBuf {
    directory.join(format!("graph-{:032x}.zgraph", artifact.get()))
}
