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
    let mut out = DirectoryBatch::new(memory, changes.len())?;
    let mut incoming = DirectoryBatch::new(memory, changes.len())?;
    let mut descriptors = StorageBuffer::new(
        memory,
        changes.len().checked_mul(2).ok_or(TreeError::Memory)?,
    )?;
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
        let root = roots.directory(kind)?;
        let pending = if kind == TreeKind::OutRanges {
            &mut out
        } else {
            &mut incoming
        };
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
            update(
                sink,
                root,
                key,
                old,
                old_count,
                incoming,
                base_sequence,
                context,
                &mut workspace,
                pending,
                &mut descriptors,
                r,
            )?;
            position = end;
        }
    }
    for (kind, pending) in [(TreeKind::OutRanges, out), (TreeKind::InRanges, incoming)] {
        let root = range::flush_ranges(
            sink,
            roots.directory(kind)?,
            pending,
            context,
            &mut workspace.merged,
            &mut tree,
            r,
        )?;
        roots.replace(root)?;
    }
    for descriptor in descriptors.as_slice() {
        let kind = match descriptor.key().direction {
            Direction::Out => TreeKind::OutRanges,
            Direction::In => TreeKind::InRanges,
        };
        range::check_prepared_range(
            sink,
            roots.directory(kind)?,
            *descriptor,
            context,
            &mut workspace.merged,
            r,
        )?;
    }
    r.step(0)
}

type RangeReplacement = ([u8; 40], Option<RangeDescriptor>);

/// Rewrite payloads in selected packs and merge one remaining pending range.
/// Return descriptors for the caller's single checked directory apply.
#[allow(clippy::too_many_arguments)]
pub(crate) fn relocate_ranges<'m>(
    sink: &mut impl BlockSink,
    root: DirectoryRoot,
    context: RangeEditContext,
    drain: &[crate::property_graph::storage::artifact::ArtifactId],
    swept: &[RelationshipRow],
    resume: &mut Option<[u8; 40]>,
    byte_limit: u64,
    copied_bytes: &mut u64,
    memory: &'m StorageMemory<'m>,
    r: &mut TreeResources<'_>,
) -> Result<StorageBuffer<'m, RangeReplacement>, TreeError> {
    let mut keys = StorageBuffer::new(
        memory,
        crate::property_graph::storage::consolidation::RELOCATION_LIMIT + 1 + swept.len() * 2,
    )?;
    // Locate every required swept copy directly; ordinary relocation/pending
    // selection below visits only a resumable window of unrelated ranges.
    for row in swept {
        let node = if root.kind() == TreeKind::OutRanges {
            row.source
        } else {
            row.target
        };
        let mut probe = [0; 40];
        probe
            .get_mut(..16)
            .ok_or(TreeError::Memory)?
            .copy_from_slice(&node.get().to_le_bytes());
        probe
            .get_mut(16..24)
            .ok_or(TreeError::Memory)?
            .copy_from_slice(&row.relationship_type.get().to_le_bytes());
        probe
            .get_mut(24..)
            .ok_or(TreeError::Memory)?
            .copy_from_slice(&row.rel.get().to_le_bytes());
        let entry = lookup_predecessor(&*sink, root, &probe, r)?
            .ok_or(invalid("swept adjacency absent"))?;
        let descriptor = descriptor(root, entry, r)?;
        let k = descriptor.key();
        if k.node != node
            || k.rel_type != row.relationship_type
            || row.rel < k.lower
            || matches!(k.upper, UpperBound::Exclusive(upper) if row.rel >= upper)
        {
            return Err(invalid("swept adjacency routing"));
        }
        let key = descriptor.directory_key()?;
        if !keys.as_slice().contains(&key) {
            keys.push(key)?;
        }
    }
    let mut pending: Option<(usize, [u8; 40])> = None;
    {
        let lower = *resume;
        let mut scan =
            DirectoryCursor::seek(&*sink, root, lower.as_ref().map(|key| key.as_slice()), r)?;
        let mut visits = 0;
        while visits < crate::property_graph::storage::consolidation::SWEEP_VISIT_LIMIT {
            let Some(entry) = scan.next_entry(r)? else {
                *resume = None;
                break;
            };
            let descriptor = descriptor(root, entry, r)?;
            let key = descriptor.directory_key()?;
            if lower == Some(key) {
                continue;
            }
            visits += 1;
            *resume = Some(key);
            let affected = keys.as_slice().contains(&key);
            let mut relocated = affected;
            if !affected
                && std::iter::once(descriptor.base())
                    .chain(descriptor.deltas())
                    .any(|reference| drain.binary_search(&reference.artifact).is_ok())
            {
                let bytes = std::iter::once(descriptor.base())
                    .chain(descriptor.deltas())
                    .try_fold(0_u64, |total, reference| {
                        total
                            .checked_add(u64::from(reference.length))
                            .ok_or(TreeError::Work)
                    })?;
                let next = copied_bytes.checked_add(bytes).ok_or(TreeError::Work)?;
                // Leftover ranges keep their pack marked; the next cycle
                // drains them. The spare slot is the pending merge's.
                if keys.as_slice().len()
                    < crate::property_graph::storage::consolidation::RELOCATION_LIMIT
                    && (*copied_bytes == 0 || next <= byte_limit)
                {
                    keys.push(key)?;
                    *copied_bytes = next;
                    relocated = true;
                }
            }
            if !relocated
                && descriptor.pending_count() > 0
                && pending.is_none_or(|(count, _)| descriptor.pending_count() > count)
            {
                pending = Some((descriptor.pending_count(), key));
            }
        }
    }
    // Preserve the ordinary one-range pending merge, excluding drained ranges.
    if let Some((_, key)) = pending {
        keys.push(key)?;
    }
    for index in 1..keys.as_slice().len() {
        let mut pos = index;
        while pos > 0
            && crate::property_graph::storage::tree::compare_inline_keys(
                root.kind(),
                keys.as_slice().get(pos - 1).ok_or(TreeError::Memory)?,
                keys.as_slice().get(pos).ok_or(TreeError::Memory)?,
            )?
            .is_gt()
        {
            keys.as_mut_slice().swap(pos - 1, pos);
            pos -= 1;
        }
    }
    let mut replacements = StorageBuffer::new(memory, keys.as_slice().len())?;
    for key in keys.as_slice() {
        replacements.push((
            *key,
            rewrite_range(sink, root, context, key, swept, memory, r)?,
        ))?;
    }
    Ok(replacements)
}

fn rewrite_range(
    sink: &mut impl BlockSink,
    root: DirectoryRoot,
    context: RangeEditContext,
    selected_key: &[u8; 40],
    swept: &[RelationshipRow],
    memory: &StorageMemory<'_>,
    r: &mut TreeResources<'_>,
) -> Result<Option<RangeDescriptor>, TreeError> {
    let mut workspace = Workspace::new(memory, r)?;
    let mut cursor = DirectoryCursor::seek(&*sink, root, Some(selected_key), r)?;
    let Some(entry) = cursor.next_entry(r)? else {
        return Err(TreeError::Invalid("selected adjacency range disappeared"));
    };
    if !matches!(entry.key(), Key::Inline(key) if key == selected_key.as_slice()) {
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
    let mut count = 0;
    for edge in range.edges() {
        r.step(1)?;
        if let Some(row) = swept.iter().find(|row| row.rel == edge.rel) {
            let k = descriptor.key();
            let (node, neighbor) = match k.direction {
                Direction::Out => (row.source, row.target),
                Direction::In => (row.target, row.source),
            };
            if k.node != node || k.rel_type != row.relationship_type || edge.neighbor != neighbor {
                return Err(invalid("swept adjacency topology mismatch"));
            }
            continue;
        }
        *workspace
            .old
            .as_mut_slice()
            .get_mut(count)
            .ok_or(TreeError::Memory)? = *edge;
        count += 1;
    }
    if count == 0 {
        return Ok(None);
    }
    drop(cursor);
    // The range topology is unchanged. Prepare its new payload and descriptor;
    // maintenance combines this replacement with page hints in one tree apply.
    let bytes = encode_base(
        descriptor.key(),
        context.target_sequence(),
        workspace
            .old
            .as_slice()
            .get(..count)
            .ok_or(TreeError::Memory)?,
        workspace.encoded.as_mut_slice(),
        &mut |work| range::checkpoint(r, work),
    )
    .map_err(range::map_error)?;
    let reference = sink.append(
        BlockKind::AdjacencyBase,
        context.target_generation(),
        bytes,
        r,
    )?;
    let replacement = RangeDescriptor::new(
        descriptor.key(),
        context.target_sequence(),
        count,
        reference,
        &[],
        0,
    )?;
    range::check_prepared_range(sink, root, replacement, context, &mut workspace.merged, r)?;
    Ok(Some(replacement))
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
    pending: &mut DirectoryBatch<'m>,
    descriptors: &mut StorageBuffer<'m, RangeDescriptor>,
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
        pending.push(&old.directory_key()?, None, r)?;
        return Ok(root);
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
        queue_range(pending, descriptors, descriptor, r)?;
        return Ok(root);
    }
    // Every old entry and incoming delete/insert has passed preflight. Build
    // only final bases at the one target sequence. Remove first so subsequent
    // mandatory checked edits never see overlapping intermediate intervals.
    if let Some(old) = old {
        pending.push(&old.directory_key()?, None, r)?;
    }
    let mut writer = BaseWriter {
        sink,
        root,
        context,
        merged: &mut workspace.merged,
        encoded: &mut workspace.encoded,
        tree: None,
        pending: Some((pending, descriptors)),
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
    tree: Option<&'a mut TreeScratch<'m>>,
    pending: Option<(
        &'a mut DirectoryBatch<'m>,
        &'a mut StorageBuffer<'m, RangeDescriptor>,
    )>,
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
        if let Some((pending, descriptors)) = &mut self.pending {
            queue_range(pending, descriptors, descriptor, r)?;
        } else {
            self.root = put_range(
                self.sink,
                self.root,
                descriptor,
                self.context,
                self.merged,
                self.tree
                    .as_deref_mut()
                    .ok_or(TreeError::Invalid("missing range tree scratch"))?,
                r,
            )?;
        }
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

fn queue_range(
    pending: &mut DirectoryBatch<'_>,
    descriptors: &mut StorageBuffer<'_, RangeDescriptor>,
    descriptor: RangeDescriptor,
    r: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    let mut bytes = [0; RANGE_DESCRIPTOR_BYTES];
    descriptor.encode(&mut bytes)?;
    pending.push(&descriptor.directory_key()?, Some(&bytes), r)?;
    descriptors.push(descriptor)
}
