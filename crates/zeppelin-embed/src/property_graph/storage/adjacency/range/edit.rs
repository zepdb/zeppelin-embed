//! Checked private interval edits. Temporary gaps are allowed during a split;
//! overlapping ranges never enter a candidate used by a subsequent edit.
use super::*;
use crate::property_graph::storage::tree::directory::{
    BlockSink, DirectoryBatch, DirectoryCursor, DirectoryMutation, LeafValidator, TreeScratch,
    insert_checked, lookup_entry, lookup_predecessor, remove_checked,
};

/// Expected and target sequence domains for one changed private preparation.
/// The complete graph producer binds these numbers to its real retained WAL
/// state. This component context does not itself admit a base or publish roots.
#[derive(Clone, Copy, Debug)]
pub struct RangeEditContext {
    base_generation: GraphGeneration,
    base_sequence: u64,
    generation: GraphGeneration,
    sequence: u64,
}
impl RangeEditContext {
    /// Advance generation and WAL sequence independently, without wrapping.
    /// No-op batches retain their original roots and never construct this context.
    pub fn new(base_generation: GraphGeneration, base_sequence: u64) -> Result<Self, TreeError> {
        let generation = base_generation
            .get()
            .checked_add(1)
            .map(GraphGeneration::new)
            .ok_or(invalid("adjacency generation overflow"))?;
        Self::at_generation(base_generation, base_sequence, generation)
    }
    /// Bind edits to the exact commit generation; the sequence still advances once.
    pub fn at_generation(
        base_generation: GraphGeneration,
        base_sequence: u64,
        generation: GraphGeneration,
    ) -> Result<Self, TreeError> {
        if generation <= base_generation {
            return Err(invalid("adjacency target generation"));
        }
        Ok(Self {
            base_generation,
            base_sequence,
            generation,
            sequence: base_sequence
                .checked_add(1)
                .ok_or(invalid("adjacency sequence overflow"))?,
        })
    }
    /// One generation shared by every newly prepared block/page.
    pub const fn target_generation(self) -> GraphGeneration {
        self.generation
    }
    /// One real commit sequence shared by every new run.
    pub const fn target_sequence(self) -> u64 {
        self.sequence
    }
    fn require_root(self, root: DirectoryRoot) -> Result<(), TreeError> {
        direction(root.kind())?;
        if root.generation() != self.base_generation && root.generation() != self.generation {
            return Err(invalid("adjacency edit root generation"));
        }
        Ok(())
    }
    pub(in crate::property_graph::storage) fn cutoff(
        self,
        entry: DirectoryEntry<'_>,
    ) -> Result<u64, TreeError> {
        if entry.creation_generation().get() <= self.base_generation.get() {
            Ok(self.base_sequence)
        } else if entry.creation_generation() == self.generation {
            Ok(self.sequence)
        } else {
            Err(invalid("adjacency edit leaf generation"))
        }
    }
}

struct Values<'a, 'm> {
    context: RangeEditContext,
    scratch: &'a mut RangeScratch<'m>,
}
impl<S: BlockSource> LeafValidator<S> for Values<'_, '_> {
    fn verify(
        &mut self,
        source: &S,
        root: DirectoryRoot,
        entry: DirectoryEntry<'_>,
        r: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        checked_entry(source, root, entry, self.context, self.scratch, r)?;
        Ok(())
    }
}
fn checked_entry(
    source: &impl BlockSource,
    root: DirectoryRoot,
    entry: DirectoryEntry<'_>,
    context: RangeEditContext,
    scratch: &mut RangeScratch<'_>,
    r: &mut TreeResources<'_>,
) -> Result<RangeDescriptor, TreeError> {
    let range = validate_range(source, root, entry, context.cutoff(entry)?, scratch, r)?;
    if range.edges().is_empty() {
        return Err(invalid("empty persisted adjacency range"));
    }
    Ok(range.descriptor())
}

/// Fully validate the supplied value and its neighbors before any immutable edit.
/// Every retained old value is also checked under its original leaf/cutoff.
pub fn put_range(
    sink: &mut impl BlockSink,
    root: DirectoryRoot,
    descriptor: RangeDescriptor,
    context: RangeEditContext,
    scratch: &mut RangeScratch<'_>,
    tree: &mut TreeScratch<'_>,
    r: &mut TreeResources<'_>,
) -> Result<DirectoryRoot, TreeError> {
    validate_put(sink, root, descriptor, context, scratch, r)?;
    let key = descriptor.directory_key()?;
    let mut bytes = [0; RANGE_DESCRIPTOR_BYTES];
    r.step((40 + RANGE_DESCRIPTOR_BYTES) as u64)?;
    descriptor.encode(&mut bytes)?;
    insert_checked(
        sink,
        DirectoryMutation::new(root, context.generation, Values { context, scratch }),
        &key,
        &bytes,
        tree,
        r,
    )
}

/// Remove exactly the already observed descriptor. The old leaf is fully
/// validated before rewriting, including entries unrelated to this removal.
pub fn remove_range(
    sink: &mut impl BlockSink,
    root: DirectoryRoot,
    descriptor: RangeDescriptor,
    context: RangeEditContext,
    scratch: &mut RangeScratch<'_>,
    tree: &mut TreeScratch<'_>,
    r: &mut TreeResources<'_>,
) -> Result<DirectoryRoot, TreeError> {
    context.require_root(root)?;
    r.step(40)?;
    let key = descriptor.directory_key()?;
    let entry = lookup_entry(sink, root, &key, r)?.ok_or(TreeError::Missing)?;
    if checked_entry(sink, root, entry, context, scratch, r)? != descriptor {
        return Err(invalid("adjacency removal descriptor differs"));
    }
    remove_checked(
        sink,
        DirectoryMutation::new(root, context.generation, Values { context, scratch }),
        &key,
        tree,
        r,
    )
}

fn same_group(left: RangeKey, right: RangeKey) -> bool {
    left.node == right.node && left.rel_type == right.rel_type && left.direction == right.direction
}
fn ends_before(upper: UpperBound, lower: RelId) -> bool {
    matches!(upper, UpperBound::Exclusive(upper) if upper <= lower)
}

fn validate_put(
    sink: &impl BlockSource,
    root: DirectoryRoot,
    descriptor: RangeDescriptor,
    context: RangeEditContext,
    scratch: &mut RangeScratch<'_>,
    r: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    context.require_root(root)?;
    if direction(root.kind())? != descriptor.key.direction {
        return Err(invalid("adjacency edit direction"));
    }
    if validate_descriptor(
        sink,
        root.store(),
        context.generation,
        descriptor,
        context.sequence,
        scratch,
        r,
    )?
    .edges()
    .is_empty()
    {
        return Err(invalid("empty persisted adjacency range"));
    }
    let key = descriptor.directory_key()?;
    // A lower ID of one has no previous range within this exact group. We do
    // not manufacture an invalid zero probe or walk an unrelated prior prefix.
    if descriptor.key.lower.get() > 1 {
        let previous_key = directory_key(RangeKey {
            lower: RelId::new(descriptor.key.lower.get() - 1)
                .map_err(|_| invalid("adjacency predecessor identity"))?,
            ..descriptor.key
        })?;
        if let Some(entry) = lookup_predecessor(sink, root, &previous_key, r)? {
            let previous = checked_entry(sink, root, entry, context, scratch, r)?;
            r.step(1)?;
            if same_group(previous.key, descriptor.key)
                && !ends_before(previous.key.upper, descriptor.key.lower)
            {
                return Err(invalid("adjacency predecessor overlap"));
            }
        }
    }
    {
        let mut cursor = DirectoryCursor::seek(sink, root, Some(&key), r)?;
        let mut next = cursor.next_entry(r)?;
        if next.is_some_and(|entry| entry.key() == Key::Inline(&key)) {
            next = cursor.next_entry(r)?;
        }
        if let Some(entry) = next {
            let following = checked_entry(sink, root, entry, context, scratch, r)?;
            r.step(1)?;
            if same_group(following.key, descriptor.key)
                && !ends_before(descriptor.key.upper, following.key.lower)
            {
                return Err(invalid("adjacency successor overlap"));
            }
        }
    }
    Ok(())
}

pub(in crate::property_graph::storage::adjacency) fn flush_ranges(
    sink: &mut impl BlockSink,
    root: DirectoryRoot,
    pending: DirectoryBatch<'_>,
    context: RangeEditContext,
    scratch: &mut RangeScratch<'_>,
    tree: &mut TreeScratch<'_>,
    r: &mut TreeResources<'_>,
) -> Result<DirectoryRoot, TreeError> {
    pending.flush(
        sink,
        DirectoryMutation::new(root, context.generation, Values { context, scratch }),
        tree,
        r,
    )
}

pub(in crate::property_graph::storage::adjacency) fn check_prepared_range(
    sink: &impl BlockSource,
    root: DirectoryRoot,
    descriptor: RangeDescriptor,
    context: RangeEditContext,
    scratch: &mut RangeScratch<'_>,
    r: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    validate_put(sink, root, descriptor, context, scratch, r)
}
