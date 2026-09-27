//! Charged private edits, sorted with the owning tree's exact comparator.
use super::*;
use crate::property_graph::storage::memory::{StorageBuffer, StorageMemory};

struct Edit<'m> {
    key: StorageBuffer<'m, u8>,
    value: Option<StorageBuffer<'m, u8>>,
    ordinal: usize,
}

pub(crate) struct DirectoryBatch<'m> {
    memory: &'m StorageMemory<'m>,
    edits: StorageBuffer<'m, Edit<'m>>,
}
impl<'m> DirectoryBatch<'m> {
    pub(crate) fn new(memory: &'m StorageMemory<'m>, capacity: usize) -> Result<Self, TreeError> {
        Ok(Self {
            memory,
            edits: StorageBuffer::new(memory, capacity)?,
        })
    }
    pub(crate) fn push(
        &mut self,
        key: &[u8],
        value: Option<&[u8]>,
        r: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        // Multiple labels and split adjacency ranges can exceed the entity count.
        // Admit both old and replacement capacities before moving any descriptors.
        if self.edits.as_slice().len() == self.edits.capacity() {
            let capacity = self
                .edits
                .capacity()
                .max(1)
                .checked_mul(2)
                .ok_or(TreeError::Memory)?;
            let mut replacement = StorageBuffer::new(self.memory, capacity)?;
            std::mem::swap(&mut self.edits, &mut replacement);
            for edit in replacement.drain() {
                r.step(1)?;
                self.edits.push(edit)?;
            }
        }
        fn copy<'a>(
            memory: &'a StorageMemory<'a>,
            bytes: &[u8],
            r: &mut TreeResources<'_>,
        ) -> Result<StorageBuffer<'a, u8>, TreeError> {
            let mut result = StorageBuffer::new(memory, bytes.len())?;
            for chunk in bytes.chunks(64 * 1024) {
                r.step(chunk.len() as u64)?;
                result.extend_from_slice(chunk)?;
            }
            Ok(result)
        }
        self.edits.push(Edit {
            key: copy(self.memory, key, r)?,
            value: value.map(|v| copy(self.memory, v, r)).transpose()?,
            ordinal: self.edits.as_slice().len(),
        })
    }
    pub(crate) fn flush<S: BlockSink>(
        mut self,
        sink: &mut S,
        mutation: DirectoryMutation<impl LeafValidator<S>>,
        scratch: &mut TreeScratch<'_>,
        r: &mut TreeResources<'_>,
    ) -> Result<DirectoryRoot, TreeError> {
        let root = mutation.root;
        let edits = self.edits.as_mut_slice();
        for start in (0..edits.len() / 2).rev() {
            sift(edits, start, edits.len(), sink, root, r)?;
        }
        for end in (1..edits.len()).rev() {
            swap(edits, 0, end, r)?;
            sift(edits, 0, end, sink, root, r)?;
        }
        let mut ops = StorageBuffer::new(self.memory, edits.len())?;
        for (index, edit) in edits.iter().enumerate() {
            r.step(1)?;
            if let Some(next) = edits.get(index + 1)
                && compare(
                    sink,
                    root,
                    Key::Inline(edit.key.as_slice()),
                    Key::Inline(next.key.as_slice()),
                    r,
                )?
                .is_eq()
            {
                // Only a checked removal followed by its replacement is legal.
                if edit.value.is_some() || next.value.is_none() {
                    return Err(TreeError::Invalid("duplicate buffered directory edit"));
                }
                continue;
            }
            ops.push(match &edit.value {
                Some(value) => DirectoryOp::Insert {
                    key: edit.key.as_slice(),
                    value: value.as_slice(),
                },
                None => DirectoryOp::Remove {
                    key: edit.key.as_slice(),
                },
            })?;
        }
        apply_prepared_sorted_checked(sink, mutation, ops.as_slice(), scratch, r)
    }
}
fn swap(
    items: &mut [Edit<'_>],
    a: usize,
    b: usize,
    r: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    r.step(1)?;
    let [a, b] = items
        .get_disjoint_mut([a, b])
        .map_err(|_| TreeError::Memory)?;
    std::mem::swap(a, b);
    Ok(())
}
fn sift(
    items: &mut [Edit<'_>],
    mut root: usize,
    end: usize,
    source: &impl BlockSource,
    directory: DirectoryRoot,
    r: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    loop {
        r.step(1)?;
        let child = root
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .ok_or(TreeError::Memory)?;
        if child >= end {
            return Ok(());
        }
        let less = |a: usize, b: usize, r: &mut TreeResources<'_>| -> Result<bool, TreeError> {
            let a = items.get(a).ok_or(TreeError::Memory)?;
            let b = items.get(b).ok_or(TreeError::Memory)?;
            Ok(compare(
                source,
                directory,
                Key::Inline(a.key.as_slice()),
                Key::Inline(b.key.as_slice()),
                r,
            )?
            .then(a.ordinal.cmp(&b.ordinal))
            .is_lt())
        };
        let child = if child + 1 < end && less(child, child + 1, r)? {
            child + 1
        } else {
            child
        };
        if !less(root, child, r)? {
            return Ok(());
        }
        swap(items, root, child, r)?;
        root = child;
    }
}
