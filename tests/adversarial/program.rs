use rand::Rng;
use rand::seq::SliceRandom;

use super::campaign::{CampaignKind, FeatureOperation};
use super::test_support;

pub const DIMENSIONS: usize = 4;
/// The graph promotion threshold this harness hands to
/// `maintain_with_test_thresholds`, and therefore the row count a graph
/// program must ingest before `maintain()` will build one.
///
/// This is a HARNESS KNOB, not a measurement: it only has to be large enough
/// that a graph is genuinely built and traversed, and small enough that the
/// fault matrix stays cheap. The shipped tier threshold is
/// `PROVISIONAL_TIER_THRESHOLDS.graph_min_rows` (30,000, measured in `tasks/evidence/20-crossover.md`) and is never
/// inferred from this value.
pub const GRAPH_ROWS: u32 = 96;

/// Whether this seed's program exercises the graph tier.
///
/// BL-102: this was `seed == 0`, so across the smoke sweep's 5 profiles x 12
/// seeds the graph tier saw 5 of 60 runs, each contributing a single Graph
/// search and a single Auto search. One seed in three quadruples that to 20
/// of 60 while keeping most of the matrix on the cheap scan program.
#[must_use]
pub const fn exercises_graph(seed: u64) -> bool {
    seed.is_multiple_of(3)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchKind {
    Scan,
    Auto,
    Graph,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PredicateKind {
    Eq,
    In,
    RangeTwoSided,
    RangeHalfOpen,
    Bool,
    String,
    Exists,
    IsNull,
    And,
    Or,
    Not,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CrashBoundary {
    MidWalGroup,
    PreManifestRename,
    PostManifestRename,
    MidSeal,
    MidPurge,
}

impl CrashBoundary {
    pub const ALL: [Self; 5] = [
        Self::MidWalGroup,
        Self::PreManifestRename,
        Self::PostManifestRename,
        Self::MidSeal,
        Self::MidPurge,
    ];

    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::MidWalGroup => "mid_wal_group",
            Self::PreManifestRename => "pre_manifest_rename",
            Self::PostManifestRename => "post_manifest_rename",
            Self::MidSeal => "mid_seal",
            Self::MidPurge => "mid_purge",
        }
    }

    pub fn from_key(value: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|boundary| boundary.key() == value)
            .ok_or_else(|| format!("unknown crash boundary {value:?}"))
    }
}

impl PredicateKind {
    pub const ALL: [Self; 11] = [
        Self::Eq,
        Self::In,
        Self::RangeTwoSided,
        Self::RangeHalfOpen,
        Self::Bool,
        Self::String,
        Self::Exists,
        Self::IsNull,
        Self::And,
        Self::Or,
        Self::Not,
    ];

    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::In => "in",
            Self::RangeTwoSided => "range_two_sided",
            Self::RangeHalfOpen => "range_half_open",
            Self::Bool => "bool",
            Self::String => "string",
            Self::Exists => "exists",
            Self::IsNull => "is_null",
            Self::And => "and",
            Self::Or => "or",
            Self::Not => "not",
        }
    }
}

impl SearchKind {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Scan => "scan",
            Self::Auto => "auto",
            Self::Graph => "graph",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Op {
    Open,
    Ingest {
        first_id: u32,
        count: u32,
        revision: u64,
        timestamp: i64,
    },
    EpochMismatchProbe {
        doc_id: u32,
        revision: u64,
        timestamp: i64,
    },
    PrepareEpochB,
    SwitchAliasToB,
    RollbackToA,
    DropEpochA,
    RollbackDroppedAProbe,
    Upsert {
        doc_id: u32,
        revision: u64,
        timestamp: i64,
    },
    Revise {
        doc_id: u32,
        revision: u64,
        timestamp: i64,
    },
    Delete {
        doc_id: u32,
    },
    DropPartition {
        start: i64,
        end: i64,
    },
    Purge {
        doc_id: u32,
    },
    Seal,
    Maintain {
        bytes: u64,
    },
    Search {
        query: u8,
        k: usize,
        kind: SearchKind,
    },
    FilteredSearch {
        query: u8,
        k: usize,
        maximum_timestamp: i64,
    },
    PredicateSearch {
        query: u8,
        k: usize,
        predicate: PredicateKind,
    },
    HybridSearch {
        query: u8,
        k: usize,
    },
    DeadlineProbe {
        query: u8,
    },
    FtsExtrasProbe {
        slot: u8,
    },
    Feature(FeatureOperation),
    Stats,
    Close,
    Reopen,
    Crash {
        doc_id: u32,
        revision: u64,
        timestamp: i64,
        boundary: CrashBoundary,
    },
}

impl Op {
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Ingest { .. } => "ingest",
            Self::EpochMismatchProbe { .. } => "epoch_mismatch_probe",
            Self::PrepareEpochB => "prepare_epoch_b",
            Self::SwitchAliasToB => "switch_alias_to_b",
            Self::RollbackToA => "rollback_to_a",
            Self::DropEpochA => "drop_epoch_a",
            Self::RollbackDroppedAProbe => "rollback_dropped_a_probe",
            Self::Upsert { .. } => "upsert",
            Self::Revise { .. } => "revise",
            Self::Delete { .. } => "delete",
            Self::DropPartition { .. } => "drop_partition",
            Self::Purge { .. } => "purge",
            Self::Seal => "seal",
            Self::Maintain { .. } => "maintain",
            Self::Search { .. } => "search",
            Self::FilteredSearch { .. } => "filtered_search",
            Self::PredicateSearch { .. } => "predicate_search",
            Self::HybridSearch { .. } => "hybrid_search",
            Self::DeadlineProbe { .. } => "deadline_probe",
            Self::FtsExtrasProbe { .. } => "fts_extras_probe",
            Self::Feature(_) => "feature",
            Self::Stats => "stats",
            Self::Close => "close",
            Self::Reopen => "reopen",
            Self::Crash { .. } => "crash",
        }
    }

    #[must_use]
    pub fn json_line(&self, index: usize) -> String {
        match self {
            Self::Open
            | Self::PrepareEpochB
            | Self::SwitchAliasToB
            | Self::RollbackToA
            | Self::DropEpochA
            | Self::RollbackDroppedAProbe
            | Self::Seal
            | Self::Stats
            | Self::Close
            | Self::Reopen => {
                format!("{{\"op\":{index},\"kind\":\"{}\"}}", self.kind())
            }
            Self::Ingest {
                first_id,
                count,
                revision,
                timestamp,
            } => format!(
                "{{\"op\":{index},\"kind\":\"ingest\",\"first_id\":{first_id},\"count\":{count},\"revision\":{revision},\"timestamp\":{timestamp}}}"
            ),
            Self::Upsert {
                doc_id,
                revision,
                timestamp,
            }
            | Self::Revise {
                doc_id,
                revision,
                timestamp,
            }
            | Self::EpochMismatchProbe {
                doc_id,
                revision,
                timestamp,
            } => format!(
                "{{\"op\":{index},\"kind\":\"{}\",\"doc_id\":{doc_id},\"revision\":{revision},\"timestamp\":{timestamp}}}",
                self.kind()
            ),
            Self::Crash {
                doc_id,
                revision,
                timestamp,
                boundary,
            } => format!(
                "{{\"op\":{index},\"kind\":\"crash\",\"doc_id\":{doc_id},\"revision\":{revision},\"timestamp\":{timestamp},\"boundary\":\"{}\"}}",
                boundary.key()
            ),
            Self::Delete { doc_id } | Self::Purge { doc_id } => format!(
                "{{\"op\":{index},\"kind\":\"{}\",\"doc_id\":{doc_id}}}",
                self.kind()
            ),
            Self::DropPartition { start, end } => format!(
                "{{\"op\":{index},\"kind\":\"drop_partition\",\"start\":{start},\"end\":{end}}}"
            ),
            Self::Maintain { bytes } => {
                format!("{{\"op\":{index},\"kind\":\"maintain\",\"bytes\":{bytes}}}")
            }
            Self::Search { query, k, kind } => format!(
                "{{\"op\":{index},\"kind\":\"search\",\"query\":{query},\"k\":{k},\"tier\":\"{}\"}}",
                kind.key()
            ),
            Self::FilteredSearch {
                query,
                k,
                maximum_timestamp,
            } => format!(
                "{{\"op\":{index},\"kind\":\"filtered_search\",\"query\":{query},\"k\":{k},\"maximum_timestamp\":{maximum_timestamp}}}"
            ),
            Self::PredicateSearch {
                query,
                k,
                predicate,
            } => format!(
                "{{\"op\":{index},\"kind\":\"predicate_search\",\"query\":{query},\"k\":{k},\"predicate\":\"{}\"}}",
                predicate.key()
            ),
            Self::HybridSearch { query, k } => {
                format!("{{\"op\":{index},\"kind\":\"hybrid_search\",\"query\":{query},\"k\":{k}}}")
            }
            Self::DeadlineProbe { query } => {
                format!("{{\"op\":{index},\"kind\":\"deadline_probe\",\"query\":{query}}}")
            }
            Self::FtsExtrasProbe { slot } => {
                format!("{{\"op\":{index},\"kind\":\"fts_extras_probe\",\"slot\":{slot}}}")
            }
            Self::Feature(operation) => format!(
                "{{\"op\":{index},\"kind\":\"feature\",\"campaign\":\"{}\",\"operation\":\"{}\"}}",
                operation.campaign().key(),
                operation.key()
            ),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Program {
    pub seed: u64,
    pub ops: Vec<Op>,
}

impl Program {
    #[must_use]
    pub fn generate_for(campaign: CampaignKind, seed: u64) -> Self {
        match campaign {
            CampaignKind::Overall => Self::generate(seed),
            _ => {
                let mut program = Self::generate(seed);
                program.ops.retain(|operation| {
                    !matches!(
                        operation,
                        Op::EpochMismatchProbe { .. }
                            | Op::PrepareEpochB
                            | Op::SwitchAliasToB
                            | Op::RollbackToA
                            | Op::DropEpochA
                            | Op::RollbackDroppedAProbe
                    )
                });
                let mut operations = super::campaign::feature_operations(campaign).to_vec();
                let mut rng = test_support::seeded_rng(
                    &format!("adversarial::program::{}", campaign.key()),
                    seed,
                );
                operations.shuffle(&mut rng);
                let insertion = program
                    .ops
                    .iter()
                    .position(|operation| matches!(operation, Op::Ingest { .. }))
                    .map_or(1, |index| index.saturating_add(1));
                program.ops.splice(
                    insertion..insertion,
                    operations.into_iter().map(Op::Feature),
                );
                program
            }
        }
    }

    #[must_use]
    pub fn generate(seed: u64) -> Self {
        let mut rng = test_support::seeded_rng("adversarial::program", seed);
        let mut ops = vec![Op::Open];
        let initial_count = if exercises_graph(seed) {
            GRAPH_ROWS
        } else {
            24
        };
        let initial_allowed_count = if exercises_graph(seed) {
            initial_count.saturating_sub(8)
        } else {
            initial_count
        };
        ops.push(Op::Ingest {
            first_id: 1,
            count: initial_allowed_count,
            revision: 1,
            timestamp: 10,
        });
        if exercises_graph(seed) {
            ops.push(Op::Ingest {
                first_id: initial_allowed_count.saturating_add(1),
                count: 8,
                revision: 1,
                timestamp: 12,
            });
        }
        ops.extend([
            Op::EpochMismatchProbe {
                doc_id: initial_count.saturating_add(100),
                revision: 1,
                timestamp: 10,
            },
            Op::FilteredSearch {
                query: 0,
                k: 8,
                maximum_timestamp: 9,
            },
            Op::DeadlineProbe { query: 0 },
            Op::PredicateSearch {
                query: 0,
                k: 8,
                predicate: PredicateKind::ALL[(seed as usize) % PredicateKind::ALL.len()],
            },
            Op::Search {
                query: 0,
                k: 8,
                kind: SearchKind::Scan,
            },
            Op::Seal,
            Op::Revise {
                doc_id: 2,
                revision: 2,
                timestamp: 11,
            },
            Op::Upsert {
                doc_id: 3,
                revision: 2,
                timestamp: 12,
            },
            Op::Delete { doc_id: 7 },
            Op::Search {
                query: 0,
                k: 8,
                kind: SearchKind::Scan,
            },
            Op::Search {
                query: 3,
                k: usize::MAX,
                kind: SearchKind::Scan,
            },
            Op::Maintain { bytes: u64::MAX },
            Op::FilteredSearch {
                query: 1,
                k: 8,
                maximum_timestamp: 10,
            },
            Op::Search {
                query: 1,
                k: 8,
                kind: if exercises_graph(seed) {
                    SearchKind::Graph
                } else {
                    SearchKind::Scan
                },
            },
            Op::Seal,
            Op::Maintain { bytes: u64::MAX },
            Op::HybridSearch { query: 1, k: 8 },
            Op::FtsExtrasProbe {
                slot: (seed % 2) as u8,
            },
            Op::Search {
                query: 1,
                k: 8,
                kind: if exercises_graph(seed) {
                    SearchKind::Auto
                } else {
                    SearchKind::Scan
                },
            },
            Op::Upsert {
                doc_id: initial_count + 1,
                revision: 1,
                timestamp: 20,
            },
            Op::Revise {
                doc_id: initial_count + 1,
                revision: 2,
                timestamp: 21,
            },
            Op::Delete {
                doc_id: initial_count + 1,
            },
            Op::Purge {
                doc_id: initial_count + 1,
            },
            Op::Stats,
            Op::Close,
            Op::Reopen,
            Op::Crash {
                doc_id: initial_count + 2,
                revision: 1,
                timestamp: 30,
                boundary: CrashBoundary::ALL[(seed as usize) % CrashBoundary::ALL.len()],
            },
            Op::Ingest {
                first_id: initial_count + 3,
                count: 3,
                revision: 1,
                timestamp: 31,
            },
            Op::Search {
                query: if exercises_graph(seed) { 1 } else { 2 },
                k: 8,
                kind: if exercises_graph(seed) {
                    SearchKind::Auto
                } else {
                    SearchKind::Scan
                },
            },
            Op::DropPartition { start: 0, end: 15 },
            Op::Stats,
        ]);
        let mut next_id = initial_count + 6;
        let mut revisions = std::collections::BTreeMap::<u32, u64>::new();
        for _ in 0..12 {
            match rng.random_range(0_u8..7) {
                0 | 1 => {
                    let id = next_id;
                    next_id = next_id.saturating_add(1);
                    revisions.insert(id, 1);
                    ops.push(Op::Upsert {
                        doc_id: id,
                        revision: 1,
                        timestamp: rng.random_range(40_i64..80),
                    });
                }
                2 if !revisions.is_empty() => {
                    let slot = rng.random_range(0..revisions.len());
                    let (&id, revision) = revisions.iter_mut().nth(slot).expect("bounded slot");
                    *revision = revision.saturating_add(1);
                    ops.push(Op::Revise {
                        doc_id: id,
                        revision: *revision,
                        timestamp: rng.random_range(40_i64..80),
                    });
                }
                3 => ops.push(Op::Search {
                    query: rng.random_range(0_u8..4),
                    k: 8,
                    kind: SearchKind::Scan,
                }),
                4 => ops.push(Op::Stats),
                5 => ops.push(Op::FilteredSearch {
                    query: rng.random_range(0_u8..4),
                    k: 8,
                    maximum_timestamp: rng.random_range(9_i64..80),
                }),
                _ => ops.push(Op::Maintain {
                    bytes: 64_u64 * 256,
                }),
            }
        }
        ops.extend([
            Op::Search {
                query: 3,
                k: usize::MAX,
                kind: SearchKind::Scan,
            },
            Op::FilteredSearch {
                query: 2,
                k: 8,
                maximum_timestamp: 60,
            },
            Op::Seal,
            Op::PrepareEpochB,
            Op::SwitchAliasToB,
            Op::Search {
                query: 3,
                k: usize::MAX,
                kind: SearchKind::Scan,
            },
            Op::RollbackToA,
            Op::Search {
                query: 3,
                k: usize::MAX,
                kind: SearchKind::Scan,
            },
            Op::SwitchAliasToB,
            Op::DropEpochA,
            Op::RollbackDroppedAProbe,
            Op::Search {
                query: 3,
                k: usize::MAX,
                kind: SearchKind::Scan,
            },
            Op::Close,
        ]);
        Self { seed, ops }
    }

    #[must_use]
    pub fn jsonl(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (index, op) in self.ops.iter().enumerate() {
            bytes.extend_from_slice(op.json_line(index).as_bytes());
            bytes.push(b'\n');
        }
        bytes
    }
}

#[must_use]
pub fn vector(doc_id: u32, revision: u64) -> [f32; DIMENSIONS] {
    let id = doc_id as f32;
    let rev = revision as f32;
    [
        id * 0.03125 + rev * 0.000_125,
        (id % 97.0) * 0.0625,
        (id % 29.0) * 0.125 + rev * 0.000_25,
        (id % 7.0) * 0.25,
    ]
}

#[must_use]
pub const fn query(slot: u8) -> [f32; DIMENSIONS] {
    match slot % 4 {
        0 => [0.25, 1.0, 0.5, 0.0],
        1 => [32.0, 2.0, 1.0, 0.5],
        2 => [128.0, 4.0, 2.0, 1.0],
        _ => [256.0, 6.0, 3.0, 1.5],
    }
}

#[must_use]
pub const fn lexical_query(slot: u8) -> &'static [u8] {
    match slot % 4 {
        0 => b"alpha",
        1 => b"bravo",
        2 => b"charli",
        _ => b"delta",
    }
}

#[must_use]
pub fn lexical_text(doc_id: u32, revision: u64) -> String {
    let term = match doc_id % 4 {
        0 => "alpha",
        1 => "bravo",
        2 => "charli",
        _ => "delta",
    };
    let repeats = doc_id % 3 + 1;
    let mut text = (0..repeats).map(|_| term).collect::<Vec<_>>().join(" ");
    text.push_str(&format!(" common ze-{doc_id:08x}-r{revision}"));
    text
}

#[must_use]
pub const fn numeric_column(doc_id: u32) -> u64 {
    (doc_id % 5) as u64
}

#[must_use]
pub const fn boolean_column(doc_id: u32) -> Option<bool> {
    if doc_id.is_multiple_of(3) {
        None
    } else {
        Some(doc_id.is_multiple_of(2))
    }
}

#[must_use]
pub const fn string_column(doc_id: u32) -> &'static str {
    if doc_id.is_multiple_of(2) {
        "even"
    } else {
        "odd"
    }
}

#[must_use]
pub fn sentinel(doc_id: u32) -> Vec<u8> {
    format!("ZE-PURGE-SENTINEL-{doc_id:08x}").into_bytes()
}
