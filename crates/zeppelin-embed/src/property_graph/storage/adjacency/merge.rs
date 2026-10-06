use super::{codec::Run, *};
use std::cmp::Ordering;

/// A nonempty output base's interval and positions in the merged edge prefix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Partition {
    /// Routing interval; adjacent partitions meet at an actual RelId.
    pub key: RangeKey,
    /// Inclusive edge index in Merged::edges.
    pub start: usize,
    /// Exclusive edge index in Merged::edges.
    pub end: usize,
}
/// Fully validated private output; only its successful return makes scratch valid.
#[derive(Debug)]
pub struct Merged<'a> {
    edges: &'a [Edge],
    partitions: Partitions,
    watermark: u64,
}
impl Merged<'_> {
    /// Exact sorted surviving edges, with no hidden capacity exposed.
    pub fn edges(&self) -> &[Edge] {
        self.edges
    }
    /// Zero, one or two nonempty routing partitions.
    pub fn partitions(&self) -> &[Partition] {
        match &self.partitions {
            Partitions::Empty => &[],
            Partitions::One(one) => one,
            Partitions::Two(two) => two,
        }
    }
    /// Committed cutoff through which every supplied run was consolidated.
    pub const fn watermark(&self) -> u64 {
        self.watermark
    }
}
#[derive(Debug)]
enum Partitions {
    Empty,
    One([Partition; 1]),
    Two([Partition; 2]),
}
#[derive(Clone, Copy)]
struct Cursor<'a> {
    run: Run<'a>,
    index: usize,
    head: Option<DeltaEntry>,
}
/// Simultaneously live fixed run/cursor arrays; excludes the ordinary call stack.
/// The real storage adapter reserves these bytes and its caller buffer capacities.
pub const MERGE_STATE_BYTES: usize =
    std::mem::size_of::<[Option<Run<'static>>; MAX_DELTA_RUNS + 1]>()
        + std::mem::size_of::<[Option<Cursor<'static>>; MAX_DELTA_RUNS + 1]>();

/// Validates the entire supplied range before any output copy, then performs
/// a bounded nine-way merge. Errors invalidate private scratch; no valid row
/// escapes a late malformed input, insufficient capacity, or final cancellation.
#[allow(clippy::too_many_arguments)]
pub fn merge<'a, E>(
    key: RangeKey,
    watermark: u64,
    cutoff: u64,
    base: &[u8],
    deltas: &[&[u8]],
    output: &'a mut [Edge],
    c: &mut impl FnMut(Work) -> Result<(), E>,
) -> Result<Merged<'a>, Error<E>> {
    step(c, Work::HeaderBytes(0))?;
    valid_range(key)?;
    if compare(watermark, cutoff, c)? == Ordering::Greater {
        return Err(Error::Format(FormatIssue::Sequence));
    }
    if deltas.len() > MAX_DELTA_RUNS {
        return Err(Error::Limit(LimitIssue::DeltaRuns));
    }
    step(c, Work::MergeRun)?;
    let base = codec::decode(key, false, base, c)?;
    if compare(base.sequence, watermark, c)? != Ordering::Equal {
        return Err(Error::Format(FormatIssue::Sequence));
    }
    let mut runs = [None; MAX_DELTA_RUNS + 1];
    *runs.first_mut().ok_or(Error::Format(FormatIssue::Length))? = Some(base);
    let mut previous = watermark;
    let mut pending = 0;
    for (index, bytes) in deltas.iter().enumerate() {
        step(c, Work::MergeRun)?;
        let run = codec::decode(key, true, bytes, c)?;
        if compare(run.sequence, watermark, c)? != Ordering::Greater
            || compare(run.sequence, cutoff, c)? == Ordering::Greater
            || compare(run.sequence, previous, c)? == Ordering::Less
        {
            return Err(Error::Format(FormatIssue::Sequence));
        }
        pending += run.count;
        if pending > MAX_PENDING_ENTRIES {
            return Err(Error::Limit(LimitIssue::PendingEntries));
        }
        *runs
            .get_mut(index + 1)
            .ok_or(Error::Limit(LimitIssue::DeltaRuns))? = Some(run);
        previous = run.sequence;
    }
    // Preflight all cross-run topology/actions and capacity before private copies.
    let mut needed = 0;
    let mut split = None;
    walk(&runs, c, |edge, _| {
        if needed == MAX_BASE_ENTRIES {
            split = Some(edge.rel);
        }
        needed += 1;
        Ok(())
    })?;
    if needed > output.len() {
        return Err(Error::Limit(LimitIssue::Output));
    }
    let mut written = 0;
    walk(&runs, c, |edge, c| {
        step(c, Work::CopyBytes(std::mem::size_of::<Edge>()))?;
        *output
            .get_mut(written)
            .ok_or(Error::Limit(LimitIssue::Output))? = edge;
        written += 1;
        Ok(())
    })?;
    let whole = Partition {
        key,
        start: 0,
        end: needed,
    };
    let partitions = if let Some(at) = split {
        let first = Partition {
            key: RangeKey {
                upper: UpperBound::Exclusive(at),
                ..key
            },
            end: MAX_BASE_ENTRIES,
            ..whole
        };
        let second = Partition {
            key: RangeKey { lower: at, ..key },
            start: MAX_BASE_ENTRIES,
            ..whole
        };
        Partitions::Two([first, second])
    } else if needed == 0 {
        Partitions::Empty
    } else {
        Partitions::One([whole])
    };
    step(c, Work::Finish)?;
    let edges = output
        .get(..written)
        .ok_or(Error::Limit(LimitIssue::Output))?;
    Ok(Merged {
        edges,
        partitions,
        watermark: cutoff,
    })
}

#[cfg(any(test, feature = "test-seams"))]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
pub(crate) fn exact_split_fixture() {
    let lower = RelId::new(u128::MAX - MAX_BASE_ENTRIES as u128).unwrap();
    let maximum = RelId::new(u128::MAX).unwrap();
    let key = RangeKey {
        node: NodeId::new(u128::MAX).unwrap(),
        rel_type: RelTypeId::new(u64::MAX).unwrap(),
        direction: Direction::Out,
        lower,
        upper: UpperBound::Infinity,
    };
    let base_edges = (0..MAX_BASE_ENTRIES)
        .map(|offset| Edge {
            rel: RelId::new(lower.get() + offset as u128).unwrap(),
            neighbor: NodeId::new(u128::MAX - offset as u128).unwrap(),
        })
        .collect::<Vec<_>>();
    let mut base = vec![0; HEADER_BYTES + MAX_BASE_ENTRIES * 32];
    super::encode_base(key, 40, &base_edges, &mut base, &mut |_| Ok::<_, ()>(())).unwrap();
    let mut delta = vec![0; HEADER_BYTES + 40];
    super::encode_delta(
        key,
        41,
        &[DeltaEntry {
            edge: Edge {
                rel: maximum,
                neighbor: NodeId::new(1).unwrap(),
            },
            action: Action::Insert,
        }],
        &mut delta,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap();
    let mut output = vec![base_edges[0]; MAX_BASE_ENTRIES + 1];
    let merged = merge(key, 40, 41, &base, &[&delta], &mut output, &mut |_| {
        Ok::<_, ()>(())
    })
    .unwrap();
    assert_eq!(merged.edges().len(), MAX_BASE_ENTRIES + 1);
    assert_eq!(merged.edges().last().unwrap().rel, maximum);
    assert_eq!(merged.partitions().len(), 2);
    assert_eq!(merged.partitions()[0].end, MAX_BASE_ENTRIES);
    assert_eq!(
        merged.partitions()[0].key.upper,
        UpperBound::Exclusive(maximum)
    );
    assert_eq!(merged.partitions()[1].key.lower, maximum);
    assert_eq!(merged.partitions()[1].start, MAX_BASE_ENTRIES);
    assert_eq!(merged.partitions()[1].end, MAX_BASE_ENTRIES + 1);
}

fn walk<E>(
    runs: &[Option<Run<'_>>; MAX_DELTA_RUNS + 1],
    c: &mut impl FnMut(Work) -> Result<(), E>,
    mut emit: impl FnMut(Edge, &mut dyn FnMut(Work) -> Result<(), E>) -> Result<(), Error<E>>,
) -> Result<(), Error<E>> {
    let mut cursors = [None; MAX_DELTA_RUNS + 1];
    for (slot, run) in cursors.iter_mut().zip(runs.iter().flatten()) {
        *slot = Some(Cursor {
            run: *run,
            index: 0,
            head: run.entry(0, c)?,
        });
    }
    loop {
        let mut min = None;
        for cursor in cursors.iter().flatten() {
            if let Some(head) = cursor.head {
                let less = match min {
                    Some(id) => compare(head.edge.rel, id, c)? == Ordering::Less,
                    None => true,
                };
                if less {
                    min = Some(head.edge.rel);
                }
            }
        }
        let Some(rel) = min else {
            return Ok(());
        };
        let mut latest: Option<(DeltaEntry, u64)> = None;
        for cursor in cursors.iter_mut().flatten() {
            let Some(head) = cursor.head else {
                continue;
            };
            if compare(head.edge.rel, rel, c)? != Ordering::Equal {
                continue;
            }
            if let Some((old, sequence)) = latest {
                if compare(old.edge.neighbor, head.edge.neighbor, c)? != Ordering::Equal {
                    return Err(Error::Format(FormatIssue::Neighbor));
                }
                if compare(sequence, cursor.run.sequence, c)? == Ordering::Equal {
                    step(c, Work::Compare)?;
                    if old.action != head.action {
                        return Err(Error::Format(FormatIssue::SequenceConflict));
                    }
                }
            }
            latest = Some((head, cursor.run.sequence));
            cursor.index += 1;
            cursor.head = cursor.run.entry(cursor.index, c)?;
        }
        if let Some((winner, _)) = latest
            && winner.action == Action::Insert
        {
            emit(winner.edge, c)?;
        }
    }
}
