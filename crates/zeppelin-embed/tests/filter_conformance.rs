#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::BTreeMap;

use tempfile::{TempDir, tempdir};
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SearchOptions, Store};
use zeppelin_embed::manifest::Manifest;
use zeppelin_embed::manifest::io::commit_manifest;
use zeppelin_embed::meta::{
    AliveSet, ColumnDefinition, ColumnId, ColumnInput, ColumnStore, ColumnStoreBuilder, ColumnType,
    ColumnValue, Predicate, PredicateValue, RangeBound, RangePredicate, Schema, TIMESTAMP_COLUMN,
};
use zeppelin_embed::planner::{SegmentBranch, SegmentTier};
use zeppelin_embed::quant::{Bit4Factors, quantize_bit4, quantize_int8};
use zeppelin_embed::segment::layout::Int8Factors;
use zeppelin_embed::segment::writer::{SegmentBuild, SegmentFactors, write_segment};
use zeppelin_embed::segment::{SegmentId, SegmentMeta};
use zeppelin_embed::vfs::StdVfs;

const ROWS: usize = 1_000;
const DIMS: usize = 2;
const QUERY: [f32; DIMS] = [0.25, -0.75];
const FILTER_COLUMN: ColumnId = ColumnId::new(1);
const TARGETS: [usize; 6] = [0, 1, 10, 100, 500, 1_000];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum PredicateOp {
    Eq,
    In,
    RangeTwoSided,
    RangeHalfOpen,
    Exists,
    IsNull,
    And,
    Or,
    Not,
}

impl PredicateOp {
    const ALL: [Self; 9] = [
        Self::Eq,
        Self::In,
        Self::RangeTwoSided,
        Self::RangeHalfOpen,
        Self::Exists,
        Self::IsNull,
        Self::And,
        Self::Or,
        Self::Not,
    ];

    const fn supports_timestamp_only(self) -> bool {
        !matches!(self, Self::Exists | Self::IsNull)
    }

    fn predicate(self, column: ColumnId) -> Predicate {
        let eq = |value| Predicate::Eq {
            column,
            value: PredicateValue::U64(value),
        };
        match self {
            Self::Eq => eq(1),
            Self::In => Predicate::In {
                column,
                values: vec![PredicateValue::U64(1), PredicateValue::U64(2)],
            },
            Self::RangeTwoSided => Predicate::Range(RangePredicate {
                column,
                lower: Some(RangeBound::inclusive(PredicateValue::U64(1))),
                upper: Some(RangeBound::inclusive(PredicateValue::U64(1))),
            }),
            Self::RangeHalfOpen => Predicate::Range(RangePredicate {
                column,
                lower: Some(RangeBound::inclusive(PredicateValue::U64(1))),
                upper: None,
            }),
            Self::Exists => Predicate::Exists(column),
            Self::IsNull => Predicate::IsNull(column),
            Self::And => Predicate::And(vec![eq(1), Predicate::Exists(column)]),
            Self::Or => Predicate::Or(vec![eq(1), eq(2)]),
            Self::Not => Predicate::Not(Box::new(eq(0))),
        }
    }

    fn timestamp_predicate(self) -> Predicate {
        let eq = |value| Predicate::Eq {
            column: TIMESTAMP_COLUMN,
            value: PredicateValue::I64(value),
        };
        match self {
            Self::Eq => eq(1),
            Self::In => Predicate::In {
                column: TIMESTAMP_COLUMN,
                values: vec![PredicateValue::I64(1), PredicateValue::I64(2)],
            },
            Self::RangeTwoSided => Predicate::Range(RangePredicate {
                column: TIMESTAMP_COLUMN,
                lower: Some(RangeBound::inclusive(PredicateValue::I64(1))),
                upper: Some(RangeBound::inclusive(PredicateValue::I64(1))),
            }),
            Self::RangeHalfOpen => Predicate::Range(RangePredicate {
                column: TIMESTAMP_COLUMN,
                lower: Some(RangeBound::inclusive(PredicateValue::I64(1))),
                upper: None,
            }),
            Self::And => Predicate::And(vec![eq(1), Predicate::Exists(TIMESTAMP_COLUMN)]),
            Self::Or => Predicate::Or(vec![eq(1), eq(2)]),
            Self::Not => Predicate::Not(Box::new(eq(0))),
            Self::Exists | Self::IsNull => unreachable!("registered as skipped for public ingest"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum QuantScheme {
    Bit4,
    Int8,
    F32,
}

impl QuantScheme {
    const ALL: [Self; 3] = [Self::Bit4, Self::Int8, Self::F32];
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Tier {
    ActiveScan,
    SealedScan,
    SealedGraph,
}

impl Tier {
    const ALL: [Self; 3] = [Self::ActiveScan, Self::SealedScan, Self::SealedGraph];
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum StoreState {
    WriterSeamFresh,
    WriterSeamReopened,
    PublicIngestActive,
    PublicIngestReopened,
}

impl StoreState {
    const ALL: [Self; 4] = [
        Self::WriterSeamFresh,
        Self::WriterSeamReopened,
        Self::PublicIngestActive,
        Self::PublicIngestReopened,
    ];
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum SkipReason {
    FilteredGraphOwnedBy19M6,
    TierStateNotRepresentable,
    PublicIngestHasNoQuantSelection,
    NonTimestampColumnNeedsOwnerD2,
}

fn skip_reason(
    op: PredicateOp,
    tier: Tier,
    scheme: QuantScheme,
    state: StoreState,
) -> Option<SkipReason> {
    if tier == Tier::SealedGraph {
        return Some(SkipReason::FilteredGraphOwnedBy19M6);
    }
    let represented = matches!(
        (tier, state),
        (Tier::SealedScan, StoreState::WriterSeamFresh)
            | (Tier::SealedScan, StoreState::WriterSeamReopened)
            | (Tier::ActiveScan, StoreState::PublicIngestActive)
            | (Tier::SealedScan, StoreState::PublicIngestReopened)
    );
    if !represented {
        return Some(SkipReason::TierStateNotRepresentable);
    }
    if matches!(
        state,
        StoreState::PublicIngestActive | StoreState::PublicIngestReopened
    ) && scheme != QuantScheme::Bit4
    {
        return Some(SkipReason::PublicIngestHasNoQuantSelection);
    }
    if matches!(
        state,
        StoreState::PublicIngestActive | StoreState::PublicIngestReopened
    ) && !op.supports_timestamp_only()
    {
        return Some(SkipReason::NonTimestampColumnNeedsOwnerD2);
    }
    None
}

#[test]
fn every_registered_filter_cell_is_exact_or_counted_as_skipped() {
    let mut registered = 0_usize;
    let mut expected_run = 0_usize;
    let mut skipped = BTreeMap::<SkipReason, usize>::new();
    for op in PredicateOp::ALL {
        for tier in Tier::ALL {
            for scheme in QuantScheme::ALL {
                for _target in TARGETS {
                    for state in StoreState::ALL {
                        registered += 1;
                        if let Some(reason) = skip_reason(op, tier, scheme, state) {
                            *skipped.entry(reason).or_default() += 1;
                        } else {
                            expected_run += 1;
                        }
                    }
                }
            }
        }
    }

    let mut ran = 0_usize;
    for op in PredicateOp::ALL {
        for target in TARGETS {
            let columns = writer_columns(op, target);
            let vectors = fixture_vectors();
            for scheme in QuantScheme::ALL {
                let fixture = publish_writer_fixture(&columns, &vectors, scheme);
                let store =
                    Store::open(fixture.path(), OpenOptions::default()).expect("open fresh");
                assert_writer_cell(&store, op, target);
                ran += 1;
                store.close().expect("close fresh");
                let reopened =
                    Store::open(fixture.path(), OpenOptions::default()).expect("open reopened");
                assert_writer_cell(&reopened, op, target);
                ran += 1;
                reopened.close().expect("close reopened");
            }
        }
    }

    for target in TARGETS {
        let fixture = tempdir().expect("public-ingest fixture");
        let store = Store::open(fixture.path(), OpenOptions::default()).expect("open ingest");
        ingest_public_rows(&store, target);
        for op in PredicateOp::ALL
            .into_iter()
            .filter(|op| op.supports_timestamp_only())
        {
            assert_public_cell(&store, op, target, SegmentTier::ActiveScan);
            ran += 1;
        }
        store.seal().expect("seal public-ingest rows");
        store.close().expect("close ingested store");
        let reopened = Store::open(fixture.path(), OpenOptions::default()).expect("reopen ingest");
        for op in PredicateOp::ALL
            .into_iter()
            .filter(|op| op.supports_timestamp_only())
        {
            assert_public_cell(&reopened, op, target, SegmentTier::SealedScan);
            ran += 1;
        }
        reopened.close().expect("close public reopened");
    }

    let skipped_count = skipped.values().sum::<usize>();
    println!(
        "FILTER_MATRIX registered={registered} ran={ran} skipped={skipped_count} reasons={skipped:?}"
    );
    assert_eq!(registered, 1_944);
    assert_eq!(ran, expected_run);
    assert_eq!(registered, ran + skipped_count);
}

fn writer_columns(op: PredicateOp, target: usize) -> ColumnStore {
    let schema = Schema::new(vec![ColumnDefinition::new(
        FILTER_COLUMN,
        "filter",
        ColumnType::U64,
        true,
    )])
    .expect("writer schema");
    let mut builder = ColumnStoreBuilder::new(schema);
    for row in 0..ROWS {
        let input = match op {
            PredicateOp::Exists => (row < target).then_some(1_u64),
            PredicateOp::IsNull => (row >= target).then_some(0_u64),
            PredicateOp::Eq
            | PredicateOp::In
            | PredicateOp::RangeTwoSided
            | PredicateOp::RangeHalfOpen
            | PredicateOp::And
            | PredicateOp::Or
            | PredicateOp::Not => Some(u64::from(row < target)),
        };
        let inputs = input
            .map(|value| ColumnInput {
                column: FILTER_COLUMN,
                value: ColumnValue::U64(value),
            })
            .into_iter()
            .collect::<Vec<_>>();
        builder
            .push_row(row as i64, &inputs)
            .expect("writer metadata row");
    }
    builder.finish().expect("writer columns")
}

fn fixture_vectors() -> Vec<f32> {
    (0..ROWS)
        .flat_map(|row| [row as f32 / ROWS as f32, 1.0])
        .collect()
}

fn publish_writer_fixture(columns: &ColumnStore, vectors: &[f32], scheme: QuantScheme) -> TempDir {
    let directory = tempdir().expect("writer-seam directory");
    let id = SegmentId::new(0x0001_6000_0000 + scheme as u64, [scheme as u8; 10]);
    let alive = AliveSet::new(ROWS as u32);
    let meta = match scheme {
        QuantScheme::Bit4 => {
            let mut codes = vec![0_u8; ROWS * DIMS.div_ceil(2)];
            let mut factors = Vec::<Bit4Factors>::with_capacity(ROWS);
            for (vector, encoded) in vectors
                .chunks_exact(DIMS)
                .zip(codes.chunks_exact_mut(DIMS.div_ceil(2)))
            {
                factors.push(quantize_bit4(vector, encoded).expect("Bit4 fixture row"));
            }
            write_fixture_segment(
                &directory,
                id,
                4,
                &codes,
                SegmentFactors::Bit4(&factors),
                vectors,
                columns,
                &alive,
            )
        }
        QuantScheme::Int8 => {
            let mut signed = vec![0_i8; ROWS * DIMS];
            let mut factors = Vec::<Int8Factors>::with_capacity(ROWS);
            for (vector, encoded) in vectors
                .chunks_exact(DIMS)
                .zip(signed.chunks_exact_mut(DIMS))
            {
                let (scale, offset) = quantize_int8(vector, encoded).expect("Int8 fixture row");
                factors.push(Int8Factors { scale, offset });
            }
            let codes = signed
                .into_iter()
                .map(|value| value as u8)
                .collect::<Vec<_>>();
            write_fixture_segment(
                &directory,
                id,
                2,
                &codes,
                SegmentFactors::Int8(&factors),
                vectors,
                columns,
                &alive,
            )
        }
        QuantScheme::F32 => {
            let codes = vectors
                .iter()
                .flat_map(|value| value.to_bits().to_le_bytes())
                .collect::<Vec<_>>();
            write_fixture_segment(
                &directory,
                id,
                0,
                &codes,
                SegmentFactors::F32,
                vectors,
                columns,
                &alive,
            )
        }
    };
    commit(&directory, columns, meta);
    directory
}

#[allow(clippy::too_many_arguments)]
fn write_fixture_segment(
    directory: &TempDir,
    id: SegmentId,
    scheme: u16,
    codes: &[u8],
    factors: SegmentFactors<'_>,
    vectors: &[f32],
    columns: &ColumnStore,
    alive: &AliveSet,
) -> SegmentMeta {
    write_segment(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id,
            scheme,
            dims: DIMS as u32,
            codes,
            factors,
            rescore: vectors,
            columns,
            alive,
        },
        policy(),
    )
    .expect("write matrix segment")
}

fn commit(directory: &TempDir, columns: &ColumnStore, segment: SegmentMeta) {
    commit_manifest(
        &StdVfs,
        directory.path(),
        &Manifest {
            generation: 1,
            log_seq: 0,
            segments: vec![segment],
            epochs: Vec::new(),
            schema: columns.schema().clone(),
        },
        policy(),
    )
    .expect("commit matrix manifest");
}

fn policy() -> DurabilityPolicy {
    DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None).expect("matrix policy")
}

fn assert_writer_cell(store: &Store, op: PredicateOp, target: usize) {
    let outcome = store
        .search_filtered(
            SearchRequest::new(&QUERY),
            &op.predicate(FILTER_COLUMN),
            ROWS,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("writer filtered search");
    assert_exact_rows(store, &outcome.candidates, target);
    assert_eq!(outcome.plans.len(), 1);
    assert_eq!(outcome.plans[0].tier, SegmentTier::SealedScan);
    assert_eq!(outcome.plans[0].filter_cardinality, target as u64);
    assert_eq!(
        outcome.plans[0].branch,
        if target <= 64 {
            SegmentBranch::ExactAllowList
        } else {
            SegmentBranch::MaskedScan
        }
    );
}

fn ingest_public_rows(store: &Store, target: usize) {
    let documents = (0..ROWS)
        .map(|row| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new((row + 1) as u128), Revision::new(1)),
                vec![row as f32 / ROWS as f32, 1.0],
            )
            .with_timestamp(i64::from(row < target))
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(documents))
        .expect("public ingest rows");
}

fn assert_public_cell(store: &Store, op: PredicateOp, target: usize, tier: SegmentTier) {
    let outcome = store
        .search_filtered(
            SearchRequest::new(&QUERY),
            &op.timestamp_predicate(),
            ROWS,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("public-ingest filtered search");
    assert_exact_rows(store, &outcome.candidates, target);
    assert_eq!(outcome.plans.len(), 1);
    assert_eq!(outcome.plans[0].tier, tier);
    assert_eq!(outcome.plans[0].filter_cardinality, target as u64);
}

fn assert_exact_rows(
    store: &Store,
    candidates: &[zeppelin_embed::ingest::SearchCandidate],
    target: usize,
) {
    let expected = store
        .search(
            SearchRequest::new(&QUERY),
            ROWS,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("brute-force unfiltered scan")
        .candidates
        .into_iter()
        .filter(|candidate| candidate.row_id().local_row() < target as u32)
        .collect::<Vec<_>>();
    assert_eq!(candidates.len(), target);
    assert_eq!(candidates.len(), expected.len());
    for (actual, expected) in candidates.iter().zip(expected) {
        assert_eq!(actual.row_id(), expected.row_id());
        assert_eq!(actual.document(), expected.document());
        assert_eq!(actual.score().to_bits(), expected.score().to_bits());
    }
}
