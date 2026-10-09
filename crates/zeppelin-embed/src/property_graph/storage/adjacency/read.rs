//! Component reads over one retained immutable root bundle. The caller supplies
//! its admitted source/catalog lease; constructing this reader does not admit it.
use super::*;
use crate::epoch::EmbeddingTower;
use crate::property_graph::query::resources::QueryArena;
use crate::property_graph::storage::{
    payload::PayloadRef,
    records::{NodeRecordState, RecordCatalog, RecordShape, verify_record},
    stream::PayloadSlice,
    tree::{Key, TreeKind, directory::*},
    view::lookup_node_state,
};
use crate::property_graph::{EntityId, MAX_GRAPH_CHANGES};

/// Complete fixed authoritative relationship topology, without properties.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RelationshipRow {
    /// Full stable relationship identity.
    pub rel: RelId,
    /// Directed source.
    pub source: NodeId,
    /// Directed target.
    pub target: NodeId,
    /// Exact catalog type identity.
    pub relationship_type: RelTypeId,
}
/// Half-open relationship identity filter; infinity includes u128::MAX.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RelationshipRange {
    /// Inclusive first eligible identity.
    pub lower: RelId,
    /// Exclusive last identity or explicit infinity.
    pub upper: UpperBound,
}
/// One bound directional query, with optional exact type filtering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdjacencyQuery {
    /// Endpoint whose directional ranges are sought.
    pub node: NodeId,
    /// OUT and IN remain separate, including for a self-loop.
    pub direction: Direction,
    /// None visits every type in numeric type order.
    pub relationship_type: Option<RelTypeId>,
    /// Relationship filter applied inside each selected type.
    pub relationships: RelationshipRange,
}
/// One observable directional row; the bound endpoint is supplied by the query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdjacencyRow {
    /// Exact type of this row.
    pub relationship_type: RelTypeId,
    /// Stable relationship and far endpoint.
    pub edge: Edge,
}

/// Exact immutable range and inclusive physical relationship position for the
/// next expansion pull. `After` is the exclusive boundary after one descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExpansionResume {
    Inclusive {
        descriptor: RangeKey,
        relationship: RelId,
    },
    After {
        descriptor: RangeKey,
    },
}

/// Source-bound component reader. A returned row always agrees with the native
/// relationship and has two live endpoints. Missing required endpoints are
/// corruption; explicit retained tombstones make the edge invisible.
pub struct NativeGraphReader<'a, S, C> {
    source: &'a S,
    roots: GraphRoots,
    cutoff: u64,
    catalog: &'a C,
    document: Option<&'a EmbeddingTower>,
}
impl<'a, S: BlockSource, C: RecordCatalog<S>> NativeGraphReader<'a, S, C> {
    /// Borrow one coherent retained source/catalog and its real committed cutoff.
    /// This metadata constructor does not replace ZE45's lease admission.
    pub const fn new(
        source: &'a S,
        roots: GraphRoots,
        cutoff: u64,
        catalog: &'a C,
        document: Option<&'a EmbeddingTower>,
    ) -> Self {
        Self {
            source,
            roots,
            cutoff,
            catalog,
            document,
        }
    }
    /// Point lookup with the same endpoint predicate as scan/expand/count.
    pub fn relationship(
        &self,
        rel: RelId,
        r: &mut TreeResources<'_>,
    ) -> Result<Option<RelationshipRow>, TreeError> {
        let row = self.raw_relationship(rel, r)?;
        let row = match row {
            Some(row) if self.visible(row, r)? => Some(row),
            _ => None,
        };
        r.step(0)?;
        Ok(row)
    }
    /// Numeric relationship scan. Visibility is checked before consuming caller
    /// capacity. On error the caller discards all partially written private rows.
    pub fn scan_relationships(
        &self,
        range: RelationshipRange,
        output: &mut [RelationshipRow],
        r: &mut TreeResources<'_>,
    ) -> Result<usize, TreeError> {
        check_range(range)?;
        r.step(0)?;
        if output.is_empty() {
            return Ok(0);
        }
        let mut count = 0;
        self.visit_relationships(range, r, &mut |row, r| {
            let target = output.get_mut(count).ok_or(TreeError::Memory)?;
            r.step(std::mem::size_of::<RelationshipRow>() as u64)?;
            r.read_event(NativeReadEvent::CopiedBytes(
                std::mem::size_of::<RelationshipRow>() as u64,
            ))?;
            *target = row;
            count += 1;
            Ok(count < output.len())
        })?;
        r.step(0)?;
        Ok(count)
    }
    #[allow(dead_code, reason = "used by the crate-private ZE-45 scoped adapter")]
    pub(crate) fn scan_relationships_after<'m, 'g>(
        &self,
        after: Option<RelId>,
        accepted_types: Option<&[RelTypeId]>,
        output: &mut QueryArena<'m, 'g, RelationshipRow>,
        r: &mut TreeResources<'_>,
    ) -> Result<usize, TreeError> {
        r.step(0)?;
        if output.capacity() == 0 {
            return Err(TreeError::Invalid("zero relationship scan capacity"));
        }
        let lower = after.unwrap_or(RelId::new(1).map_err(|_| invalid("minimum relationship"))?);
        let mut count = 0;
        self.visit_relationships(
            RelationshipRange {
                lower,
                upper: UpperBound::Infinity,
            },
            r,
            &mut |row, r| {
                if after == Some(row.rel)
                    || accepted_types.is_some_and(|types| !types.contains(&row.relationship_type))
                {
                    return Ok(true);
                }
                r.step(std::mem::size_of::<RelationshipRow>() as u64)?;
                r.read_event(NativeReadEvent::CopiedBytes(
                    std::mem::size_of::<RelationshipRow>() as u64,
                ))?;
                output.push(row).map_err(|_| TreeError::Memory)?;
                count += 1;
                Ok(count < output.capacity())
            },
        )?;
        r.step(0)?;
        Ok(count)
    }
    /// Exact observable relationship count; no stale directory-size shortcut.
    pub fn relationship_count(&self, r: &mut TreeResources<'_>) -> Result<u64, TreeError> {
        let mut count = 0u64;
        self.visit_relationships(all_relationships()?, r, &mut |_, r| {
            r.step(1)?;
            count = count.checked_add(1).ok_or(TreeError::Work)?;
            Ok(true)
        })?;
        r.step(0)?;
        Ok(count)
    }
    /// Expand actual OUT/IN ranges in exact type/RelId order. The output limit
    /// counts visible rows only; no complete incident collection is allocated.
    pub fn expand(
        &self,
        query: AdjacencyQuery,
        output: &mut [AdjacencyRow],
        scratch: &mut RangeScratch<'_>,
        r: &mut TreeResources<'_>,
    ) -> Result<usize, TreeError> {
        scratch.require_owner(r)?;
        check_range(query.relationships)?;
        r.step(0)?;
        if output.is_empty() {
            return Ok(0);
        }
        let mut count = 0;
        self.visit_adjacency(query, scratch, r, &mut |row, r| {
            let target = output.get_mut(count).ok_or(TreeError::Memory)?;
            r.step(std::mem::size_of::<AdjacencyRow>() as u64)?;
            r.read_event(NativeReadEvent::CopiedBytes(
                std::mem::size_of::<AdjacencyRow>() as u64,
            ))?;
            *target = row;
            count += 1;
            Ok(count < output.len())
        })?;
        r.step(0)?;
        Ok(count)
    }
    /// Pull from one exact physical range/relationship position. Resume never
    /// replays visible rows from the beginning, and `After` avoids incrementing
    /// `u128::MAX` at a descriptor boundary.
    #[allow(dead_code, reason = "used by the crate-private ZE-45 scoped adapter")]
    pub(crate) fn expand_from<'m, 'g>(
        &self,
        query: AdjacencyQuery,
        resume: Option<ExpansionResume>,
        bound_live: &mut Option<bool>,
        output: &mut QueryArena<'m, 'g, RelationshipRow>,
        scratch: &mut RangeScratch<'_>,
        r: &mut TreeResources<'_>,
    ) -> Result<(usize, Option<ExpansionResume>), TreeError> {
        scratch.require_owner(r)?;
        check_range(query.relationships)?;
        if output.capacity() == 0 {
            return Err(TreeError::Invalid("zero expansion capacity"));
        }
        let kind = match query.direction {
            Direction::Out => TreeKind::OutRanges,
            Direction::In => TreeKind::InRanges,
        };
        let root = self.roots.directory(kind)?;
        let probe = RangeKey {
            node: query.node,
            rel_type: query
                .relationship_type
                .unwrap_or(RelTypeId::new(1).map_err(|_| invalid("minimum relationship type"))?),
            direction: query.direction,
            lower: if query.relationship_type.is_some() {
                query.relationships.lower
            } else {
                all_relationships()?.lower
            },
            upper: UpperBound::Infinity,
        };
        let mut start = match resume {
            Some(ExpansionResume::Inclusive { descriptor, .. })
            | Some(ExpansionResume::After { descriptor }) => range::directory_key(descriptor)?,
            None => range::directory_key(probe)?,
        };
        if resume.is_none()
            && query.relationship_type.is_some()
            && let Some(entry) = lookup_predecessor(self.source, root, &start, r)?
        {
            let descriptor = decode_descriptor(root, entry, r)?;
            if descriptor.key().node == query.node
                && Some(descriptor.key().rel_type) == query.relationship_type
                && !beyond(query.relationships.lower, descriptor.key().upper)
            {
                start = descriptor.directory_key()?;
            }
        }
        let mut cursor = DirectoryCursor::seek(self.source, root, Some(&start), r)?;
        let mut resumed = resume.is_none();
        let mut count = 0_usize;
        while let Some(entry) = cursor.next_entry(r)? {
            let descriptor = decode_descriptor(root, entry, r)?;
            let key = descriptor.key();
            if key.node != query.node
                || query
                    .relationship_type
                    .is_some_and(|wanted| key.rel_type != wanted)
            {
                break;
            }
            let lower = match resume {
                Some(ExpansionResume::Inclusive {
                    descriptor,
                    relationship,
                }) if !resumed => {
                    if key != descriptor {
                        return Err(invalid("expansion resume descriptor missing"));
                    }
                    resumed = true;
                    relationship
                }
                Some(ExpansionResume::After { descriptor }) if !resumed => {
                    if key != descriptor {
                        return Err(invalid("expansion resume descriptor missing"));
                    }
                    resumed = true;
                    continue;
                }
                _ => query.relationships.lower,
            };
            let range = validate_range(self.source, root, entry, self.cutoff, scratch, r)?;
            let first = range.edges().partition_point(|edge| edge.rel < lower);
            for (index, edge) in range.edges().iter().enumerate().skip(first) {
                r.step(std::mem::size_of::<Edge>() as u64)?;
                r.read_event(NativeReadEvent::AdjacencyEntry)?;
                r.read_event(NativeReadEvent::MergedAdjacency)?;
                if beyond(edge.rel, query.relationships.upper) {
                    break;
                }
                let authoritative = self
                    .raw_relationship(edge.rel, r)?
                    .ok_or(TreeError::Missing)?;
                let (bound, neighbor) = match query.direction {
                    Direction::Out => (authoritative.source, authoritative.target),
                    Direction::In => (authoritative.target, authoritative.source),
                };
                if bound != query.node
                    || neighbor != edge.neighbor
                    || authoritative.relationship_type != key.rel_type
                {
                    return Err(invalid("adjacency differs from authoritative relationship"));
                }
                // The owner-checked cursor retains this proof only for its
                // exact bound node and immutable admission, including resumes.
                // Keep it lazy: an implicit node with no edges needs no graph
                // state record. Missing required endpoints still fail loudly.
                let bound_is_live = match *bound_live {
                    Some(live) => live,
                    None => {
                        let live = self.endpoint_live(bound, r)?;
                        *bound_live = Some(live);
                        live
                    }
                };
                // Check the neighbor even for a tombstoned bound node so an
                // invisible edge cannot hide a corrupt required endpoint.
                let neighbor_is_live = self.endpoint_live(neighbor, r)?;
                if !bound_is_live || !neighbor_is_live {
                    continue;
                }
                r.step(std::mem::size_of::<RelationshipRow>() as u64)?;
                r.read_event(NativeReadEvent::CopiedBytes(
                    std::mem::size_of::<RelationshipRow>() as u64,
                ))?;
                output.push(authoritative).map_err(|_| TreeError::Memory)?;
                count += 1;
                if count == output.capacity() {
                    let next = range.edges().get(index + 1).map_or(
                        ExpansionResume::After { descriptor: key },
                        |edge| ExpansionResume::Inclusive {
                            descriptor: key,
                            relationship: edge.rel,
                        },
                    );
                    r.step(0)?;
                    return Ok((count, Some(next)));
                }
            }
        }
        if !resumed {
            return Err(invalid("expansion resume descriptor missing"));
        }
        r.step(0)?;
        Ok((count, None))
    }
    /// Count one physical direction after checking both endpoints. A self-loop
    /// counts once in each direction; undirected execution owns its deduplication.
    pub fn degree(
        &self,
        node: NodeId,
        direction: Direction,
        relationship_type: Option<RelTypeId>,
        scratch: &mut RangeScratch<'_>,
        r: &mut TreeResources<'_>,
    ) -> Result<u64, TreeError> {
        scratch.require_owner(r)?;
        let query = AdjacencyQuery {
            node,
            direction,
            relationship_type,
            relationships: all_relationships()?,
        };
        let mut count = 0u64;
        self.visit_adjacency(query, scratch, r, &mut |_, r| {
            r.step(1)?;
            count = count.checked_add(1).ok_or(TreeError::Work)?;
            Ok(true)
        })?;
        r.step(0)?;
        Ok(count)
    }
    /// Bounded plain-DELETE probe against this admitted pre-batch view. Only
    /// explicitly removed sorted unique relationship IDs are excluded; pending
    /// far-node deletions never redefine this view's endpoint liveness.
    pub fn has_live_incident(
        &self,
        node: NodeId,
        removed: &[RelId],
        scratch: &mut RangeScratch<'_>,
        r: &mut TreeResources<'_>,
    ) -> Result<bool, TreeError> {
        scratch.require_owner(r)?;
        if removed.len() > MAX_GRAPH_CHANGES {
            return Err(TreeError::Memory);
        }
        let mut previous = None;
        for id in removed {
            r.step(1)?;
            if previous.is_some_and(|prior| prior >= *id) {
                return Err(invalid("incident removal order"));
            }
            previous = Some(*id);
        }
        let mut found = false;
        for direction in [Direction::Out, Direction::In] {
            self.visit_adjacency(
                AdjacencyQuery {
                    node,
                    direction,
                    relationship_type: None,
                    relationships: all_relationships()?,
                },
                scratch,
                r,
                &mut |row, r| {
                    if !contains(removed, row.edge.rel, r)? {
                        found = true;
                    }
                    Ok(!found)
                },
            )?;
            if found {
                break;
            }
        }
        r.step(0)?;
        Ok(found)
    }
    pub(super) fn raw_relationship(
        &self,
        rel: RelId,
        r: &mut TreeResources<'_>,
    ) -> Result<Option<RelationshipRow>, TreeError> {
        let root = self.roots.directory(TreeKind::Relationships)?;
        lookup_entry(self.source, root, &rel.get().to_le_bytes(), r)?
            .map(|entry| self.decode_relationship(entry, rel, r))
            .transpose()
    }
    fn decode_relationship(
        &self,
        entry: DirectoryEntry<'_>,
        rel: RelId,
        r: &mut TreeResources<'_>,
    ) -> Result<RelationshipRow, TreeError> {
        let payload = PayloadRef::decode(entry.value())?;
        let record = verify_record(
            PayloadSlice::new(
                self.source,
                self.roots.store(),
                entry.creation_generation(),
                payload,
            ),
            EntityId::Relationship(rel),
            self.catalog,
            self.document,
            r,
        )?;
        let RecordShape::Relationship {
            id,
            source,
            target,
            relationship_type,
        } = record.shape()
        else {
            return Err(invalid("relationship directory role"));
        };
        Ok(RelationshipRow {
            rel: id,
            source,
            target,
            relationship_type,
        })
    }
    pub(crate) fn endpoint_live(
        &self,
        node: NodeId,
        r: &mut TreeResources<'_>,
    ) -> Result<bool, TreeError> {
        let state = lookup_node_state(
            self.source,
            self.roots,
            node,
            self.catalog,
            self.document,
            r,
        )?
        .ok_or(TreeError::Missing)?;
        Ok(matches!(state, NodeRecordState::Live(_)))
    }
    pub(super) fn visible(
        &self,
        row: RelationshipRow,
        r: &mut TreeResources<'_>,
    ) -> Result<bool, TreeError> {
        let source = self.endpoint_live(row.source, r)?;
        let target = self.endpoint_live(row.target, r)?;
        Ok(source && target)
    }
    fn visit_relationships(
        &self,
        range: RelationshipRange,
        r: &mut TreeResources<'_>,
        visit: &mut impl FnMut(RelationshipRow, &mut TreeResources<'_>) -> Result<bool, TreeError>,
    ) -> Result<(), TreeError> {
        check_range(range)?;
        let mut cursor = DirectoryCursor::seek(
            self.source,
            self.roots.directory(TreeKind::Relationships)?,
            Some(&range.lower.get().to_le_bytes()),
            r,
        )?;
        while let Some(entry) = cursor.next_entry(r)? {
            r.step(16)?;
            let Key::Inline(key) = entry.key() else {
                return Err(invalid("overflow relationship identity"));
            };
            let rel = RelId::new(u128::from_le_bytes(
                key.try_into()
                    .map_err(|_| invalid("relationship identity width"))?,
            ))
            .map_err(|_| invalid("zero relationship identity"))?;
            if beyond(rel, range.upper) {
                break;
            }
            let row = self.decode_relationship(entry, rel, r)?;
            if self.visible(row, r)? && !visit(row, r)? {
                break;
            }
        }
        r.step(0)
    }
    pub(crate) fn visit_adjacency(
        &self,
        query: AdjacencyQuery,
        scratch: &mut RangeScratch<'_>,
        r: &mut TreeResources<'_>,
        visit: &mut impl FnMut(AdjacencyRow, &mut TreeResources<'_>) -> Result<bool, TreeError>,
    ) -> Result<(), TreeError> {
        check_range(query.relationships)?;
        let kind = match query.direction {
            Direction::Out => TreeKind::OutRanges,
            Direction::In => TreeKind::InRanges,
        };
        let root = self.roots.directory(kind)?;
        let probe = RangeKey {
            node: query.node,
            rel_type: query
                .relationship_type
                .unwrap_or(RelTypeId::new(1).map_err(|_| invalid("minimum relationship type"))?),
            direction: query.direction,
            lower: if query.relationship_type.is_some() {
                query.relationships.lower
            } else {
                all_relationships()?.lower
            },
            upper: UpperBound::Infinity,
        };
        let mut start = range::directory_key(probe)?;
        if query.relationship_type.is_some()
            && let Some(entry) = lookup_predecessor(self.source, root, &start, r)?
        {
            let descriptor = decode_descriptor(root, entry, r)?;
            if descriptor.key().node == query.node
                && Some(descriptor.key().rel_type) == query.relationship_type
                && !beyond(query.relationships.lower, descriptor.key().upper)
            {
                start = descriptor.directory_key()?;
            }
        }
        let mut cursor = DirectoryCursor::seek(self.source, root, Some(&start), r)?;
        while let Some(entry) = cursor.next_entry(r)? {
            let descriptor = decode_descriptor(root, entry, r)?;
            let key = descriptor.key();
            if key.node != query.node
                || query
                    .relationship_type
                    .is_some_and(|wanted| key.rel_type != wanted)
            {
                break;
            }
            let range = validate_range(self.source, root, entry, self.cutoff, scratch, r)?;
            for edge in range.edges() {
                r.step(std::mem::size_of::<Edge>() as u64)?;
                r.read_event(NativeReadEvent::AdjacencyEntry)?;
                r.read_event(NativeReadEvent::MergedAdjacency)?;
                if edge.rel < query.relationships.lower {
                    continue;
                }
                if beyond(edge.rel, query.relationships.upper) {
                    break;
                }
                let authoritative = self
                    .raw_relationship(edge.rel, r)?
                    .ok_or(TreeError::Missing)?;
                let (bound, neighbor) = match query.direction {
                    Direction::Out => (authoritative.source, authoritative.target),
                    Direction::In => (authoritative.target, authoritative.source),
                };
                if bound != query.node
                    || neighbor != edge.neighbor
                    || authoritative.relationship_type != key.rel_type
                {
                    return Err(invalid("adjacency differs from authoritative relationship"));
                }
                if self.visible(authoritative, r)?
                    && !visit(
                        AdjacencyRow {
                            relationship_type: key.rel_type,
                            edge: *edge,
                        },
                        r,
                    )?
                {
                    return r.step(0);
                }
            }
        }
        r.step(0)
    }
}
fn decode_descriptor(
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
fn all_relationships() -> Result<RelationshipRange, TreeError> {
    Ok(RelationshipRange {
        lower: RelId::new(1).map_err(|_| invalid("minimum relationship identity"))?,
        upper: UpperBound::Infinity,
    })
}
fn check_range(range: RelationshipRange) -> Result<(), TreeError> {
    if matches!(range.upper, UpperBound::Exclusive(upper) if upper < range.lower) {
        Err(invalid("relationship read interval"))
    } else {
        Ok(())
    }
}
fn beyond(rel: RelId, upper: UpperBound) -> bool {
    matches!(upper, UpperBound::Exclusive(upper) if rel >= upper)
}
fn contains(values: &[RelId], value: RelId, r: &mut TreeResources<'_>) -> Result<bool, TreeError> {
    let (mut lower, mut upper) = (0, values.len());
    while lower < upper {
        r.step(1)?;
        let middle = lower + (upper - lower) / 2;
        match values.get(middle).ok_or(TreeError::Memory)?.cmp(&value) {
            std::cmp::Ordering::Less => lower = middle + 1,
            std::cmp::Ordering::Greater => upper = middle,
            std::cmp::Ordering::Equal => return Ok(true),
        }
    }
    Ok(false)
}
fn invalid(message: &'static str) -> TreeError {
    TreeError::Invalid(message)
}
