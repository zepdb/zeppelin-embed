//! Path-independent namespace metadata and coordinator-only preparation authority.
use super::*;

pub(super) const ROOT_MAGIC: &[u8] = b"ZENS0003";
const REFERENCE_MAGIC: &[u8] = b"ZENR0002";

/// Random, nonzero identity of a complete namespace root, independent of its path.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct NamespaceRootId(u128);

impl NamespaceRootId {
    /// Validates a persisted identity; zero is reserved and always refused.
    pub fn new(value: u128) -> std::io::Result<Self> {
        if value == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "zero namespace root identity",
            ));
        }
        Ok(Self(value))
    }

    /// Allocates one identity from OS randomness. Entropy errors and zero propagate.
    pub fn generate() -> std::io::Result<Self> {
        Self::from_entropy(fill_entropy)
    }

    /// Returns the persisted numeric identity.
    pub const fn get(self) -> u128 {
        self.0
    }

    pub(super) fn from_entropy(
        fill: impl FnOnce(&mut [u8; 16]) -> std::io::Result<()>,
    ) -> std::io::Result<Self> {
        let mut bytes = [0; 16];
        fill(&mut bytes)?;
        Self::new(u128::from_le_bytes(bytes))
    }
}

fn fill_entropy(output: &mut [u8; 16]) -> std::io::Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        // SAFETY: output owns sixteen writable bytes, below getentropy's 256-byte limit.
        if unsafe { libc::getentropy(output.as_mut_ptr().cast(), output.len()) } == 0 {
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
            "namespace OS entropy is unsupported on this platform",
        ))
    }
}

fn checked_body<'a>(path: &Path, bytes: &'a [u8], magic: &[u8]) -> Result<&'a [u8], StoreError> {
    let end = bytes
        .len()
        .checked_sub(8)
        .ok_or_else(|| invalid(path, "short portable record"))?;
    let prefix = bytes
        .get(..end)
        .ok_or_else(|| invalid(path, "portable record bounds"))?;
    if bytes.len() > MAX_RECORD
        || !prefix.starts_with(magic)
        || bytes.get(end..) != Some(xxh3_64(prefix).to_le_bytes().as_slice())
    {
        return Err(invalid(path, "portable record checksum/version"));
    }
    prefix
        .get(magic.len()..)
        .ok_or_else(|| invalid(path, "portable record header"))
}

fn take<'a>(path: &Path, bytes: &mut &'a [u8], n: usize) -> Result<&'a [u8], StoreError> {
    let value = bytes
        .get(..n)
        .ok_or_else(|| invalid(path, "truncated portable record"))?;
    *bytes = bytes
        .get(n..)
        .ok_or_else(|| invalid(path, "portable record bounds"))?;
    Ok(value)
}

fn integer<const N: usize>(path: &Path, bytes: &mut &[u8]) -> Result<[u8; N], StoreError> {
    take(path, bytes, N)?
        .try_into()
        .map_err(|_| invalid(path, "portable integer"))
}

fn finish(mut bytes: Vec<u8>) -> Vec<u8> {
    bytes.extend_from_slice(&xxh3_64(&bytes).to_le_bytes());
    bytes
}

pub(super) fn participant_id(id: NamespaceRootId, name: &str) -> Result<u128, StoreError> {
    let path = Path::new(REFERENCE);
    if !name_valid(name) {
        return Err(invalid(path, "portable namespace name"));
    }
    let length = u16::try_from(name.len()).map_err(|_| invalid(path, "portable name length"))?;
    let mut bytes = b"ZENSPID1".to_vec();
    bytes.extend_from_slice(&id.get().to_le_bytes());
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(name.as_bytes());
    let identity = xxhash_rust::xxh3::xxh3_128(&bytes);
    if identity == 0 {
        return Err(invalid(path, "zero portable participant identity"));
    }
    Ok(identity)
}

pub(super) fn encode_root(
    id: NamespaceRootId,
    descriptor: &RootDescriptor,
) -> Result<Vec<u8>, StoreError> {
    let path = Path::new(RECORD);
    let descriptor = match descriptor {
        RootDescriptor::Legacy(routes) => {
            let bytes = encode(routes);
            decode_routes(path, &bytes)?;
            bytes
        }
        RootDescriptor::Staged(staged) => {
            for (name, selection) in &staged.0 {
                if selection.binding.participant != participant_id(id, name)? {
                    return Err(invalid(path, "portable staged participant identity"));
                }
            }
            encode_staged(staged)?
        }
    };
    let length =
        u32::try_from(descriptor.len()).map_err(|_| invalid(path, "portable descriptor length"))?;
    let mut bytes = ROOT_MAGIC.to_vec();
    bytes.extend_from_slice(&id.get().to_le_bytes());
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(&descriptor);
    let bytes = finish(bytes);
    if bytes.len() > MAX_RECORD {
        return Err(invalid(path, "portable root too large"));
    }
    Ok(bytes)
}

pub(super) fn decode_root(
    path: &Path,
    bytes: &[u8],
) -> Result<(NamespaceRootId, RootDescriptor), StoreError> {
    let mut remaining = checked_body(path, bytes, ROOT_MAGIC)?;
    let id = NamespaceRootId::new(u128::from_le_bytes(integer(path, &mut remaining)?))
        .map_err(|e| io(path, e))?;
    let length = usize::try_from(u32::from_le_bytes(integer(path, &mut remaining)?))
        .map_err(|_| invalid(path, "portable descriptor length"))?;
    // Only canonical v1/v2 descriptors are accepted: a nested v3 is refused here.
    let descriptor = decode_descriptor(path, take(path, &mut remaining, length)?)?;
    if !remaining.is_empty() || encode_root(id, &descriptor)? != bytes {
        return Err(invalid(path, "noncanonical portable root"));
    }
    Ok((id, descriptor))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Reference {
    pub root_id: NamespaceRootId,
    pub parent_depth: u8,
    pub name: String,
}

pub(super) fn encode_reference(reference: &Reference) -> Result<Vec<u8>, StoreError> {
    let path = Path::new(REFERENCE);
    if !matches!(reference.parent_depth, 1 | 2) || !name_valid(&reference.name) {
        return Err(invalid(path, "portable reference name/depth"));
    }
    let length =
        u16::try_from(reference.name.len()).map_err(|_| invalid(path, "portable name length"))?;
    let mut bytes = REFERENCE_MAGIC.to_vec();
    bytes.extend_from_slice(&reference.root_id.get().to_le_bytes());
    bytes.extend_from_slice(&[reference.parent_depth, 0]);
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(reference.name.as_bytes());
    Ok(finish(bytes))
}

pub(super) fn decode_reference(path: &Path, bytes: &[u8]) -> Result<Reference, StoreError> {
    let mut remaining = checked_body(path, bytes, REFERENCE_MAGIC)?;
    let root_id = NamespaceRootId::new(u128::from_le_bytes(integer(path, &mut remaining)?))
        .map_err(|e| io(path, e))?;
    let [parent_depth, reserved] = integer(path, &mut remaining)?;
    let length = usize::from(u16::from_le_bytes(integer(path, &mut remaining)?));
    let name = std::str::from_utf8(take(path, &mut remaining, length)?)
        .map_err(|_| invalid(path, "portable name ASCII"))?
        .to_owned();
    let reference = Reference {
        root_id,
        parent_depth,
        name,
    };
    if reserved != 0 || !remaining.is_empty() || encode_reference(&reference)? != bytes {
        return Err(invalid(path, "noncanonical portable reference"));
    }
    Ok(reference)
}

/// A portable reference prescribes the relative root; no path hash is tried on failure.
pub(super) fn authority<'a>(
    vfs: &dyn Vfs,
    directory: &'a Path,
) -> Result<Option<(&'a Path, &'a str, NamespaceRootId)>, StoreError> {
    authority_with_preparation(vfs, directory, None)
}

pub(super) fn authority_with_preparation<'a>(
    vfs: &dyn Vfs,
    directory: &'a Path,
    preparation: Option<&PrivatePreparation>,
) -> Result<Option<(&'a Path, &'a str, NamespaceRootId)>, StoreError> {
    let reference_path = directory.join(REFERENCE);
    let Some(reference) = read_optional_vfs(vfs, &reference_path)? else {
        // Only a reference claims portable membership. Absence is a plain store.
        return Ok(None);
    };
    let Some(parent) = directory.parent() else {
        return Ok(None);
    };
    if !reference.starts_with(REFERENCE_MAGIC) {
        // A legacy reference cannot authorize membership in a portable root.
        let portable_parent = root_record_vfs(vfs, parent)?.0.is_some();
        let portable_ancestor = if parent
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".ze-batch-"))
        {
            match parent.parent() {
                Some(root) => root_record_vfs(vfs, root)?.0.is_some(),
                None => false,
            }
        } else {
            false
        };
        if portable_parent || portable_ancestor {
            return Err(invalid(
                &reference_path,
                "portable namespace requires its portable reference",
            ));
        }
        return Ok(None);
    }
    let reference = decode_reference(&reference_path, &reference)?;
    let name = directory
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid(directory, "portable participant name"))?;
    if name != reference.name {
        return Err(invalid(directory, "portable namespace name mismatch"));
    }
    let root = if reference.parent_depth == 1 {
        parent
    } else {
        parent
            .parent()
            .ok_or_else(|| invalid(directory, "portable relative root"))?
    };
    let (id, descriptor) = root_record_vfs(vfs, root)?;
    if id != Some(reference.root_id) {
        return Err(invalid(
            directory,
            "portable namespace requires its matching root",
        ));
    }
    if let Some(preparation) = preparation
        && (reference.parent_depth != 2
            || preparation.root_id != reference.root_id
            || preparation.name != name
            || preparation.directory != directory
            || parent.file_name().and_then(|s| s.to_str()).is_none_or(|s| {
                !s.starts_with(".ze-batch-")
                    || s.contains("..")
                    || !s
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
            }))
    {
        return Err(invalid(directory, "private preparation authority mismatch"));
    }
    if reference.parent_depth == 2 && preparation.is_none() {
        let routes = match &descriptor {
            RootDescriptor::Legacy(routes) => routes,
            RootDescriptor::Staged(staged) => &staged.1,
        };
        if routes
            .get(name)
            .is_none_or(|route| root.join(route) != directory)
        {
            return Err(invalid(
                directory,
                "portable participant is outside its selected route",
            ));
        }
    }
    for path in [directory, parent] {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(invalid(path, "portable participant location is a symlink"));
            }
            Ok(_) => {}
            // Memory/crash VFS fixtures need not have corresponding OS directories.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(path, error)),
        }
    }
    Ok(Some((root, name, reference.root_id)))
}
