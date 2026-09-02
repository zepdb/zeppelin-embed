#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
};
use zeppelin_embed::fts::index::DEFAULT_FIELD;
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::fts::tokenizer::{Analyzer, TokenizerConfig};
use zeppelin_embed::graph::GraphParams;
use zeppelin_embed::graph::block::{
    GraphNodeBlockBuild, GraphNodeBlockInput, GraphNodeLayout, decode_node_blocks,
    encode_node_blocks, encode_node_blocks_with_refinement_passes,
};
use zeppelin_embed::graph::build::GraphBuildError;
use zeppelin_embed::graph::refine::{
    CheckpointedRefinement, RefinementError, RefinementPass, refine_graph,
    refine_graph_checkpointed, validate_refinement_checkpoint,
};
use zeppelin_embed::graph::search::{GraphSearchRequest, GraphSearchScratch, GraphSearcher};
use zeppelin_embed::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
};
use zeppelin_embed::meta::{AliveSet, ColumnStoreBuilder, Predicate, Schema, TIMESTAMP_COLUMN};
use zeppelin_embed::quant::{Bit4Factors, quantize_bit4};
use zeppelin_embed::scan::ScanOptions;
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::segment::reader::SegmentReader;
use zeppelin_embed::segment::writer::{SegmentBuild, SegmentFactors, write_segment_with_graph};
use zeppelin_embed::tier::{MaintenanceBudget, TierThresholds};
use zeppelin_embed::vfs::StdVfs;

fn one_node_graph() -> (GraphNodeLayout, [u8; 64], GraphNodeBlockInput<'static>) {
    let layout = GraphNodeLayout::new(128, 128, 0).expect("one-node layout");
    let codes = [0_u8; 64];
    let input = GraphNodeBlockInput {
        codes: &[0_u8; 64],
        factors: Bit4Factors::from_persisted(1.0, 0.0, 0.0),
        flags: 1,
        neighbors: &[],
    };
    (layout, codes, input)
}

const DIMS: usize = 128;

fn sift_epoch() -> StoreEpoch {
    let document = EmbeddingTower {
        model_id: "refinement-sift-fixture".to_owned(),
        model_version: "1".to_owned(),
        weights_digest: vec![0x19],
        dims: DIMS as u32,
        normalization: Normalization::None,
        prompt_prefix: String::new(),
        max_tokens: 512,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    StoreEpoch {
        embedding: EmbeddingEpoch {
            query: document.clone(),
            document,
            alignment_digest: Vec::new(),
        },
        tokenizer: TokenizerConfig::text_default().epoch(),
    }
}

fn angular_epoch() -> StoreEpoch {
    let mut epoch = sift_epoch();
    epoch.embedding.document.normalization = Normalization::L2;
    epoch.embedding.query.normalization = Normalization::L2;
    epoch
}

fn refinement_fixture(directory: &std::path::Path) -> SegmentReader {
    let rows = 8_usize;
    let vectors = (0..rows)
        .flat_map(|row| std::iter::repeat_n(row as f32, DIMS))
        .collect::<Vec<_>>();
    let mut codes = vec![0_u8; rows * DIMS.div_ceil(2)];
    let mut factors = Vec::with_capacity(rows);
    for (vector, encoded) in vectors
        .chunks_exact(DIMS)
        .zip(codes.chunks_exact_mut(DIMS.div_ceil(2)))
    {
        factors.push(quantize_bit4(vector, encoded).expect("fixture row quantizes"));
    }
    let mut columns = ColumnStoreBuilder::new(Schema::new(Vec::new()).expect("timestamp schema"));
    for row in 0..rows {
        columns
            .push_row(row as i64, &[])
            .expect("fixture timestamp");
    }
    let columns = columns.finish().expect("fixture columns");
    let alive = AliveSet::new(rows as u32);
    let adjacency = [
        vec![4, 1, 2],
        vec![0, 3, 4],
        vec![0, 5],
        vec![1, 6],
        vec![0, 1, 7],
        vec![2, 6],
        vec![3, 5, 7],
        vec![4, 6],
    ];
    let nodes = codes
        .chunks_exact(DIMS.div_ceil(2))
        .zip(&factors)
        .zip(&adjacency)
        .enumerate()
        .map(|(row, ((codes, factors), neighbors))| GraphNodeBlockInput {
            codes,
            factors: *factors,
            flags: u8::from(row.is_multiple_of(2)),
            neighbors,
        })
        .collect::<Vec<_>>();
    let id = SegmentId::new(19, [0x91; 10]);
    write_segment_with_graph(
        &StdVfs,
        directory,
        SegmentBuild {
            id,
            scheme: 4,
            dims: DIMS as u32,
            codes: &codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: &vectors,
            columns: &columns,
            alive: &alive,
        },
        GraphNodeBlockBuild {
            layout: GraphNodeLayout::new(DIMS as u32, DIMS as u32, 4)
                .expect("fixture graph layout"),
            nodes: &nodes,
        },
        DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None).expect("derived policy"),
    )
    .expect("write refinement fixture");
    SegmentReader::open(&StdVfs, &directory.join(id.file_name()), id)
        .expect("open refinement fixture")
}

fn property_fixture(directory: &std::path::Path, values: &[i16]) -> SegmentReader {
    let rows = values.len();
    let vectors = values
        .iter()
        .flat_map(|value| std::iter::repeat_n(f32::from(*value), DIMS))
        .collect::<Vec<_>>();
    let mut codes = vec![0_u8; rows * DIMS.div_ceil(2)];
    let mut factors = Vec::with_capacity(rows);
    for (vector, encoded) in vectors
        .chunks_exact(DIMS)
        .zip(codes.chunks_exact_mut(DIMS.div_ceil(2)))
    {
        factors.push(quantize_bit4(vector, encoded).expect("property row quantizes"));
    }
    let mut columns = ColumnStoreBuilder::new(Schema::new(Vec::new()).expect("property schema"));
    for row in 0..rows {
        columns
            .push_row(row as i64, &[])
            .expect("property timestamp");
    }
    let columns = columns.finish().expect("property columns");
    let alive = AliveSet::new(rows as u32);
    let adjacency = (0..rows)
        .map(|owner| {
            let component_start = owner / 4 * 4;
            let component_end = (component_start + 4).min(rows);
            (component_start..component_end)
                .filter(|neighbor| *neighbor != owner)
                .map(|neighbor| neighbor as u32)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let entries = [0, rows / 4, rows / 2, rows * 3 / 4];
    let nodes = codes
        .chunks_exact(DIMS.div_ceil(2))
        .zip(&factors)
        .zip(&adjacency)
        .enumerate()
        .map(|(row, ((codes, factors), neighbors))| GraphNodeBlockInput {
            codes,
            factors: *factors,
            flags: u8::from(entries.contains(&row)),
            neighbors,
        })
        .collect::<Vec<_>>();
    let id = SegmentId::new(29, [0x92; 10]);
    write_segment_with_graph(
        &StdVfs,
        directory,
        SegmentBuild {
            id,
            scheme: 4,
            dims: DIMS as u32,
            codes: &codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: &vectors,
            columns: &columns,
            alive: &alive,
        },
        GraphNodeBlockBuild {
            layout: GraphNodeLayout::new(DIMS as u32, DIMS as u32, 4)
                .expect("property graph layout"),
            nodes: &nodes,
        },
        DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
            .expect("property durability"),
    )
    .expect("write property fixture");
    SegmentReader::open(&StdVfs, &directory.join(id.file_name()), id)
        .expect("open property fixture")
}

#[test]
fn refinement_pass_trailer_is_additive_and_old_graphs_decode_as_unapplied() {
    let (layout, _codes, input) = one_node_graph();
    let legacy = encode_node_blocks(GraphNodeBlockBuild {
        layout,
        nodes: &[input],
    })
    .expect("legacy graph encodes");
    let decoded = decode_node_blocks(legacy.as_bytes()).expect("legacy graph decodes");
    for pass in RefinementPass::ALL {
        assert!(!decoded.refinement_passes().contains(pass));
    }

    let all = RefinementPass::ALL.into_iter().collect();
    let stamped = encode_node_blocks_with_refinement_passes(
        GraphNodeBlockBuild {
            layout,
            nodes: &[input],
        },
        all,
    )
    .expect("stamped graph encodes");
    let decoded = decode_node_blocks(stamped.as_bytes()).expect("stamped graph decodes");
    for pass in RefinementPass::ALL {
        assert!(decoded.refinement_passes().contains(pass));
    }
}

#[test]
fn every_refinement_pass_is_deterministic_bounded_and_row_preserving() {
    let directory = tempfile::tempdir().expect("refinement directory");
    let source = refinement_fixture(directory.path());
    let params = GraphParams::new(3, 4, 1.0, 1.2, 4, 2).expect("refinement params");
    let analyzer = Analyzer::new(TokenizerConfig::text_default()).expect("default analyzer");
    let policy =
        DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None).expect("derived policy");
    let source_rows = source.rescore_f32().expect("source exact rows").to_vec();
    let source_graph = source.graph_node_blocks().expect("source graph");
    let source_adjacency = (0..source_graph.node_count())
        .map(|owner| {
            let block = source_graph.block(owner).expect("source node");
            block
                .neighbors_padded()
                .take(usize::from(block.degree()))
                .collect::<BTreeSet<_>>()
        })
        .collect::<Vec<_>>();
    let baseline_recall = seeded_recall_hits(&source, &source_rows);

    for (ordinal, pass) in RefinementPass::ALL.into_iter().enumerate() {
        let first = refine_graph(&source, pass, params, 0x19_0009)
            .unwrap_or_else(|error| panic!("{} first refinement: {error}", pass.label()));
        let second = refine_graph(&source, pass, params, 0x19_0009)
            .unwrap_or_else(|error| panic!("{} second refinement: {error}", pass.label()));
        assert_eq!(
            first.encoded_region(),
            second.encoded_region(),
            "{} output differs under the same seed",
            pass.label()
        );
        let decoded =
            decode_node_blocks(first.encoded_region()).expect("refined graph region decodes");
        assert!(decoded.refinement_passes().contains(pass));
        assert_eq!(decoded.node_count(), 8);
        for owner in 0..decoded.node_count() {
            let block = decoded.block(owner).expect("refined node");
            assert!(block.degree() <= 4);
            let neighbors = block
                .neighbors_padded()
                .take(usize::from(block.degree()))
                .collect::<Vec<_>>();
            assert!(!neighbors.contains(&owner));
            assert!(neighbors.iter().all(|neighbor| *neighbor < 8));
            assert_eq!(
                neighbors.iter().copied().collect::<BTreeSet<_>>().len(),
                neighbors.len(),
                "{} node {owner} repeats a neighbor",
                pass.label()
            );
        }
        if pass == RefinementPass::NeighborReorder {
            let zero = decoded.block(0).expect("reordered node zero");
            assert_eq!(
                zero.neighbors_padded()
                    .take(usize::from(zero.degree()))
                    .collect::<Vec<_>>(),
                vec![1, 2, 4]
            );
            for owner in 0..decoded.node_count() {
                let block = decoded.block(owner).expect("reordered node");
                assert_eq!(
                    block
                        .neighbors_padded()
                        .take(usize::from(block.degree()))
                        .collect::<BTreeSet<_>>(),
                    source_adjacency[owner as usize],
                    "neighbor reorder changed node {owner} membership"
                );
            }
        }
        if pass == RefinementPass::SeedRefit {
            let entries = (0..decoded.node_count())
                .filter(|row| decoded.block(*row).expect("seed row").flags() & 1 != 0)
                .collect::<Vec<_>>();
            assert_eq!(entries, vec![0, 3, 5, 7]);
        }
        let output_id = SegmentId::new(20 + ordinal as u64, [0xa0 + ordinal as u8; 10]);
        first
            .write_segment(
                &StdVfs,
                directory.path(),
                &source,
                output_id,
                &analyzer,
                policy,
            )
            .unwrap_or_else(|error| panic!("{} segment rewrite: {error}", pass.label()));
        let output = SegmentReader::open(
            &StdVfs,
            &directory.path().join(output_id.file_name()),
            output_id,
        )
        .expect("open refined segment");
        let output_rows = output.rescore_f32().expect("refined exact rows");
        match first.old_to_new_rows() {
            Some(old_to_new) => {
                assert_eq!(old_to_new.len(), 8);
                assert_eq!(old_to_new.iter().copied().collect::<BTreeSet<_>>().len(), 8);
                for (old, new) in old_to_new.iter().copied().enumerate() {
                    assert_eq!(
                        &output_rows[new as usize * DIMS..(new as usize + 1) * DIMS],
                        &source_rows[old * DIMS..(old + 1) * DIMS]
                    );
                }
            }
            None => assert_eq!(output_rows, source_rows),
        }
        assert!(
            seeded_recall_hits(&output, &source_rows) >= baseline_recall,
            "{} lost seeded recall against exact top-k",
            pass.label()
        );
        assert!(matches!(
            refine_graph(&output, pass, params, 0x19_0009),
            Err(RefinementError::AlreadyApplied(applied)) if applied == pass
        ));
    }
}

fn reachable_rows(graph: zeppelin_embed::graph::block::GraphNodeBlocks<'_>) -> BTreeSet<u32> {
    let entry = (0..graph.node_count())
        .find(|row| graph.block(*row).expect("entry row").flags() & 1 != 0)
        .expect("graph entry");
    let mut reached = BTreeSet::from([entry]);
    let mut pending = vec![entry];
    while let Some(owner) = pending.pop() {
        let block = graph.block(owner).expect("reachable row");
        for neighbor in block.neighbors_padded().take(usize::from(block.degree())) {
            if reached.insert(neighbor) {
                pending.push(neighbor);
            }
        }
    }
    reached
}

#[test]
fn connectivity_repair_is_deterministic_connected_bounded_and_persisted() {
    let directory = tempfile::tempdir().expect("connectivity repair directory");
    let source = property_fixture(directory.path(), &[0, 1, 2, 3, 10, 11, 12, 13, 20]);
    let params = GraphParams::new(3, 4, 1.0, 1.2, 4, 2).expect("repair params");
    let pass = RefinementPass::named("connectivity-repair").expect("connectivity repair pass");

    let first = refine_graph(&source, pass, params, 0x19_000a).expect("first repair");
    let second = refine_graph(&source, pass, params, 0x19_000a).expect("second repair");
    assert_eq!(first.encoded_region(), second.encoded_region());
    let repaired = decode_node_blocks(first.encoded_region()).expect("repaired graph");
    assert!(repaired.refinement_passes().contains(pass));
    assert_eq!(reachable_rows(repaired), BTreeSet::from_iter(0..9));
    let hubs = (0..repaired.node_count())
        .filter(|row| repaired.block(*row).expect("hub row").flags() & 2 != 0)
        .collect::<Vec<_>>();
    assert!(!hubs.is_empty());
    for owner in 0..repaired.node_count() {
        let block = repaired.block(owner).expect("bounded repair row");
        assert!(block.degree() <= params.r_max());
        if block.flags() & 2 != 0 {
            assert_eq!(block.degree(), params.r_max().min(8));
        }
    }

    let output_id = SegmentId::new(30, [0xb0; 10]);
    let analyzer = Analyzer::new(TokenizerConfig::text_default()).expect("default analyzer");
    first
        .write_segment(
            &StdVfs,
            directory.path(),
            &source,
            output_id,
            &analyzer,
            DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
                .expect("repair policy"),
        )
        .expect("persist repaired graph");
    let reopened = SegmentReader::open(
        &StdVfs,
        &directory.path().join(output_id.file_name()),
        output_id,
    )
    .expect("reopen repaired graph");
    let reopened_graph = reopened.graph_node_blocks().expect("reopened graph blocks");
    assert_eq!(reachable_rows(reopened_graph), BTreeSet::from_iter(0..9));
    assert_eq!(
        (0..reopened_graph.node_count())
            .filter(|row| reopened_graph.block(*row).expect("reopened row").flags() & 2 != 0)
            .collect::<Vec<_>>(),
        hubs
    );
}

fn seeded_recall_hits(reader: &SegmentReader, queries: &[f32]) -> usize {
    let graph = reader
        .graph_node_blocks()
        .expect("recall graph node blocks");
    let rescore = reader.rescore_f32().expect("recall rescore rows");
    let mut scratch = GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
        .expect("recall scratch");
    let mut searcher = GraphSearcher::new(graph, rescore, &mut scratch).expect("recall searcher");
    let mut hits = 0_usize;
    for (query_index, query) in queries.chunks_exact(DIMS).enumerate() {
        let mut truth = rescore
            .chunks_exact(DIMS)
            .enumerate()
            .map(|(row, vector)| {
                let distance = query
                    .iter()
                    .zip(vector)
                    .map(|(left, right)| {
                        let difference = f64::from(*left) - f64::from(*right);
                        difference * difference
                    })
                    .sum::<f64>();
                (row as u32, distance)
            })
            .collect::<Vec<_>>();
        truth.sort_unstable_by(|left, right| {
            left.1
                .total_cmp(&right.1)
                .then_with(|| left.0.cmp(&right.0))
        });
        let expected = truth
            .iter()
            .take(2)
            .map(|(row, _)| *row)
            .collect::<BTreeSet<_>>();
        let result = searcher
            .search(
                GraphSearchRequest::new(query, 2, 0x19_0009 ^ query_index as u64).with_ef(6),
                None,
            )
            .expect("seeded recall query");
        hits += result
            .candidates()
            .iter()
            .filter(|candidate| expected.contains(&candidate.row_id()))
            .count();
    }
    hits
}

#[test]
fn interrupted_refinement_resumes_byte_identically_and_corruption_clears() {
    let directory = tempfile::tempdir().expect("checkpoint refinement directory");
    let store = Store::open(directory.path(), OpenOptions::default())
        .expect("open checkpoint refinement store");
    let source = refinement_fixture(directory.path());
    let params = GraphParams::new(3, 4, 1.0, 1.2, 4, 2).expect("refinement params");
    let generation = store.snapshot().expect("generation snapshot").generation();
    let control = QueryControl::Cancel(CancelToken::new());

    let connectivity =
        RefinementPass::named("connectivity-repair").expect("connectivity repair pass");
    for pass in RefinementPass::ALL.into_iter().chain([connectivity]) {
        let checkpoint = directory
            .path()
            .join(format!("{}.checkpoint", pass.label()));
        let lease = store.snapshot().expect("refinement lease");
        let interrupted = refine_graph_checkpointed(
            &store,
            &source,
            CheckpointedRefinement::new(pass, params, 0x19_0009, generation, &checkpoint, &control)
                .with_max_work_rows(2),
            &lease,
        );
        match interrupted {
            Err(RefinementError::Graph(GraphBuildError::BudgetExhausted { rows_completed: 2 })) => {
            }
            Err(error) => panic!("{} interruption returned {error:?}", pass.label()),
            Ok(_) => panic!("{} unexpectedly completed", pass.label()),
        }
        let bytes = std::fs::read(&checkpoint).expect("checkpoint persisted");
        validate_refinement_checkpoint(&bytes).expect("checkpoint validates");
        assert!(directory.path().join(source.meta().id.file_name()).exists());

        let resumed = refine_graph_checkpointed(
            &store,
            &source,
            CheckpointedRefinement::new(pass, params, 0x19_0009, generation, &checkpoint, &control),
            &lease,
        )
        .unwrap_or_else(|error| panic!("{} resume: {error}", pass.label()));
        let uninterrupted = refine_graph(&source, pass, params, 0x19_0009)
            .unwrap_or_else(|error| panic!("{} uninterrupted: {error}", pass.label()));
        assert_eq!(resumed.encoded_region(), uninterrupted.encoded_region());
        assert!(!checkpoint.exists());
        drop(lease);
    }

    let corrupt = directory.path().join("corrupt.checkpoint");
    let lease = store.snapshot().expect("corrupt checkpoint lease");
    let first = refine_graph_checkpointed(
        &store,
        &source,
        CheckpointedRefinement::new(
            RefinementPass::Renumber,
            params,
            0x19_0009,
            generation,
            &corrupt,
            &control,
        )
        .with_max_work_rows(2),
        &lease,
    );
    assert!(matches!(
        first,
        Err(RefinementError::Graph(
            GraphBuildError::BudgetExhausted { .. }
        ))
    ));
    std::fs::write(&corrupt, b"corrupt refinement checkpoint")
        .expect("poison refinement checkpoint");
    let refused = refine_graph_checkpointed(
        &store,
        &source,
        CheckpointedRefinement::new(
            RefinementPass::Renumber,
            params,
            0x19_0009,
            generation,
            &corrupt,
            &control,
        ),
        &lease,
    );
    assert!(matches!(
        refused,
        Err(RefinementError::Graph(GraphBuildError::CheckpointCorrupt(
            _
        )))
    ));
    assert!(!corrupt.exists());
    assert!(!directory.path().join("corrupt.checkpoint.tmp").exists());
    refine_graph_checkpointed(
        &store,
        &source,
        CheckpointedRefinement::new(
            RefinementPass::Renumber,
            params,
            0x19_0009,
            generation,
            &corrupt,
            &control,
        ),
        &lease,
    )
    .expect("fresh retry succeeds after corrupt checkpoint is cleared");
    drop(lease);
    drop(source);
    store.close().expect("close checkpoint refinement store");
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 12,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn refinement_properties_hold_for_seeded_graphs(
        values in prop::collection::vec(-200_i16..200, 4..24),
    ) {
        let directory = tempfile::tempdir().expect("property refinement directory");
        let source = property_fixture(directory.path(), &values);
        let params = GraphParams::new(3, 4, 1.0, 1.2, 4, 2)
            .expect("property refinement params");
        let source_rows = source
            .rescore_f32()
            .expect("property source rows")
            .chunks_exact(DIMS)
            .map(|row| row[0].to_bits())
            .collect::<Vec<_>>();
        for pass in RefinementPass::ALL {
            let artifact = refine_graph(&source, pass, params, 0x19_0009)
                .unwrap_or_else(|error| panic!("{} property refinement: {error}", pass.label()));
            let decoded = decode_node_blocks(artifact.encoded_region())
                .expect("property refined graph decodes");
            prop_assert!(decoded.refinement_passes().contains(pass));
            for owner in 0..decoded.node_count() {
                let block = decoded.block(owner).expect("property refined node");
                let neighbors = block
                    .neighbors_padded()
                    .take(usize::from(block.degree()))
                    .collect::<Vec<_>>();
                prop_assert!(block.degree() <= 4);
                prop_assert!(!neighbors.contains(&owner));
                prop_assert!(neighbors.iter().all(|neighbor| *neighbor < decoded.node_count()));
                prop_assert_eq!(
                    neighbors.iter().copied().collect::<BTreeSet<_>>().len(),
                    neighbors.len(),
                );
            }
            if let Some(old_to_new) = artifact.old_to_new_rows() {
                prop_assert_eq!(old_to_new.len(), values.len());
                prop_assert_eq!(
                    old_to_new.iter().copied().collect::<BTreeSet<_>>().len(),
                    values.len(),
                );
                let remapped = old_to_new
                    .iter()
                    .copied()
                    .enumerate()
                    .map(|(old, new)| (new, source_rows[old]))
                    .collect::<BTreeMap<_, _>>();
                prop_assert_eq!(remapped.len(), values.len());
            }
        }
        let repair = RefinementPass::named("connectivity-repair")
            .expect("connectivity repair pass");
        let repaired = refine_graph(&source, repair, params, 0x19_000a)
            .expect("property connectivity repair");
        let graph = decode_node_blocks(repaired.encoded_region())
            .expect("property repaired graph decodes");
        prop_assert_eq!(reachable_rows(graph).len(), values.len());
        for owner in 0..graph.node_count() {
            let block = graph.block(owner).expect("property repair row");
            prop_assert!(block.degree() <= params.r_max());
            if block.flags() & 2 != 0 {
                prop_assert_eq!(block.degree(), params.r_max().min(graph.node_count().saturating_sub(1) as u8));
            }
        }
    }
}

#[test]
fn maintenance_applies_every_due_refinement_after_consolidation() {
    let directory = tempfile::tempdir().expect("maintenance refinement directory");
    let epoch = sift_epoch();
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_epoch(epoch.clone()),
    )
    .expect("open maintenance refinement store");
    for batch in 0..3_u64 {
        let documents = (0..8_u64)
            .map(|row| {
                let id = batch * 8 + row;
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(u128::from(id + 1)), Revision::new(1)),
                    vec![id as f32; DIMS],
                )
                .with_timestamp(id as i64)
                .with_text(format!("refinement document {id}"))
            })
            .collect::<Vec<_>>();
        store
            .ingest(IngestBatch::new(documents).with_epoch(epoch.identity()))
            .expect("ingest refinement batch");
        store.seal().expect("seal refinement batch");
    }
    let before_rows = published_row_facts(&store);
    let before_lexical = lexical_scores(&store);
    let before_filtered = filtered_docs(&store);
    let before_stats = store
        .lexical_corpus_stats()
        .expect("pre-refinement corpus stats");

    let report = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: std::time::Duration::from_secs(30),
            bytes: u64::MAX,
        },
        TierThresholds { graph_min_rows: 1 },
    );
    assert_eq!(report.graphs_built, 3);
    assert_eq!(report.consolidations, 1);
    assert_eq!(report.passes_applied, 4);
    assert_eq!(report.pass_counters.renumber, 1);
    assert_eq!(report.pass_counters.alpha_reprune, 1);
    assert_eq!(report.pass_counters.seed_refit, 1);
    assert_eq!(report.pass_counters.neighbor_reorder, 1);
    assert!(report.refinement_generation.is_some());

    let snapshot = store.snapshot().expect("published refined snapshot");
    assert_eq!(snapshot.segments().len(), 1);
    let graph = snapshot.segments()[0]
        .graph_node_blocks()
        .expect("published refined graph");
    for pass in RefinementPass::ALL {
        assert!(graph.refinement_passes().contains(pass));
    }
    drop(snapshot);
    assert_eq!(published_row_facts(&store), before_rows);
    assert_eq!(lexical_scores(&store), before_lexical);
    assert_eq!(filtered_docs(&store), before_filtered);
    let after_stats = store
        .lexical_corpus_stats()
        .expect("post-refinement corpus stats");
    assert_eq!(after_stats.document_count(), before_stats.document_count());
    assert_eq!(after_stats.total_tokens(), before_stats.total_tokens());
    let repeated = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: std::time::Duration::from_secs(30),
            bytes: u64::MAX,
        },
        TierThresholds { graph_min_rows: 1 },
    );
    assert_eq!(repeated.passes_applied, 0);
    let dropped = store
        .drop_partition(0..24)
        .expect("drop fully-contained refined partition");
    assert_eq!(dropped.segments_dropped().len(), 1);
    assert!(
        store
            .snapshot()
            .expect("post-drop snapshot")
            .segments()
            .is_empty()
    );
    store.close().expect("close refined store");
}

#[test]
fn maintenance_publishes_connectivity_repair_only_for_angular_graphs() {
    let directory = tempfile::tempdir().expect("angular maintenance directory");
    let epoch = angular_epoch();
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_epoch(epoch.clone()),
    )
    .expect("open angular maintenance store");
    let documents = (0..8_u64)
        .map(|row| {
            let mut vector = vec![0.0_f32; DIMS];
            vector[row as usize] = 1.0;
            IngestDocument::new(
                DocumentVersion::new(DocId::new(u128::from(row + 1)), Revision::new(1)),
                vector,
            )
            .with_timestamp(row as i64)
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(documents).with_epoch(epoch.identity()))
        .expect("ingest angular maintenance rows");
    store.seal().expect("seal angular maintenance rows");

    let report = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: std::time::Duration::from_secs(30),
            bytes: u64::MAX,
        },
        TierThresholds { graph_min_rows: 1 },
    );

    assert_eq!(report.graphs_built, 1);
    assert_eq!(report.passes_applied, 5);
    assert_eq!(report.pass_counters.connectivity_repair, 1);
    let snapshot = store.snapshot().expect("published angular repair snapshot");
    let graph = snapshot.segments()[0]
        .graph_node_blocks()
        .expect("published angular repair graph");
    let pass = RefinementPass::named("connectivity-repair").expect("connectivity repair pass");
    assert!(graph.refinement_passes().contains(pass));
    assert_eq!(reachable_rows(graph), BTreeSet::from_iter(0..8));
    drop(snapshot);
    store.close().expect("close angular maintenance store");
}

fn published_row_facts(store: &Store) -> BTreeMap<u128, (u64, i64, String)> {
    let snapshot = store.snapshot().expect("row-fact snapshot");
    let mut facts = BTreeMap::new();
    for segment in snapshot.segments() {
        let columns = segment.columns().expect("row-fact columns");
        let text = segment
            .stored_text()
            .expect("row-fact stored text")
            .expect("text region");
        for row in 0..segment.meta().row_count {
            let document = segment
                .document_version(row as usize)
                .expect("row-fact document read")
                .expect("row-fact document");
            let value = text
                .row(row as usize)
                .expect("row-fact text row")
                .expect("row-fact text")
                .to_owned();
            facts.insert(
                document.doc_id().get(),
                (
                    document.revision().get(),
                    columns.timestamp(row).expect("row-fact timestamp"),
                    value,
                ),
            );
        }
    }
    facts
}

fn lexical_scores(store: &Store) -> Vec<(u128, u64)> {
    store
        .search_lexical(
            &TermQuery::flat(vec![b"refinement".to_vec()], &[DEFAULT_FIELD]),
            24,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("refinement lexical search")
        .candidates
        .into_iter()
        .map(|candidate| (candidate.document.doc_id().get(), candidate.score.to_bits()))
        .collect()
}

fn filtered_docs(store: &Store) -> Vec<u128> {
    store
        .search_filtered(
            zeppelin_embed::ingest::SearchRequest::new(&[0.0; DIMS]),
            &Predicate::Exists(TIMESTAMP_COLUMN),
            24,
            SearchOptions::new(ScanOptions { thread_budget: 1 }).with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("refinement filtered exact search")
        .candidates
        .into_iter()
        .map(|candidate| {
            candidate
                .document()
                .expect("filtered candidate document")
                .doc_id()
                .get()
        })
        .collect()
}

#[test]
fn purge_after_renumber_removes_the_same_document_identity() {
    let directory = tempfile::tempdir().expect("refinement purge directory");
    let epoch = sift_epoch();
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_epoch(epoch.clone()),
    )
    .expect("open refinement purge store");
    let documents = (0..8_u128)
        .map(|row| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new(row + 1), Revision::new(1)),
                vec![row as f32; DIMS],
            )
            .with_timestamp(row as i64)
            .with_text(format!("purge refinement document {row}"))
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(documents).with_epoch(epoch.identity()))
        .expect("ingest refinement purge rows");
    store.seal().expect("seal refinement purge rows");
    let report = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: std::time::Duration::from_secs(30),
            bytes: u64::MAX,
        },
        TierThresholds { graph_min_rows: 1 },
    );
    assert_eq!(report.passes_applied, 4);
    let purged = DocId::new(4);
    let token = store.purge(&[purged]).expect("schedule refined purge");
    store
        .await_physical_purge(token)
        .expect("complete refined purge");
    let rows = published_row_facts(&store);
    assert_eq!(rows.len(), 7);
    assert!(!rows.contains_key(&purged.get()));
    store.close().expect("close refinement purge store");
}
