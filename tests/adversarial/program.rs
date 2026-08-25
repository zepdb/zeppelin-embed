use rand::Rng;

use super::test_support;

pub const DIMENSIONS: usize = 4;
/// PLACEHOLDER -- NOT YET MEASURED. Test-only graph reachability extent; the
/// shipped tier threshold remains unchanged and is never inferred from this.
pub const GRAPH_ROWS: u32 = 96;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchKind {
    Scan,
    Auto,
    Graph,
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
    Stats,
    Close,
    Reopen,
    Crash {
        doc_id: u32,
        revision: u64,
        timestamp: i64,
    },
}

impl Op {
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Ingest { .. } => "ingest",
            Self::EpochMismatchProbe { .. } => "epoch_mismatch_probe",
            Self::Upsert { .. } => "upsert",
            Self::Revise { .. } => "revise",
            Self::Delete { .. } => "delete",
            Self::DropPartition { .. } => "drop_partition",
            Self::Purge { .. } => "purge",
            Self::Seal => "seal",
            Self::Maintain { .. } => "maintain",
            Self::Search { .. } => "search",
            Self::FilteredSearch { .. } => "filtered_search",
            Self::Stats => "stats",
            Self::Close => "close",
            Self::Reopen => "reopen",
            Self::Crash { .. } => "crash",
        }
    }

    #[must_use]
    pub fn json_line(&self, index: usize) -> String {
        match self {
            Self::Open | Self::Seal | Self::Stats | Self::Close | Self::Reopen => {
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
            }
            | Self::Crash {
                doc_id,
                revision,
                timestamp,
            } => format!(
                "{{\"op\":{index},\"kind\":\"{}\",\"doc_id\":{doc_id},\"revision\":{revision},\"timestamp\":{timestamp}}}",
                self.kind()
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
    pub fn generate(seed: u64) -> Self {
        let mut rng = test_support::seeded_rng("adversarial::program", seed);
        let mut ops = vec![Op::Open];
        let initial_count = if seed == 0 { GRAPH_ROWS } else { 24 };
        let initial_allowed_count = if seed == 0 {
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
        if seed == 0 {
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
                kind: if seed == 0 {
                    SearchKind::Graph
                } else {
                    SearchKind::Scan
                },
            },
            Op::Seal,
            Op::Maintain { bytes: u64::MAX },
            Op::Search {
                query: 1,
                k: 8,
                kind: if seed == 0 {
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
            },
            Op::Ingest {
                first_id: initial_count + 3,
                count: 3,
                revision: 1,
                timestamp: 31,
            },
            Op::Search {
                query: if seed == 0 { 1 } else { 2 },
                k: 8,
                kind: if seed == 0 {
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
pub fn sentinel(doc_id: u32) -> Vec<u8> {
    format!("ZE-PURGE-SENTINEL-{doc_id:08x}").into_bytes()
}
