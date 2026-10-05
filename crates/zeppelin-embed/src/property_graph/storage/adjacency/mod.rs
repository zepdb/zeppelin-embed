//! Bounded adjacency codecs, immutable OUT/IN preparation and native reads.
//! The retained source/catalog owner supplies the coherent admitted view. These
//! components prepare private candidates and never publish or acquire a lease.

mod codec;
mod merge;
mod prepare;
mod range;
mod read;
use crate::property_graph::{NodeId, RelId, catalog::RelTypeId};
pub use codec::{encode_base, encode_delta};
#[cfg(any(test, feature = "test-support"))]
pub(crate) use merge::exact_split_fixture;
pub use merge::{MERGE_STATE_BYTES, Merged, Partition, merge};
#[cfg(any(test, feature = "test-support"))]
pub(crate) use prepare::QUALIFICATION_RANGES;
pub(crate) use prepare::relocate_ranges;
pub use prepare::{NativeGraphBase, NativeGraphCandidate, prepare_native_graph};
pub(crate) use range::validate_descriptor;
pub use range::{
    RANGE_DESCRIPTOR_BYTES, RangeDescriptor, RangeEditContext, RangeScratch, ValidatedRange,
    put_range, remove_range, validate_range,
};
pub(crate) use read::ExpansionResume;
pub use read::{
    AdjacencyQuery, AdjacencyRow, NativeGraphReader, RelationshipRange, RelationshipRow,
};

/// Version-one inner header, including all required reserved bytes.
pub const HEADER_BYTES: usize = 96;
/// One immutable base's maximum number of edges.
pub const MAX_BASE_ENTRIES: usize = 4096;
/// Maximum pending immutable runs before consolidation.
pub const MAX_DELTA_RUNS: usize = 8;
/// Maximum total pending observations before consolidation.
pub const MAX_PENDING_ENTRIES: usize = 2048;
/// Largest possible consolidated edge output for one admitted range.
pub const MAX_MERGED_ENTRIES: usize = MAX_BASE_ENTRIES + MAX_PENDING_ENTRIES;

/// Physical directional copy; undirected semantics belong to execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Direction {
    /// Source-bound outgoing copy.
    Out = 1,
    /// Target-bound incoming copy.
    In = 2,
}
/// Exclusive upper endpoint with a distinct, nonwrapping final bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpperBound {
    /// This ID belongs to the following interval, if any.
    Exclusive(RelId),
    /// Includes the maximum representable RelId.
    Infinity,
}
/// Complete immutable adjacency group identity and routing interval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RangeKey {
    /// Endpoint to which this direction is bound.
    pub node: NodeId,
    /// Relationship type of every entry.
    pub rel_type: RelTypeId,
    /// Exactly one physical direction.
    pub direction: Direction,
    /// Inclusive lower endpoint.
    pub lower: RelId,
    /// Exclusive finite endpoint or infinity.
    pub upper: UpperBound,
}
/// Exact stable relationship/neighbor identity; no property duplication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Edge {
    /// All 128 bits of relationship identity.
    pub rel: RelId,
    /// Other endpoint, retained even by a deletion.
    pub neighbor: NodeId,
}
/// One observation in an immutable committed run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Action {
    /// Relationship is present at this sequence.
    Insert = 1,
    /// Relationship is absent at this sequence.
    Delete = 2,
}
/// Typed input to the delta encoder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeltaEntry {
    /// Stable topology, including for Delete.
    pub edge: Edge,
    /// Committed observation.
    pub action: Action,
}
/// Compact inner-format diagnostics; constructing these never allocates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatIssue {
    /// Unknown magic, version, kind, direction, upper or action tag.
    Tag,
    /// Nonzero reserved bits or noncanonical infinity payload.
    Reserved,
    /// Truncated/trailing bytes or inconsistent count/width.
    Length,
    /// Zero entity or symbol identity.
    Identity,
    /// Reversed/empty interval or an entry outside it.
    Range,
    /// Non-strict numeric RelId order inside a run.
    Order,
    /// Bound node/type/direction/range differs from the expected group.
    Group,
    /// Watermark/cutoff or run sequence is inconsistent.
    Sequence,
    /// Equal sequence assigns different actions to one RelId.
    SequenceConflict,
    /// Any supplied observations disagree on a RelId's neighbor.
    Neighbor,
}
/// Fixed component limits and caller capacity, never relaxed automatically.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LimitIssue {
    /// Base exceeds 4096 entries.
    BaseEntries,
    /// More than eight pending runs.
    DeltaRuns,
    /// More than 2048 pending entries.
    PendingEntries,
    /// Caller did not provide enough output slots/bytes.
    Output,
}
/// Typed inner error preserves the caller's cancellation/resource error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error<E> {
    /// Malformed immutable payload or incompatible group.
    Format(FormatIssue),
    /// Component/caller capacity exceeded.
    Limit(LimitIssue),
    /// Exact caller failure, without formatting or allocating.
    Control(E),
}
/// Work reported before each actual bounded unit; Finish gates success.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Work {
    /// Header bytes examined (at most 96).
    HeaderBytes(usize),
    /// Entry bytes decoded/validated (32 or 40).
    EntryBytes(usize),
    /// One actual ID/sequence/topology comparison.
    Compare,
    /// Bytes written into private caller scratch (at most 96).
    CopyBytes(usize),
    /// Last checkpoint before returning valid output.
    Finish,
}
/// Admission result before constructing a new immutable run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Admission {
    /// Property-only/empty change creates no delta.
    NoChange,
    /// The new run fits both pending bounds.
    Append,
    /// Consolidate the existing group before adding this run.
    ConsolidateFirst,
}
/// Checks the ninth-run/2049th-observation threshold without allocating.
/// An oversized incoming run needs caller partitioning within its atomic batch.
pub fn append_admission(
    existing_runs: usize,
    pending_entries: usize,
    incoming_entries: usize,
) -> Result<Admission, LimitIssue> {
    if existing_runs > MAX_DELTA_RUNS {
        return Err(LimitIssue::DeltaRuns);
    }
    if pending_entries > MAX_PENDING_ENTRIES || incoming_entries > MAX_PENDING_ENTRIES {
        return Err(LimitIssue::PendingEntries);
    }
    if incoming_entries == 0 {
        return Ok(Admission::NoChange);
    }
    if existing_runs == MAX_DELTA_RUNS || pending_entries + incoming_entries > MAX_PENDING_ENTRIES {
        Ok(Admission::ConsolidateFirst)
    } else {
        Ok(Admission::Append)
    }
}
fn step<E>(c: &mut (impl FnMut(Work) -> Result<(), E> + ?Sized), w: Work) -> Result<(), Error<E>> {
    c(w).map_err(Error::Control)
}
fn compare<T: Ord, E>(
    left: T,
    right: T,
    c: &mut impl FnMut(Work) -> Result<(), E>,
) -> Result<std::cmp::Ordering, Error<E>> {
    step(c, Work::Compare)?;
    Ok(left.cmp(&right))
}
fn valid_range<E>(key: RangeKey) -> Result<(), Error<E>> {
    if let UpperBound::Exclusive(upper) = key.upper
        && key.lower >= upper
    {
        return Err(Error::Format(FormatIssue::Range));
    }
    Ok(())
}
fn valid_edge<E>(
    key: RangeKey,
    edge: Edge,
    previous: Option<RelId>,
    c: &mut impl FnMut(Work) -> Result<(), E>,
) -> Result<(), Error<E>> {
    use std::cmp::Ordering;
    if let Some(previous) = previous
        && compare(previous, edge.rel, c)? != Ordering::Less
    {
        return Err(Error::Format(FormatIssue::Order));
    }
    if compare(edge.rel, key.lower, c)? == Ordering::Less {
        return Err(Error::Format(FormatIssue::Range));
    }
    if let UpperBound::Exclusive(upper) = key.upper
        && compare(edge.rel, upper, c)? != Ordering::Less
    {
        return Err(Error::Format(FormatIssue::Range));
    }
    Ok(())
}

#[cfg(all(test, feature = "allocation-audit"))]
mod tests;
