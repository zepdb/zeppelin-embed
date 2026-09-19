//! Owned native records, with no FFI import or source-view lifetime.
use super::{Span, ValueIndex};
use crate::lifecycle::SearchTier;
use crate::property_graph::query::plan::SearchCallId;
use crate::property_graph::query::runtime::WorkCounters;
use crate::property_graph::{EntityKind, GraphGeneration, GraphRevision, NodeId, RelId};

/// Complete authenticated per-item write outcome. Deletion cannot be inferred
/// from the presence or absence of a copied entity in the result pools.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Receipt {
    /// Original zero-based request item, retained in complete original order.
    pub item_index: u32,
    /// Whether the coordinator's acknowledged operation deletes this entity.
    pub deleted: bool,
    /// Authentic full identity, revision, changed generation and replay status.
    pub receipt: crate::property_graph::staging::ItemReceipt,
}

/// Query-list versus exact stored scalar-list interpretation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ListKind {
    /// Heterogeneous query list, including nested lists and null.
    Query,
    /// Canonical untyped stored empty list; children must be empty.
    Empty,
    /// Boolean scalar or homogeneous boolean-list element tag.
    Bool,
    /// Exact signed 64-bit integer or homogeneous integer-list element tag.
    I64,
    /// Exact IEEE binary64 bits or homogeneous floating-list element tag.
    F64,
    /// Exact UTF-8 range or homogeneous string-list element tag.
    String,
}
/// Exact application-key bytes in the owned UTF-8 pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Key {
    /// Explicit entity domain; keys cannot cross node/relationship kinds.
    pub kind: EntityKind,
    /// Exact UTF-8 namespace range, including empty or embedded NUL.
    pub namespace: Span,
    /// Exact scalar/value index or application-key byte range, as typed.
    pub value: Span,
}
/// Copied graph property: exact name and scalar/typed-list value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Property {
    /// Exact UTF-8 name range in the byte pool.
    pub name: Span,
    /// Exact scalar/value index or application-key byte range, as typed.
    pub value: ValueIndex,
}
/// Copied node. Default entity projection omits text/vector; explicit selection
/// preserves absent versus present empty text and original vector bits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Node {
    /// Full nonzero native identity; no narrowing or allocation occurs here.
    pub id: NodeId,
    /// Exact positive installed ingestion revision.
    pub revision: GraphRevision,
    /// Exact logical generation associated with this record or admitted view.
    pub generation: GraphGeneration,
    /// Optional exact application key; absence differs from an empty key.
    pub key: Option<Key>,
    /// Range in names, strictly ordered by exact UTF-8 bytes.
    pub labels: Span,
    /// Named scalar or typed homogeneous scalar-list properties.
    pub properties: Span,
    /// Explicitly selected stored text; None differs from present empty text.
    pub text: Option<Span>,
    /// Explicitly selected original f32 bits in vectors; default projection omits it.
    pub vector: Option<Span>,
}
/// Copied relationship. Endpoints retain original direction, even after IN reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Relationship {
    /// Full nonzero native identity; no narrowing or allocation occurs here.
    pub id: RelId,
    /// Exact positive installed ingestion revision.
    pub revision: GraphRevision,
    /// Exact logical generation associated with this record or admitted view.
    pub generation: GraphGeneration,
    /// Optional exact application key; absence differs from an empty key.
    pub key: Option<Key>,
    /// Original full-width source node ID, regardless of traversal direction.
    pub source: NodeId,
    /// Original full-width target node ID, regardless of traversal direction.
    pub target: NodeId,
    /// Exact relationship type name range in the byte pool.
    pub relationship_type: Span,
    /// Named scalar or typed homogeneous scalar-list properties.
    pub properties: Span,
}
/// Eager invocation mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchKind {
    /// Vector-only invocation using squared-L2 ascending scores.
    Vector,
    /// Lexical-only invocation using descending BM25 scores.
    Lexical,
    /// Hybrid invocation with complete present-component cross-scoring.
    Hybrid,
}
/// Actual vector route; Auto is a request preference, never an actual route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActualTier {
    /// Exact actual route or independently established complete candidate coverage.
    Exact,
    /// Actual vector scan route.
    Scan,
    /// Actual approximate graph route.
    Graph,
}
/// Score precision, deliberately separate from candidate coverage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScorePrecision {
    /// No vector scoring precision applies.
    NotApplicable,
    /// Scores computed in the original f32 vector domain.
    Original,
    /// Quantized vector scoring without complete original rescoring.
    Quantized,
    /// Multiple actual vector scoring precisions retained.
    Mixed,
}
/// Whether the source proved complete eligible top-k coverage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateCoverage {
    /// Exact actual route or independently established complete candidate coverage.
    Exact,
    /// Candidate source lacks a complete eligible top-k certificate.
    Approximate,
}
/// Modality state established by the ranking producer, not inferred from count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegState {
    /// The modality was not requested.
    NotRequested,
    /// The requested eligible modality is nonempty.
    Nonempty,
    /// There is no live indexed population for the modality.
    NoIndexedPopulation,
    /// The eligible intersection has no indexed members.
    NoEligibleMembers,
    /// Eligible indexed text exists but the lexical query has no matches.
    NoQueryMatches,
}
/// Lossless per-invocation provenance, retained even when no score is projected.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SearchReport {
    /// Unique source-order invocation identifier; complete reports begin at zero.
    pub call: SearchCallId,
    /// Exact logical generation associated with this record or admitted view.
    pub generation: GraphGeneration,
    /// Actual invocation mode; requested legs and actual route must agree.
    pub kind: SearchKind,
    /// Original optional request preference; absence differs from explicit Auto.
    pub requested_tier: Option<SearchTier>,
    /// Actual vector route, independent of requested preference.
    pub actual_tier: Option<ActualTier>,
    /// Actual scoring precision, independent of coverage.
    pub precision: ScorePrecision,
    /// Candidate coverage evidence retained through later graph projection.
    pub coverage: CandidateCoverage,
    /// Actual vector membership/empty-leg state.
    pub vector_leg: LegState,
    /// Actual lexical membership/empty-leg state.
    pub lexical_leg: LegState,
    /// Optional document embedding interpretation epoch.
    pub document_epoch: Option<u64>,
    /// Optional query embedding interpretation epoch.
    pub query_epoch: Option<u64>,
    /// Optional lexical analyzer interpretation epoch.
    pub tokenizer_epoch: Option<u64>,
    /// Exact finite query-level vector-weight bits in [0,1].
    pub effective_alpha_bits: u64,
    /// Actual normalization-anchor policy version.
    pub normalization_version: u32,
    /// Actual weighting/rules policy version.
    pub rules_version: u32,
    /// Actual retained candidate union size.
    pub candidate_count: u64,
    /// Actual candidates with all present components evaluated.
    pub cross_scored_count: u64,
    /// Actual fallback occurrences; zero is not proof of exactness.
    pub fallback_count: u64,
    /// Whether every retained candidate has complete component scoring.
    pub cross_score_complete: bool,
    /// Per-invocation actual work, distinct from the final cumulative counters.
    pub work: WorkCounters,
}
