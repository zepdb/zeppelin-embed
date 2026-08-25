//! Typed filter planning, exact execution branches, and truthful plan reports.

mod choose;
mod exec;
mod lexical;
mod prune;

pub use choose::{ALLOW_LIST_ROWS_THRESHOLD, choose_scan_branch};
pub use exec::{FilteredSearchError, FilteredSearchOutcome};
pub use lexical::{
    LEXICAL_ALLOW_LIST_DIVISOR, LexicalBranch, LexicalFilterError, LexicalSearchOutcome,
    search_lexical_filtered,
};
pub use prune::{PlanError, segment_may_match, validate_predicate};

use crate::ingest::RowSource;
use crate::meta::Predicate;

/// One exact vector-filter execution branch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentBranch {
    /// The clustering range proved that the segment cannot match.
    Pruned,
    /// Iterate the exact allow-list and score only its rows.
    ExactAllowList,
    /// Sweep the segment and mask every row through the exact allow-list.
    MaskedScan,
    /// Graph traversal navigates every row and retains only effective-mask rows.
    FilteredGraph,
    /// Filtered traversal abandoned to an exact allow-list answer.
    GraphExactFallback,
    /// Unfiltered graph traversal.
    Graph,
}

/// The physical tier represented by one segment plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentTier {
    /// The in-RAM active segment.
    ActiveScan,
    /// An immutable segment without graph traversal selected.
    SealedScan,
    /// An immutable segment with a published graph artifact.
    SealedGraph,
}

/// Where the exact filter is enforced relative to candidate scoring.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilterMode {
    /// No filter was requested.
    None,
    /// The bitmap restricts rows before scoring.
    Pre,
    /// Traversal enforces the filter while visiting nodes.
    InTraversal,
    /// Candidate scoring precedes an exact membership check.
    Post,
}

/// A named fallback recorded by the planner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanFallback {
    /// No fallback was required.
    None,
    /// Filtered traversal crossed its independent selectivity budget.
    VisitedBudget,
    /// A caller-supplied `ef` was raised to compensate for selectivity.
    EfWidened,
    /// Corrective ef widening exhausted before retaining the requested rows.
    CandidateShortfall,
}

/// Closed recursive plan IR. New execution families require a new enum variant.
#[derive(Clone, Debug, PartialEq)]
pub enum PlanNode {
    /// One exact vector scan.
    Scan {
        /// Store-global segment address.
        source: RowSource,
        /// Exact scan branch selected for the segment.
        branch: SegmentBranch,
    },
    /// One future lexical execution leg.
    Lexical {
        /// Whether the allow-list drives iteration.
        allow_list_driven: bool,
    },
    /// One graph leg with an optional exact fallback child.
    Graph {
        /// Store-global segment address.
        source: RowSource,
        /// Exact child used when traversal reports a fallback disposition.
        fallback: Option<Box<PlanNode>>,
    },
    /// Exact bitmap intersection wrapped around another typed node.
    BitmapIntersect {
        /// The validated closed predicate AST.
        predicate: Predicate,
        /// The execution node constrained by the bitmap.
        input: Box<PlanNode>,
    },
    /// Reserved typed fusion node for task 17.
    Fusion {
        /// Independently typed child legs.
        inputs: Vec<PlanNode>,
    },
}

/// Truthful per-segment plan report returned with filtered results.
#[derive(Clone, Debug, PartialEq)]
pub struct SegmentPlan {
    /// Collision-free segment address.
    pub source: RowSource,
    /// Physical tier observed from published state.
    pub tier: SegmentTier,
    /// Exact branch that actually executed.
    pub branch: SegmentBranch,
    /// Filter enforcement position.
    pub filter_mode: FilterMode,
    /// Exact alive-bounded allow-list cardinality.
    pub filter_cardinality: u64,
    /// Whether result quality is approximate.
    pub approximate: bool,
    /// Named fallback, if any.
    pub fallback: PlanFallback,
    /// Caller-supplied graph width, or `None` for adaptive or scan work.
    pub ef_requested: Option<usize>,
    /// Actual filter-aware graph width, or `None` when no traversal ran.
    pub ef_effective: Option<usize>,
    /// Recursive typed node for this segment.
    pub node: PlanNode,
}

impl SegmentPlan {
    pub(crate) fn unfiltered_scan(source: RowSource, tier: SegmentTier, cardinality: u64) -> Self {
        let branch = SegmentBranch::MaskedScan;
        Self {
            source,
            tier,
            branch,
            filter_mode: FilterMode::None,
            filter_cardinality: cardinality,
            approximate: false,
            fallback: PlanFallback::None,
            ef_requested: None,
            ef_effective: None,
            node: PlanNode::Scan { source, branch },
        }
    }

    pub(crate) fn unfiltered_graph(
        source: RowSource,
        cardinality: u64,
        ef_requested: Option<usize>,
        ef_effective: usize,
    ) -> Self {
        Self {
            source,
            tier: SegmentTier::SealedGraph,
            branch: SegmentBranch::Graph,
            filter_mode: FilterMode::None,
            filter_cardinality: cardinality,
            approximate: true,
            fallback: PlanFallback::None,
            ef_requested,
            ef_effective: Some(ef_effective),
            node: PlanNode::Graph {
                source,
                fallback: None,
            },
        }
    }

    pub(crate) fn unfiltered_pruned(source: RowSource, cardinality: u64) -> Self {
        Self {
            source,
            tier: SegmentTier::SealedGraph,
            branch: SegmentBranch::Pruned,
            filter_mode: FilterMode::None,
            filter_cardinality: cardinality,
            approximate: false,
            fallback: PlanFallback::None,
            ef_requested: None,
            ef_effective: None,
            node: PlanNode::Scan {
                source,
                branch: SegmentBranch::Pruned,
            },
        }
    }

    pub(crate) fn exact(
        source: RowSource,
        tier: SegmentTier,
        branch: SegmentBranch,
        cardinality: u64,
    ) -> Self {
        Self {
            source,
            tier,
            branch,
            filter_mode: FilterMode::Pre,
            filter_cardinality: cardinality,
            approximate: false,
            fallback: PlanFallback::None,
            ef_requested: None,
            ef_effective: None,
            node: PlanNode::Scan { source, branch },
        }
    }

    pub(crate) fn filtered_graph(
        source: RowSource,
        cardinality: u64,
        ef_requested: Option<usize>,
        ef_effective: usize,
        branch: SegmentBranch,
        fallback: PlanFallback,
    ) -> Self {
        let (node, filter_mode, approximate) = match branch {
            SegmentBranch::FilteredGraph => (
                PlanNode::Graph {
                    source,
                    fallback: None,
                },
                FilterMode::InTraversal,
                true,
            ),
            SegmentBranch::GraphExactFallback => (
                PlanNode::Graph {
                    source,
                    fallback: Some(Box::new(PlanNode::Scan {
                        source,
                        branch: SegmentBranch::ExactAllowList,
                    })),
                },
                FilterMode::Pre,
                false,
            ),
            _ => (PlanNode::Scan { source, branch }, FilterMode::Pre, false),
        };
        Self {
            source,
            tier: SegmentTier::SealedGraph,
            branch,
            filter_mode,
            filter_cardinality: cardinality,
            approximate,
            fallback,
            ef_requested,
            ef_effective: Some(ef_effective),
            node,
        }
    }

    pub(crate) fn with_predicate(mut self, predicate: &Predicate) -> Self {
        self.node = PlanNode::BitmapIntersect {
            predicate: predicate.clone(),
            input: Box::new(self.node),
        };
        self
    }
}
