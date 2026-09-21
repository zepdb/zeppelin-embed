//! Borrowed logical fence probes avoid copying or persisting lookup text.
use super::*;
use crate::property_graph::catalog::NamespaceId;
use crate::property_graph::storage::payload::prepare_stream;
use crate::property_graph::{EntityKind, MAX_GRAPH_INPUT_BYTES};
use std::cmp::Ordering;

/// A kind-scoped namespace symbol and exact UTF-8 key, including empty/NUL text.
/// The caller retains the key; lookup does not allocate or write artifacts.
#[derive(Clone, Copy, Debug)]
pub struct FenceKey<'a> {
    kind: EntityKind,
    namespace: NamespaceId,
    key: &'a str,
}
impl<'a> FenceKey<'a> {
    /// Check the complete persisted logical-key bound before any preparation.
    pub fn new(kind: EntityKind, namespace: NamespaceId, key: &'a str) -> Result<Self, TreeError> {
        if key
            .len()
            .checked_add(9)
            .is_none_or(|n| n > MAX_GRAPH_INPUT_BYTES)
        {
            return Err(TreeError::Invalid("key logical length"));
        }
        Ok(Self {
            kind,
            namespace,
            key,
        })
    }
    /// Exact entity-kind domain.
    pub const fn kind(self) -> EntityKind {
        self.kind
    }
    /// Numeric namespace domain; little-endian wire order is not comparison order.
    pub const fn namespace(self) -> NamespaceId {
        self.namespace
    }
    /// Borrowed unnormalized UTF-8 bytes.
    pub const fn text(self) -> &'a str {
        self.key
    }
    fn prefix(self) -> (u8, u64) {
        (
            match self.kind {
                EntityKind::Node => 1,
                EntityKind::Relationship => 2,
            },
            self.namespace.get(),
        )
    }
    fn fill(
        self,
        offset: usize,
        output: &mut [u8],
        r: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        let (kind, namespace) = self.prefix();
        let mut prefix = [0; 9];
        *prefix.first_mut().ok_or(TreeError::Memory)? = kind;
        prefix
            .get_mut(1..)
            .ok_or(TreeError::Memory)?
            .copy_from_slice(&namespace.to_le_bytes());
        r.step(output.len() as u64)?;
        let prefix_length = 9usize.saturating_sub(offset).min(output.len());
        output
            .get_mut(..prefix_length)
            .ok_or(TreeError::Memory)?
            .copy_from_slice(
                prefix
                    .get(offset.min(9)..offset.min(9) + prefix_length)
                    .ok_or(TreeError::Memory)?,
            );
        let start = offset
            .checked_add(prefix_length)
            .and_then(|n| n.checked_sub(9))
            .ok_or(TreeError::Memory)?;
        let tail = output.get_mut(prefix_length..).ok_or(TreeError::Memory)?;
        let end = start.checked_add(tail.len()).ok_or(TreeError::Memory)?;
        tail.copy_from_slice(
            self.key
                .as_bytes()
                .get(start..end)
                .ok_or(TreeError::Invalid("key producer extent"))?,
        );
        Ok(())
    }
}
#[derive(Clone, Copy)]
pub(super) enum ProbeKey<'a> {
    Stored(Key<'a>),
    Fence(FenceKey<'a>),
}
impl ProbeKey<'_> {
    pub(super) fn validate(
        self,
        source: &impl BlockSource,
        root: DirectoryRoot,
        r: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        match self {
            Self::Stored(key) => validate_key(source, root, key, r),
            Self::Fence(_) => {
                r.step(1)?;
                if root.kind != TreeKind::KeyFences {
                    return Err(TreeError::Invalid("fence probe tree kind"));
                }
                Ok(())
            }
        }
    }
    /// Return probe-versus-stored ordering, polling every compared byte span.
    pub(super) fn compare_stored(
        self,
        source: &impl BlockSource,
        root: DirectoryRoot,
        stored: Key<'_>,
        r: &mut TreeResources<'_>,
    ) -> Result<Ordering, TreeError> {
        let Self::Fence(probe) = self else {
            let Self::Stored(key) = self else {
                return Err(TreeError::Invalid("key probe"));
            };
            return compare(source, root, key, stored, r);
        };
        r.step(1)?;
        let order = probe.prefix().cmp(&keys::prefix(source, root, stored, r)?);
        if !order.is_eq() {
            return Ok(order);
        }
        let stored_length = keys::length(stored)?
            .checked_sub(9)
            .ok_or(TreeError::Invalid("short fence key"))?;
        let end = stored_length.min(probe.key.len());
        let mut position = 0;
        while position < end {
            let bytes = keys::span(source, root, stored, position + 9, r)?;
            let width = bytes.len().min(end - position);
            if width == 0 {
                return Err(TreeError::Invalid("short fence key stream"));
            }
            r.step(width as u64)?;
            let order = probe
                .key
                .as_bytes()
                .get(position..position + width)
                .ok_or(TreeError::Invalid("probe span"))?
                .cmp(
                    bytes
                        .get(..width)
                        .ok_or(TreeError::Invalid("stored probe span"))?,
                );
            if !order.is_eq() {
                return Ok(order);
            }
            position += width;
        }
        Ok(probe.key.len().cmp(&stored_length))
    }
}
/// Look up a borrowed key without materializing its prefixed or overflow encoding.
pub fn lookup_fence(
    source: &impl BlockSource,
    root: DirectoryRoot,
    key: FenceKey<'_>,
    output: &mut [u8],
    r: &mut TreeResources<'_>,
) -> Result<Option<usize>, TreeError> {
    lookup_probe(source, root, ProbeKey::Fence(key), output, r)
}
/// Stream a long insertion key into private extents; retain compact keys inline.
/// All new extents remain in the owning sink's abort inventory on later failure.
pub fn insert_fence(
    store: &mut impl BlockSink,
    root: DirectoryRoot,
    key: FenceKey<'_>,
    value: &[u8],
    generation: GraphGeneration,
    scratch: &mut TreeScratch<'_>,
    r: &mut TreeResources<'_>,
) -> Result<DirectoryRoot, TreeError> {
    insert_fence_checked(
        store,
        DirectoryMutation::new(root, generation, OpaqueValues),
        key,
        value,
        scratch,
        r,
    )
}
/// Stream an insertion only after every copied old value passes its owning role.
pub fn insert_fence_checked<S: BlockSink>(
    store: &mut S,
    mut mutation: DirectoryMutation<impl LeafValidator<S>>,
    key: FenceKey<'_>,
    value: &[u8],
    scratch: &mut TreeScratch<'_>,
    r: &mut TreeResources<'_>,
) -> Result<DirectoryRoot, TreeError> {
    let (root, generation) = (mutation.root, mutation.generation);
    let probe = ProbeKey::Fence(key);
    probe.validate(store, root, r)?;
    if generation.get() < root.generation.get() {
        return Err(TreeError::Invalid("generation regressed"));
    }
    // Check old contents before even allocating a long new key's private extents.
    find_path(
        store,
        root,
        probe,
        &mut Path::new(),
        Some(&mut mutation.validator),
        r,
    )?;
    let length = key.key.len().checked_add(9).ok_or(TreeError::Memory)?;
    let mut inline = [0; INLINE_BYTES];
    let stored = if length <= INLINE_BYTES {
        let bytes = inline.get_mut(..length).ok_or(TreeError::Memory)?;
        key.fill(0, bytes, r)?;
        Key::Inline(bytes)
    } else {
        let reference = prepare_stream(
            store,
            root.store,
            generation,
            BlockKind::OverflowKey,
            length,
            &mut |offset, bytes, r| {
                key.fill(
                    usize::try_from(offset).map_err(|_| TreeError::Memory)?,
                    bytes,
                    r,
                )
            },
            r,
        )?;
        Key::Overflow {
            logical_length: reference.len(),
            reference: reference.reference(),
        }
    };
    insert_key(
        store,
        mutation,
        InsertionKey { stored, probe },
        value,
        scratch,
        r,
    )
}

/// Borrow a complete fence entry under its actual immutable leaf generation.
pub fn lookup_fence_entry<'a>(
    source: &'a impl BlockSource,
    root: DirectoryRoot,
    key: FenceKey<'_>,
    r: &mut TreeResources<'_>,
) -> Result<Option<DirectoryEntry<'a>>, TreeError> {
    lookup_probe_entry(source, root, ProbeKey::Fence(key), r)
}
impl DirectoryEntry<'_> {
    pub(crate) fn matches_fence<S: BlockSource>(
        self,
        source: &S,
        root: DirectoryRoot,
        kind: EntityKind,
        namespace: NamespaceId,
        text: crate::property_graph::storage::stream::PayloadSlice<'_, S>,
        r: &mut TreeResources<'_>,
    ) -> Result<bool, TreeError> {
        self.require_root(root)?;
        if root.kind != TreeKind::KeyFences || self.generation > root.generation {
            return Err(TreeError::Invalid("fence entry context"));
        }
        let root = DirectoryRoot {
            generation: self.generation,
            ..root
        };
        validate_key(source, root, self.key, r)?;
        let kind = match kind {
            EntityKind::Node => 1,
            EntityKind::Relationship => 2,
        };
        let key_prefix = if source.scoped_blocks() {
            keys::scoped_prefix(source, root, self.key, r)?
        } else {
            keys::prefix(source, root, self.key, r)?
        };
        if key_prefix != (kind, namespace.get())
            || keys::length(self.key)?
                .checked_sub(9)
                .ok_or(TreeError::Invalid("fence key length"))? as u64
                != text.len()
        {
            return Ok(false);
        }
        if source.scoped_blocks() {
            return self.scoped_fence_stream_matches(source, root, text, r);
        }
        let mut position = 0usize;
        while (position as u64) < text.len() {
            let a = keys::span(source, root, self.key, position + 9, r)?;
            let b = text.span_at(position as u64, r)?;
            let n = a.len().min(b.len());
            if n == 0 {
                return Err(TreeError::Invalid("short fence key stream"));
            }
            r.step(n as u64)?;
            if a.get(..n) != b.get(..n) {
                return Ok(false);
            }
            position = position.checked_add(n).ok_or(TreeError::Work)?;
        }
        r.step(0)?;
        Ok(true)
    }

    /// The same exact byte-for-byte fence-stream comparison, copying one bounded
    /// span of each side at a time so neither mapping is retained afterwards.
    fn scoped_fence_stream_matches<S: BlockSource>(
        self,
        source: &S,
        root: DirectoryRoot,
        text: crate::property_graph::storage::stream::PayloadSlice<'_, S>,
        r: &mut TreeResources<'_>,
    ) -> Result<bool, TreeError> {
        let mut key_bytes = [0_u8; crate::property_graph::storage::payload::CHUNK_BYTES];
        let mut text_bytes = [0_u8; crate::property_graph::storage::payload::CHUNK_BYTES];
        let mut position = 0usize;
        while (position as u64) < text.len() {
            let remaining = usize::try_from(text.len() - position as u64)
                .map_err(|_| TreeError::Memory)?
                .min(crate::property_graph::storage::payload::CHUNK_BYTES);
            let n = keys::copy_scoped_span(
                source,
                root,
                self.key,
                position
                    .checked_add(9)
                    .ok_or(TreeError::Invalid("fence key offset"))?,
                key_bytes.get_mut(..remaining).ok_or(TreeError::Memory)?,
                r,
            )?;
            if n == 0 {
                return Err(TreeError::Invalid("short fence key stream"));
            }
            let target = text_bytes.get_mut(..n).ok_or(TreeError::Memory)?;
            if text.read_at(position as u64, target, r)? != n {
                return Err(TreeError::Invalid("short fence text stream"));
            }
            r.step(n as u64)?;
            if key_bytes.get(..n) != text_bytes.get(..n) {
                return Ok(false);
            }
            position = position.checked_add(n).ok_or(TreeError::Work)?;
        }
        r.step(0)?;
        Ok(true)
    }
}
