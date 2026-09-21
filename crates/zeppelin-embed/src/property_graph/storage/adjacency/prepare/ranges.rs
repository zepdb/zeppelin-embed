//! One charged current-range workspace and bounded final-base emission.
use super::*;
use crate::property_graph::storage::artifact::BlockKind;
use crate::property_graph::storage::tree::Key;

struct Workspace<'a> {
    merged: RangeScratch<'a>,
    old: StorageBuffer<'a, Edge>,
    output: StorageBuffer<'a, Edge>,
    deltas: StorageBuffer<'a, DeltaEntry>,
    encoded: StorageBuffer<'a, u8>,
    _charge: StorageReservation<'a>,
}
impl<'a> Workspace<'a> {
    fn new(memory: &'a StorageMemory<'a>, r: &mut TreeResources<'_>) -> Result<Self, TreeError> {
        let charge = memory.reserve(std::mem::size_of::<Self>())?;
        let edge = Edge {
            rel: RelId::new(1).map_err(|_| invalid("range scratch identity"))?,
            neighbor: NodeId::new(1).map_err(|_| invalid("range scratch identity"))?,
        };
        Ok(Self {
            merged: RangeScratch::for_prepare(memory, r)?,
            old: filled(memory, MAX_MERGED_ENTRIES, edge, r)?,
            output: filled(memory, MAX_BASE_ENTRIES + 1, edge, r)?,
            deltas: filled(
                memory,
                MAX_PENDING_ENTRIES,
                DeltaEntry {
                    edge,
                    action: Action::Insert,
                },
                r,
            )?,
            encoded: filled(memory, HEADER_BYTES + MAX_BASE_ENTRIES * 32, 0, r)?,
            _charge: charge,
        })
    }
}
fn filled<'a, T: Copy>(
    memory: &'a StorageMemory<'a>,
    length: usize,
    value: T,
    r: &mut TreeResources<'_>,
) -> Result<StorageBuffer<'a, T>, TreeError> {
    let mut result = StorageBuffer::new(memory, length)?;
    let chunk = (64 * 1024 / std::mem::size_of::<T>().max(1)).max(1);
    let mut position = 0;
    while position < length {
        let count = (length - position).min(chunk);
        r.step((count * std::mem::size_of::<T>()) as u64)?;
        for _ in 0..count {
            result.push(value)?;
        }
        position += count;
    }
    r.step(0)?;
    Ok(result)
}

pub(super) fn apply(
    sink: &mut impl BlockSink,
    roots: &mut GraphRoots,
    changes: &[Change],
    base_sequence: u64,
    context: RangeEditContext,
    memory: &StorageMemory<'_>,
    r: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    let mut workspace = Workspace::new(memory, r)?;
    let mut tree = TreeScratch::for_prepare(memory)?;
    let mut position = 0;
    while let Some(first) = changes.get(position) {
        r.step(1)?;
        let group = first.group();
        let mut group_end = position + 1;
        while let Some(change) = changes.get(group_end) {
            r.step(1)?;
            if change.group() != group {
                break;
            }
            group_end += 1;
        }
        let kind = match first.direction {
            Direction::Out => TreeKind::OutRanges,
            Direction::In => TreeKind::InRanges,
        };
        let mut root = roots.directory(kind)?;
        while position < group_end {
            let first = *changes.get(position).ok_or(TreeError::Memory)?;
            let (key, old, old_count) = select(sink, root, first, context, &mut workspace, r)?;
            let mut end = position;
            while end < group_end {
                r.step(1)?;
                let change = changes.get(end).ok_or(TreeError::Memory)?;
                if matches!(key.upper, UpperBound::Exclusive(upper) if change.delta.edge.rel >= upper)
                {
                    break;
                }
                end += 1;
            }
            if end == position {
                return Err(invalid("adjacency range did not advance"));
            }
            let incoming = changes.get(position..end).ok_or(TreeError::Memory)?;
            root = update(
                sink,
                root,
                key,
                old,
                old_count,
                incoming,
                base_sequence,
                context,
                &mut workspace,
                &mut tree,
                r,
            )?;
            position = end;
        }
        roots.replace(root)?;
    }
    r.step(0)
}

/// Merge the pending deltas of one adjacency range into a bounded base. The
/// range with the most pending edges goes first, then the smallest key. A
/// consolidated range has no pending edges, so successive calls move through
/// every range that needs work, with no persisted cursor. A direction with
/// nothing pending is left alone: rewriting an already bounded base would
/// only copy bytes.
pub(crate) fn consolidate_pending_range(
    sink: &mut impl BlockSink,
    root: DirectoryRoot,
    context: RangeEditContext,
    memory: &StorageMemory<'_>,
    r: &mut TreeResources<'_>,
) -> Result<Option<DirectoryRoot>, TreeError> {
    const RANGE_KEY_BYTES: usize = 40;
    let mut selected: Option<(usize, [u8; RANGE_KEY_BYTES])> = None;
    {
        let mut scan = DirectoryCursor::seek(&*sink, root, None, r)?;
        while let Some(entry) = scan.next_entry(r)? {
            let pending = descriptor(root, entry, r)?.pending_count();
            if pending == 0 || selected.is_some_and(|(most, _)| pending <= most) {
                continue;
            }
            let Key::Inline(key) = entry.key() else {
                return Err(TreeError::Invalid("adjacency range key overflow"));
            };
            selected = Some((
                pending,
                key.try_into()
                    .map_err(|_| TreeError::Invalid("adjacency range key width"))?,
            ));
        }
    }
    let Some((_, selected_key)) = selected else {
        return Ok(None);
    };
    let mut workspace = Workspace::new(memory, r)?;
    let mut cursor = DirectoryCursor::seek(&*sink, root, Some(&selected_key), r)?;
    let Some(entry) = cursor.next_entry(r)? else {
        return Err(TreeError::Invalid("selected adjacency range disappeared"));
    };
    if !matches!(entry.key(), Key::Inline(key) if key == selected_key) {
        return Err(TreeError::Invalid("selected adjacency range disappeared"));
    }
    let descriptor = descriptor(root, entry, r)?;
    let range = validate_range(
        &*sink,
        root,
        entry,
        context.cutoff(entry)?,
        &mut workspace.merged,
        r,
    )?;
    let count = range.edges().len();
    let copied = workspace
        .old
        .as_mut_slice()
        .get_mut(..count)
        .ok_or(TreeError::Memory)?;
    for (target, source) in copied
        .chunks_mut((64 * 1024 / std::mem::size_of::<Edge>()).max(1))
        .zip(
            range
                .edges()
                .chunks((64 * 1024 / std::mem::size_of::<Edge>()).max(1)),
        )
    {
        r.step(std::mem::size_of_val(source) as u64)?;
        target.copy_from_slice(source);
    }
    drop(cursor);
    let mut tree = TreeScratch::for_prepare(memory)?;
    let root = remove_range(
        sink,
        root,
        descriptor,
        context,
        &mut workspace.merged,
        &mut tree,
        r,
    )?;
    let mut writer = BaseWriter {
        sink,
        root,
        context,
        merged: &mut workspace.merged,
        encoded: &mut workspace.encoded,
        tree: &mut tree,
    };
    writer.emit(
        descriptor.key(),
        workspace
            .old
            .as_slice()
            .get(..count)
            .ok_or(TreeError::Memory)?,
        r,
    )?;
    Ok(Some(writer.root))
}

fn descriptor(
    root: DirectoryRoot,
    entry: DirectoryEntry<'_>,
    r: &mut TreeResources<'_>,
) -> Result<RangeDescriptor, TreeError> {
    entry.require_root(root)?;
    r.step((40 + RANGE_DESCRIPTOR_BYTES) as u64)?;
    let Key::Inline(key) = entry.key() else {
        return Err(invalid("overflow adjacency key"));
    };
    RangeDescriptor::decode(root.kind(), key, entry.value())
}
fn same_group(key: RangeKey, change: Change) -> bool {
    key.node == change.node
        && key.rel_type == change.relationship_type
        && key.direction == change.direction
}
fn select(
    source: &impl BlockSource,
    root: DirectoryRoot,
    first: Change,
    context: RangeEditContext,
    workspace: &mut Workspace<'_>,
    r: &mut TreeResources<'_>,
) -> Result<(RangeKey, Option<RangeDescriptor>, usize), TreeError> {
    let mut key = RangeKey {
        node: first.node,
        rel_type: first.relationship_type,
        direction: first.direction,
        lower: first.delta.edge.rel,
        upper: UpperBound::Infinity,
    };
    r.step(40)?;
    let probe = range::directory_key(key)?;
    if let Some(entry) = lookup_predecessor(source, root, &probe, r)? {
        let candidate = descriptor(root, entry, r)?;
        let candidate_key = candidate.key();
        if same_group(candidate_key, first)
            && !matches!(candidate_key.upper, UpperBound::Exclusive(upper) if first.delta.edge.rel >= upper)
        {
            let validated = validate_range(
                source,
                root,
                entry,
                context.cutoff(entry)?,
                &mut workspace.merged,
                r,
            )?;
            let count = validated.edges().len();
            let target = workspace
                .old
                .as_mut_slice()
                .get_mut(..count)
                .ok_or(TreeError::Memory)?;
            for (target, values) in target
                .chunks_mut(64 * 1024 / std::mem::size_of::<Edge>())
                .zip(
                    validated
                        .edges()
                        .chunks(64 * 1024 / std::mem::size_of::<Edge>()),
                )
            {
                r.step(std::mem::size_of_val(values) as u64)?;
                target.copy_from_slice(values);
            }
            return Ok((candidate_key, Some(candidate), count));
        }
    }
    // A missing predecessor interval is a real gap. Its new upper bound stops
    // at the next same-group range, without MAX+1 or an unrelated-prefix scan.
    let mut cursor = DirectoryCursor::seek(source, root, Some(&probe), r)?;
    if let Some(entry) = cursor.next_entry(r)? {
        let following = descriptor(root, entry, r)?;
        if same_group(following.key(), first) {
            if following.key().lower <= key.lower {
                return Err(invalid("adjacency gap routing"));
            }
            key.upper = UpperBound::Exclusive(following.key().lower);
        }
    }
    Ok((key, None, 0))
}

#[allow(clippy::too_many_arguments)]
fn update<'m>(
    sink: &mut impl BlockSink,
    root: DirectoryRoot,
    key: RangeKey,
    old: Option<RangeDescriptor>,
    old_count: usize,
    incoming: &[Change],
    base_sequence: u64,
    context: RangeEditContext,
    workspace: &mut Workspace<'m>,
    tree: &mut TreeScratch<'m>,
    r: &mut TreeResources<'_>,
) -> Result<DirectoryRoot, TreeError> {
    let old_edges = workspace
        .old
        .as_slice()
        .get(..old_count)
        .ok_or(TreeError::Memory)?;
    let mut preflight = Combined::new(old_edges, incoming);
    let mut survivors = 0usize;
    while preflight.next(r)?.is_some() {
        survivors = survivors.checked_add(1).ok_or(TreeError::Work)?;
    }
    if survivors == 0 {
        let old = old.ok_or(invalid("empty new adjacency interval"))?;
        return remove_range(sink, root, old, context, &mut workspace.merged, tree, r);
    }
    let fits = incoming.len() <= MAX_PENDING_ENTRIES
        && matches!(
            append_admission(
                old.map_or(0, |old| old.deltas().count()),
                old.map_or(0, RangeDescriptor::pending_count),
                incoming.len()
            ),
            Ok(Admission::Append)
        );
    if fits {
        let delta_output = workspace
            .deltas
            .as_mut_slice()
            .get_mut(..incoming.len())
            .ok_or(TreeError::Memory)?;
        for (output, change) in delta_output.iter_mut().zip(incoming) {
            r.step(std::mem::size_of::<DeltaEntry>() as u64)?;
            *output = change.delta;
        }
        let base = if let Some(old) = old {
            old.base()
        } else {
            let bytes = encode_base(
                key,
                base_sequence,
                &[],
                workspace.encoded.as_mut_slice(),
                &mut |work| range::checkpoint(r, work),
            )
            .map_err(range::map_error)?;
            sink.append(
                BlockKind::AdjacencyBase,
                context.target_generation(),
                bytes,
                r,
            )?
        };
        let bytes = encode_delta(
            key,
            context.target_sequence(),
            delta_output,
            workspace.encoded.as_mut_slice(),
            &mut |work| range::checkpoint(r, work),
        )
        .map_err(range::map_error)?;
        let delta = sink.append(
            BlockKind::AdjacencyDelta,
            context.target_generation(),
            bytes,
            r,
        )?;
        let mut references = [base; MAX_DELTA_RUNS];
        let mut count = 0;
        if let Some(old) = old {
            for reference in old.deltas() {
                *references.get_mut(count).ok_or(TreeError::Memory)? = reference;
                count += 1;
            }
        }
        *references.get_mut(count).ok_or(TreeError::Memory)? = delta;
        count += 1;
        r.step((RANGE_DESCRIPTOR_BYTES + 40) as u64)?;
        let descriptor = RangeDescriptor::new(
            key,
            old.map_or(base_sequence, RangeDescriptor::watermark),
            old.map_or(0, RangeDescriptor::base_count),
            base,
            references.get(..count).ok_or(TreeError::Memory)?,
            old.map_or(0, RangeDescriptor::pending_count)
                .checked_add(incoming.len())
                .ok_or(TreeError::Memory)?,
        )?;
        return put_range(
            sink,
            root,
            descriptor,
            context,
            &mut workspace.merged,
            tree,
            r,
        );
    }
    // Every old entry and incoming delete/insert has passed preflight. Build
    // only final bases at the one target sequence. Remove first so subsequent
    // mandatory checked edits never see overlapping intermediate intervals.
    let root = match old {
        Some(old) => remove_range(sink, root, old, context, &mut workspace.merged, tree, r)?,
        None => root,
    };
    let mut writer = BaseWriter {
        sink,
        root,
        context,
        merged: &mut workspace.merged,
        encoded: &mut workspace.encoded,
        tree,
    };
    let mut merged = Combined::new(old_edges, incoming);
    let mut count = 0;
    let mut lower = key.lower;
    while let Some(edge) = merged.next(r)? {
        r.step(std::mem::size_of::<Edge>() as u64)?;
        *workspace
            .output
            .as_mut_slice()
            .get_mut(count)
            .ok_or(TreeError::Memory)? = edge;
        count += 1;
        if count == MAX_BASE_ENTRIES + 1 {
            writer.emit(
                RangeKey {
                    lower,
                    upper: UpperBound::Exclusive(edge.rel),
                    ..key
                },
                workspace
                    .output
                    .as_slice()
                    .get(..MAX_BASE_ENTRIES)
                    .ok_or(TreeError::Memory)?,
                r,
            )?;
            lower = edge.rel;
            r.step(std::mem::size_of::<Edge>() as u64)?;
            *workspace
                .output
                .as_mut_slice()
                .first_mut()
                .ok_or(TreeError::Memory)? = edge;
            count = 1;
        }
    }
    if count == 0 {
        return Err(invalid("adjacency survivor preflight differs"));
    }
    writer.emit(
        RangeKey { lower, ..key },
        workspace
            .output
            .as_slice()
            .get(..count)
            .ok_or(TreeError::Memory)?,
        r,
    )?;
    r.step(0)?;
    Ok(writer.root)
}

struct BaseWriter<'a, 'm, S> {
    sink: &'a mut S,
    root: DirectoryRoot,
    context: RangeEditContext,
    merged: &'a mut RangeScratch<'m>,
    encoded: &'a mut StorageBuffer<'m, u8>,
    tree: &'a mut TreeScratch<'m>,
}
impl<S: BlockSink> BaseWriter<'_, '_, S> {
    fn emit(
        &mut self,
        key: RangeKey,
        edges: &[Edge],
        r: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        let bytes = encode_base(
            key,
            self.context.target_sequence(),
            edges,
            self.encoded.as_mut_slice(),
            &mut |work| range::checkpoint(r, work),
        )
        .map_err(range::map_error)?;
        let reference = self.sink.append(
            BlockKind::AdjacencyBase,
            self.context.target_generation(),
            bytes,
            r,
        )?;
        r.step((RANGE_DESCRIPTOR_BYTES + 40) as u64)?;
        let descriptor = RangeDescriptor::new(
            key,
            self.context.target_sequence(),
            edges.len(),
            reference,
            &[],
            0,
        )?;
        self.root = put_range(
            self.sink,
            self.root,
            descriptor,
            self.context,
            self.merged,
            self.tree,
            r,
        )?;
        r.step(0)
    }
}

/// Bounded two-way walk after the existing kernel has merged all old runs.
/// New inserts must be absent and explicit deletes must match an existing raw
/// edge exactly, so a missing reverse entry cannot be hidden by a new run.
struct Combined<'a> {
    old: &'a [Edge],
    incoming: &'a [Change],
    old_at: usize,
    incoming_at: usize,
}
impl<'a> Combined<'a> {
    fn new(old: &'a [Edge], incoming: &'a [Change]) -> Self {
        Self {
            old,
            incoming,
            old_at: 0,
            incoming_at: 0,
        }
    }
    fn next(&mut self, r: &mut TreeResources<'_>) -> Result<Option<Edge>, TreeError> {
        loop {
            r.step(1)?;
            let old = self.old.get(self.old_at).copied();
            let change = self
                .incoming
                .get(self.incoming_at)
                .map(|change| change.delta);
            match (old, change) {
                (None, None) => return Ok(None),
                (Some(old), None) => {
                    self.old_at += 1;
                    return Ok(Some(old));
                }
                (old, Some(change)) if old.is_none_or(|old| change.edge.rel < old.rel) => {
                    if change.action != Action::Insert {
                        return Err(invalid("adjacency delete is absent"));
                    }
                    self.incoming_at += 1;
                    return Ok(Some(change.edge));
                }
                (Some(old), Some(change)) if old.rel < change.edge.rel => {
                    self.old_at += 1;
                    return Ok(Some(old));
                }
                (Some(old), Some(change)) => {
                    if old.neighbor != change.edge.neighbor || change.action != Action::Delete {
                        return Err(invalid("adjacency change topology/presence"));
                    }
                    self.old_at += 1;
                    self.incoming_at += 1;
                }
                (None, Some(_)) => return Err(invalid("adjacency merge branch")),
            }
        }
    }
}
