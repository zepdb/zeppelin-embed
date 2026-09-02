#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use std::path::Path;

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};
use xxhash_rust::xxh3::xxh3_64;
use zeppelin_embed::format::frame::{FILE_HEADER_LEN, FILE_TRAILER_LEN};
use zeppelin_embed::format::golden::decode_hex;
use zeppelin_embed::graph::block::{
    CACHE_LINE_BYTES, GraphNodeBlockBuild, GraphNodeBlockInput, GraphNodeError, GraphNodeLayout,
    NODE_BLOCK_TRAILER_LEN, decode_node_blocks, encode_node_blocks,
};
use zeppelin_embed::graph::{GraphLoadOutcome, GraphQueryError, load_graph_or_exact_scan};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::meta::{AliveSet, ColumnStore, ColumnStoreBuilder, Schema};
use zeppelin_embed::quant::Bit4Factors;
use zeppelin_embed::scan::{F32Rows, ScanCandidate, ScanQuery, ScanRequest, ScanRows};
use zeppelin_embed::segment::layout::{
    CHECKSUM_CHUNK_BYTES, REGION_ALIGNMENT, REGION_ENTRY_LEN, RegionKind, SEGMENT_PREFIX_LEN,
};
use zeppelin_embed::segment::reader::SegmentReader;
use zeppelin_embed::segment::writer::{
    SegmentBuild, SegmentFactors, encode_segment_with_graph, write_segment_with_graph,
};
use zeppelin_embed::segment::{SegmentError, SegmentId};
use zeppelin_embed::vfs::StdVfs;

fn empty_columns(rows: u32) -> ColumnStore {
    let mut builder = ColumnStoreBuilder::new(Schema::new(Vec::new()).expect("empty schema"));
    for row in 0..rows {
        builder
            .push_row(i64::from(row), &[])
            .expect("timestamp-only row");
    }
    builder.finish().expect("columns")
}

fn ordered_policy() -> DurabilityPolicy {
    DurabilityPolicy::new(DurabilityMode::Durable, CommitTier::Ordered).expect("ordered durability")
}

fn golden_bytes(text: &str) -> Vec<u8> {
    decode_hex(text).expect("valid graph golden hex")
}

fn assert_bytes_equal(label: &str, actual: &[u8], expected: &[u8]) {
    if actual == expected {
        return;
    }
    let first = actual
        .iter()
        .zip(expected)
        .position(|(left, right)| left != right)
        .unwrap_or_else(|| actual.len().min(expected.len()));
    let window_start = first.saturating_sub(8);
    let window_end = first
        .saturating_add(9)
        .min(actual.len().max(expected.len()));
    let actual_window = actual
        .get(window_start..window_end.min(actual.len()))
        .expect("actual diff window");
    let expected_window = expected
        .get(window_start..window_end.min(expected.len()))
        .expect("expected diff window");
    panic!(
        "{label} byte mismatch at offset {first}: actual len {} bytes {:?}; expected len {} bytes {:?}",
        actual.len(),
        actual_window,
        expected.len(),
        expected_window
    );
}

fn repair_graph_checksum(bytes: &mut [u8]) {
    let trailer_start = bytes.len() - NODE_BLOCK_TRAILER_LEN;
    let checksum_offset = trailer_start + 32;
    let checksum = xxh3_64(&bytes[..checksum_offset]).to_le_bytes();
    bytes[checksum_offset..checksum_offset + 8].copy_from_slice(&checksum);
}

fn rewrite_graph_region(path: &Path, id: SegmentId, mutate: impl FnOnce(&mut Vec<u8>)) {
    let reader =
        SegmentReader::open(&StdVfs, path, id).expect("segment header opens before rewrite");
    let header_length = reader.header_length();
    let graph_index = reader
        .directory()
        .iter()
        .position(|entry| entry.kind == RegionKind::GraphNodeBlocks.id())
        .expect("graph directory entry");
    let graph_entry = &reader.directory()[graph_index];
    let graph_offset = usize::try_from(graph_entry.offset).expect("graph offset fits usize");
    let graph_length = usize::try_from(graph_entry.length).expect("graph length fits usize");
    let checksum_index = reader
        .directory()
        .iter()
        .position(|entry| entry.kind == RegionKind::ChecksumTable.id())
        .expect("checksum-table directory entry");
    let checksum_entry = &reader.directory()[checksum_index];
    let checksum_offset =
        usize::try_from(checksum_entry.offset).expect("checksum-table offset fits usize");
    let checksum_length =
        usize::try_from(checksum_entry.length).expect("checksum-table length fits usize");
    drop(reader);

    let mut bytes = std::fs::read(path).expect("read segment for rewrite");
    let mut graph = bytes[graph_offset..graph_offset + graph_length].to_vec();
    mutate(&mut graph);
    assert!(
        graph.len() <= checksum_offset - graph_offset,
        "rewritten graph must fit its aligned allocation"
    );

    bytes[graph_offset..checksum_offset].fill(0);
    bytes[graph_offset..graph_offset + graph.len()].copy_from_slice(&graph);

    let graph_directory_offset =
        FILE_HEADER_LEN + SEGMENT_PREFIX_LEN + graph_index * REGION_ENTRY_LEN;
    bytes[graph_directory_offset + 16..graph_directory_offset + 24]
        .copy_from_slice(&(graph.len() as u64).to_le_bytes());
    bytes[graph_directory_offset + 24..graph_directory_offset + 32]
        .copy_from_slice(&xxh3_64(&graph).to_le_bytes());

    let graph_chunk_checksums = graph
        .chunks(CHECKSUM_CHUNK_BYTES)
        .map(xxh3_64)
        .collect::<Vec<_>>();
    let checksum_table = &mut bytes[checksum_offset..checksum_offset + checksum_length];
    let entry_count = u32::from_le_bytes(checksum_table[0..4].try_into().expect("entry count"));
    assert_eq!(
        u32::from_le_bytes(checksum_table[4..8].try_into().expect("chunk size")) as usize,
        CHECKSUM_CHUNK_BYTES
    );
    let mut repaired_chunks = vec![false; graph_chunk_checksums.len()];
    for table_index in 0..entry_count as usize {
        let record = 8 + table_index * 16;
        let kind = u16::from_le_bytes(
            checksum_table[record..record + 2]
                .try_into()
                .expect("checksum kind"),
        );
        let chunk_index = u32::from_le_bytes(
            checksum_table[record + 4..record + 8]
                .try_into()
                .expect("checksum chunk index"),
        ) as usize;
        if kind == RegionKind::GraphNodeBlocks.id()
            && let Some(checksum) = graph_chunk_checksums.get(chunk_index)
        {
            checksum_table[record + 8..record + 16].copy_from_slice(&checksum.to_le_bytes());
            repaired_chunks[chunk_index] = true;
        }
    }
    assert!(repaired_chunks.iter().all(|repaired| *repaired));

    let checksum_directory_offset =
        FILE_HEADER_LEN + SEGMENT_PREFIX_LEN + checksum_index * REGION_ENTRY_LEN;
    let checksum_table_checksum =
        xxh3_64(&bytes[checksum_offset..checksum_offset + checksum_length]);
    bytes[checksum_directory_offset + 24..checksum_directory_offset + 32]
        .copy_from_slice(&checksum_table_checksum.to_le_bytes());

    let header_checksum_offset = header_length - 8;
    let header_checksum = xxh3_64(&bytes[..header_checksum_offset]);
    bytes[header_checksum_offset..header_length].copy_from_slice(&header_checksum.to_le_bytes());

    let file_checksum_offset = bytes.len() - FILE_TRAILER_LEN;
    let file_checksum = xxh3_64(&bytes[..file_checksum_offset]);
    bytes[file_checksum_offset..].copy_from_slice(&file_checksum.to_le_bytes());
    std::fs::write(path, bytes).expect("write rewritten segment");
}

fn assert_decode_error(bytes: &[u8], expected: &str) {
    let error = decode_node_blocks(bytes).expect_err("malformed graph region must fail");
    assert!(error.to_string().contains(expected), "{error}");
}

fn expect_segment_error<T>(result: Result<T, SegmentError>, context: &str) -> SegmentError {
    match result {
        Ok(_) => panic!("{context}"),
        Err(error) => error,
    }
}

#[test]
fn graph_node_block_goldens_are_byte_exact() {
    let layout_128 = GraphNodeLayout::new(128, 128, 44).expect("d128 layout");
    let codes_128_a = (0_u8..64).collect::<Vec<_>>();
    let codes_128_b = (0_u8..64).map(|value| 255 - value).collect::<Vec<_>>();
    let codes_128_c = (0_u8..64).map(|value| value ^ 0x5a).collect::<Vec<_>>();
    let codes_128_d = (0_u8..64)
        .map(|value| value.wrapping_mul(3))
        .collect::<Vec<_>>();
    let nodes_128 = [
        GraphNodeBlockInput {
            codes: &codes_128_a,
            factors: Bit4Factors::from_persisted(1.25, 2.5, -0.75),
            flags: 1,
            neighbors: &[3, 1, 2],
        },
        GraphNodeBlockInput {
            codes: &codes_128_b,
            factors: Bit4Factors::from_persisted(0.5, 4.0, 0.125),
            flags: 2,
            neighbors: &[0],
        },
        GraphNodeBlockInput {
            codes: &codes_128_c,
            factors: Bit4Factors::from_persisted(2.0, 0.75, -1.25),
            flags: 0,
            neighbors: &[3, 0],
        },
        GraphNodeBlockInput {
            codes: &codes_128_d,
            factors: Bit4Factors::from_persisted(4.5, 0.25, 1.0),
            flags: 0,
            neighbors: &[2],
        },
    ];
    let actual_128 = encode_node_blocks(GraphNodeBlockBuild {
        layout: layout_128,
        nodes: &nodes_128,
    })
    .expect("d128 golden encode")
    .into_bytes();
    assert_bytes_equal(
        "FROZEN graph d128 R44 v1",
        &actual_128,
        &golden_bytes(include_str!(
            "fixtures/format/graph_node_blocks_d128_r44_FROZEN_v1.hex"
        )),
    );
    let decoded_128 = decode_node_blocks(&actual_128).expect("decode d128 golden");
    let golden_first = decoded_128.block(0).expect("golden node 0");
    assert_eq!(
        golden_first
            .neighbors_padded()
            .take(usize::from(golden_first.degree()))
            .collect::<Vec<_>>(),
        [3, 1, 2]
    );

    let layout_100 = GraphNodeLayout::new(100, 128, 64).expect("d100 padded layout");
    let mut codes_100_a = (0_u8..50).map(|value| value ^ 0xa5).collect::<Vec<_>>();
    codes_100_a.resize(64, 0);
    let mut codes_100_b = (0_u8..50).map(|value| value ^ 0x5a).collect::<Vec<_>>();
    codes_100_b.resize(64, 0);
    let nodes_100 = [
        GraphNodeBlockInput {
            codes: &codes_100_a,
            factors: Bit4Factors::from_persisted(3.0, 1.5, -2.0),
            flags: 3,
            neighbors: &[1],
        },
        GraphNodeBlockInput {
            codes: &codes_100_b,
            factors: Bit4Factors::from_persisted(0.25, 8.0, 0.0),
            flags: 0,
            neighbors: &[0],
        },
    ];
    let actual_100 = encode_node_blocks(GraphNodeBlockBuild {
        layout: layout_100,
        nodes: &nodes_100,
    })
    .expect("d100 golden encode")
    .into_bytes();
    assert_bytes_equal(
        "FROZEN graph d100 padded128 R64 v1",
        &actual_100,
        &golden_bytes(include_str!(
            "fixtures/format/graph_node_blocks_d100_padded128_r64_FROZEN_v1.hex"
        )),
    );
}

#[test]
fn graph_node_block_round_trip_through_mapped_accessor() {
    let layout = GraphNodeLayout::new(128, 128, 44).expect("layout");
    let codes_a = (0_u8..64).collect::<Vec<_>>();
    let codes_b = (0_u8..64).map(|value| value ^ 0xff).collect::<Vec<_>>();
    let codes_c = (0_u8..64).map(|value| value ^ 0x5a).collect::<Vec<_>>();
    let codes_d = (0_u8..64)
        .map(|value| value.wrapping_mul(3))
        .collect::<Vec<_>>();
    let factors = [
        Bit4Factors::from_persisted(1.25, 2.5, -0.75),
        Bit4Factors::from_persisted(0.5, 4.0, 0.125),
        Bit4Factors::from_persisted(2.0, 0.75, -1.25),
        Bit4Factors::from_persisted(4.5, 0.25, 1.0),
    ];
    let graph_nodes = [
        GraphNodeBlockInput {
            codes: &codes_a,
            factors: factors[0],
            flags: 1,
            neighbors: &[3, 1, 2],
        },
        GraphNodeBlockInput {
            codes: &codes_b,
            factors: factors[1],
            flags: 2,
            neighbors: &[0],
        },
        GraphNodeBlockInput {
            codes: &codes_c,
            factors: factors[2],
            flags: 0,
            neighbors: &[3, 0],
        },
        GraphNodeBlockInput {
            codes: &codes_d,
            factors: factors[3],
            flags: 0,
            neighbors: &[2],
        },
    ];
    let graph = GraphNodeBlockBuild {
        layout,
        nodes: &graph_nodes,
    };
    let mut segment_codes = codes_a.clone();
    segment_codes.extend_from_slice(&codes_b);
    segment_codes.extend_from_slice(&codes_c);
    segment_codes.extend_from_slice(&codes_d);
    let rescore = vec![0.0_f32; 4 * 128];
    let columns = empty_columns(4);
    let alive = AliveSet::new(4);
    let id = SegmentId::new(19, [4; 10]);
    let directory = tempfile::tempdir().expect("tempdir");

    write_segment_with_graph(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id,
            scheme: 4,
            dims: 128,
            codes: &segment_codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: &rescore,
            columns: &columns,
            alive: &alive,
        },
        graph,
        ordered_policy(),
    )
    .expect("segment with graph");

    let reader =
        SegmentReader::open(&StdVfs, &directory.path().join(id.file_name()), id).expect("open");
    let graph_entry = reader
        .directory()
        .iter()
        .find(|entry| entry.kind == RegionKind::GraphNodeBlocks.id())
        .expect("graph directory entry");
    assert_eq!(graph_entry.offset as usize % REGION_ALIGNMENT, 0);
    let mapped = reader.graph_node_blocks().expect("mapped graph accessor");
    assert_eq!(mapped.layout(), layout);
    assert_eq!(mapped.node_count(), 4);

    let first = mapped.block(0).expect("node 0");
    assert_eq!(first.codes(), codes_a);
    assert_eq!(first.codes().as_ptr() as usize % CACHE_LINE_BYTES, 0);
    assert_eq!(first.factors(), factors[0]);
    assert_eq!(first.degree(), 3);
    assert_eq!(first.flags(), 1);
    assert_eq!(first.reserved_bytes(), [0, 0]);
    let first_neighbors = first.neighbors_padded().collect::<Vec<_>>();
    assert_eq!(first_neighbors.len(), 44);
    assert_eq!(&first_neighbors[..3], &[3, 1, 2]);
    assert!(first_neighbors[3..].iter().all(|id| *id == u32::MAX));

    let second = mapped.block(1).expect("node 1");
    assert_eq!(second.codes(), codes_b);
    assert_eq!(second.codes().as_ptr() as usize % CACHE_LINE_BYTES, 0);
    assert_eq!(second.factors(), factors[1]);
    assert_eq!(second.degree(), 1);
    assert_eq!(second.flags(), 2);
    assert_eq!(second.reserved_bytes(), [0, 0]);
    let second_neighbors = second.neighbors_padded().collect::<Vec<_>>();
    assert_eq!(second_neighbors[0], 0);
    assert!(second_neighbors[1..].iter().all(|id| *id == u32::MAX));
    assert!(matches!(
        mapped.block(4),
        Err(GraphNodeError::NodeIdOutOfRange { .. })
    ));

    let exact_rows = F32Rows::new(rescore);
    let query = [0.0_f32; 128];
    let loaded = load_graph_or_exact_scan(
        &reader,
        ScanRequest {
            query: ScanQuery::F32(&query),
            rows: ScanRows::F32RowMajor(&exact_rows),
            row_mask: None,
        },
        1,
    )
    .expect("valid graph loads");
    assert!(matches!(loaded, GraphLoadOutcome::Graph(graph) if graph.node_count() == 4));
}

#[test]
fn graph_segment_writer_rejects_mismatched_dimensions_and_node_count() {
    let factors = [
        Bit4Factors::from_persisted(1.0, 1.0, 0.0),
        Bit4Factors::from_persisted(1.0, 1.0, 0.0),
        Bit4Factors::from_persisted(1.0, 1.0, 0.0),
    ];
    let columns = empty_columns(3);
    let alive = AliveSet::new(3);
    let segment_codes = [0_u8; 3];
    let rescore = [0.0_f32; 6];
    let build = SegmentBuild {
        id: SegmentId::new(19, [5; 10]),
        scheme: 4,
        dims: 2,
        codes: &segment_codes,
        factors: SegmentFactors::Bit4(&factors),
        rescore: &rescore,
        columns: &columns,
        alive: &alive,
    };
    let graph_codes = [[0_u8; 64]; 3];
    let graph_nodes = [
        GraphNodeBlockInput {
            codes: &graph_codes[0],
            factors: factors[0],
            flags: 0,
            neighbors: &[1],
        },
        GraphNodeBlockInput {
            codes: &graph_codes[1],
            factors: factors[1],
            flags: 0,
            neighbors: &[2],
        },
        GraphNodeBlockInput {
            codes: &graph_codes[2],
            factors: factors[2],
            flags: 0,
            neighbors: &[0],
        },
    ];

    let dimension_error = expect_segment_error(
        encode_segment_with_graph(
            build,
            GraphNodeBlockBuild {
                layout: GraphNodeLayout::new(1, 128, 2).expect("mismatched graph layout"),
                nodes: &graph_nodes,
            },
        ),
        "writer must reject graph dimensions that differ from the segment",
    );
    assert!(
        matches!(&dimension_error, SegmentError::Geometry(detail) if detail == "graph dimensions 1, segment dimensions 2"),
        "{dimension_error}"
    );

    let two_graph_nodes = [
        graph_nodes[0],
        GraphNodeBlockInput {
            codes: &graph_codes[1],
            factors: factors[1],
            flags: 0,
            neighbors: &[0],
        },
    ];
    let node_count_error = expect_segment_error(
        encode_segment_with_graph(
            build,
            GraphNodeBlockBuild {
                layout: GraphNodeLayout::new(2, 128, 2).expect("matching graph layout"),
                nodes: &two_graph_nodes,
            },
        ),
        "writer must reject graph node count that differs from segment rows",
    );
    assert!(
        matches!(&node_count_error, SegmentError::Geometry(detail) if detail == "graph nodes 2, segment rows 3"),
        "{node_count_error}"
    );
}

#[test]
fn graph_segment_reader_rejects_checksum_valid_geometry_mismatches() {
    let layout = GraphNodeLayout::new(2, 128, 2).expect("layout");
    let graph_codes = [[0_u8; 64]; 3];
    let factors = [
        Bit4Factors::from_persisted(1.0, 1.0, 0.0),
        Bit4Factors::from_persisted(1.0, 1.0, 0.0),
        Bit4Factors::from_persisted(1.0, 1.0, 0.0),
    ];
    let graph_nodes = [
        GraphNodeBlockInput {
            codes: &graph_codes[0],
            factors: factors[0],
            flags: 0,
            neighbors: &[1],
        },
        GraphNodeBlockInput {
            codes: &graph_codes[1],
            factors: factors[1],
            flags: 0,
            neighbors: &[2],
        },
        GraphNodeBlockInput {
            codes: &graph_codes[2],
            factors: factors[2],
            flags: 0,
            neighbors: &[0],
        },
    ];
    let columns = empty_columns(3);
    let alive = AliveSet::new(3);
    let segment_codes = [0_u8; 3];
    let rescore = [0.0_f32; 6];
    let id = SegmentId::new(19, [6; 10]);
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join(id.file_name());
    write_segment_with_graph(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id,
            scheme: 4,
            dims: 2,
            codes: &segment_codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: &rescore,
            columns: &columns,
            alive: &alive,
        },
        GraphNodeBlockBuild {
            layout,
            nodes: &graph_nodes,
        },
        ordered_policy(),
    )
    .expect("matching segment with graph");
    let original = std::fs::read(&path).expect("original segment bytes");

    rewrite_graph_region(&path, id, |graph| {
        let trailer = graph.len() - NODE_BLOCK_TRAILER_LEN;
        graph[trailer + 12..trailer + 16].copy_from_slice(&1_u32.to_le_bytes());
        repair_graph_checksum(graph);
    });
    let dimensions_reader =
        SegmentReader::open(&StdVfs, &path, id).expect("forged dimensions header");
    dimensions_reader
        .validate_all()
        .expect("all segment framing checksums were repaired");
    let dimensions_error = expect_segment_error(
        dimensions_reader.graph_node_blocks(),
        "reader must reject graph dimensions that differ from segment dimensions",
    );
    assert!(
        matches!(&dimensions_error, SegmentError::Geometry(detail) if detail == "graph dimensions 1, segment dimensions 2"),
        "{dimensions_error}"
    );
    drop(dimensions_reader);

    std::fs::write(&path, &original).expect("restore original segment");
    let two_graph_nodes = [
        graph_nodes[0],
        GraphNodeBlockInput {
            codes: &graph_codes[1],
            factors: factors[1],
            flags: 0,
            neighbors: &[0],
        },
    ];
    let two_node_graph = encode_node_blocks(GraphNodeBlockBuild {
        layout,
        nodes: &two_graph_nodes,
    })
    .expect("standalone two-node graph")
    .into_bytes();
    rewrite_graph_region(&path, id, |graph| *graph = two_node_graph);
    let row_count_reader =
        SegmentReader::open(&StdVfs, &path, id).expect("forged node-count header");
    row_count_reader
        .validate_all()
        .expect("all segment framing checksums were repaired");
    let row_count_error = expect_segment_error(
        row_count_reader.graph_node_blocks(),
        "reader must reject graph node count that differs from segment rows",
    );
    assert!(
        matches!(&row_count_error, SegmentError::Geometry(detail) if detail == "graph nodes 2, segment rows 3"),
        "{row_count_error}"
    );
}

#[test]
fn graph_reader_validates_every_node_after_forgeable_checksums() {
    enum Damage {
        Byte(usize, u8),
        U32(usize, u32),
        NonFiniteFactor(usize),
    }

    let layout = GraphNodeLayout::new(100, 128, 2).expect("layout");
    let graph_codes = [[0_u8; 64]; 4];
    let factors = [
        Bit4Factors::from_persisted(1.0, 2.0, 3.0),
        Bit4Factors::from_persisted(1.0, 2.0, 3.0),
        Bit4Factors::from_persisted(1.0, 2.0, 3.0),
        Bit4Factors::from_persisted(1.0, 2.0, 3.0),
    ];
    let graph_nodes = [
        GraphNodeBlockInput {
            codes: &graph_codes[0],
            factors: factors[0],
            flags: 0,
            neighbors: &[0],
        },
        GraphNodeBlockInput {
            codes: &graph_codes[1],
            factors: factors[1],
            flags: 0,
            neighbors: &[0],
        },
        GraphNodeBlockInput {
            codes: &graph_codes[2],
            factors: factors[2],
            flags: 0,
            neighbors: &[0],
        },
        GraphNodeBlockInput {
            codes: &graph_codes[3],
            factors: factors[3],
            flags: 0,
            neighbors: &[0],
        },
    ];
    let columns = empty_columns(4);
    let alive = AliveSet::new(4);
    let segment_codes = [0_u8; 200];
    let rescore = [0.0_f32; 400];
    let id = SegmentId::new(19, [7; 10]);
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join(id.file_name());
    write_segment_with_graph(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id,
            scheme: 4,
            dims: 100,
            codes: &segment_codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: &rescore,
            columns: &columns,
            alive: &alive,
        },
        GraphNodeBlockBuild {
            layout,
            nodes: &graph_nodes,
        },
        ordered_policy(),
    )
    .expect("matching segment with graph");
    let original = std::fs::read(&path).expect("original segment bytes");
    let node_one = usize::try_from(layout.stride()).expect("stride fits usize");
    let cases = [
        (Damage::Byte(50, 1), "padded Bit4 dimensions"),
        (Damage::Byte(77, 4), "reserved flag bits"),
        (Damage::Byte(78, 1), "reserved bytes"),
        (Damage::Byte(88, 1), "cache-line padding"),
        (Damage::Byte(76, 3), "degree 3"),
        (Damage::NonFiniteFactor(64), "non-finite"),
        (Damage::U32(80, 4), "outside row count"),
        (Damage::U32(84, 0), "unused neighbour slot"),
    ];

    for (damage, expected) in cases {
        std::fs::write(&path, &original).expect("restore original segment");
        rewrite_graph_region(&path, id, |graph| {
            match damage {
                Damage::Byte(offset, value) => graph[node_one + offset] = value,
                Damage::U32(offset, value) => graph[node_one + offset..node_one + offset + 4]
                    .copy_from_slice(&value.to_le_bytes()),
                Damage::NonFiniteFactor(offset) => graph[node_one + offset..node_one + offset + 4]
                    .copy_from_slice(&f32::NAN.to_bits().to_le_bytes()),
            }
            repair_graph_checksum(graph);
        });
        let reader = SegmentReader::open(&StdVfs, &path, id).expect("forged segment header");
        reader
            .validate_all()
            .expect("all segment framing checksums were repaired");
        let error = expect_segment_error(
            reader.graph_node_blocks(),
            "malformed node 1 must be rejected",
        );
        assert!(
            matches!(
                &error,
                SegmentError::Graph(GraphNodeError::InvalidNode { node_id: 1, .. })
            ),
            "{error}"
        );
        assert!(error.to_string().contains(expected), "{error}");
    }
}

#[test]
fn corrupt_graph_fallback_retains_the_complete_boundary_tie_set() {
    let layout = GraphNodeLayout::new(2, 128, 2).expect("layout");
    let mut codes = [[0_u8; 64]; 3];
    codes[1][0] = 0x10;
    codes[2][0] = 0x20;
    let factors = [
        Bit4Factors::from_persisted(1.0, 1.0, 0.0),
        Bit4Factors::from_persisted(1.0, 1.0, 0.0),
        Bit4Factors::from_persisted(1.0, 1.0, 0.0),
    ];
    let graph_nodes = [
        GraphNodeBlockInput {
            codes: &codes[0],
            factors: factors[0],
            flags: 1,
            neighbors: &[1, 2],
        },
        GraphNodeBlockInput {
            codes: &codes[1],
            factors: factors[1],
            flags: 0,
            neighbors: &[0],
        },
        GraphNodeBlockInput {
            codes: &codes[2],
            factors: factors[2],
            flags: 0,
            neighbors: &[0],
        },
    ];
    let graph = GraphNodeBlockBuild {
        layout,
        nodes: &graph_nodes,
    };
    let segment_codes = [0x00_u8, 0x10_u8, 0x20_u8];
    let exact_values = vec![1.0_f32, 0.0, 0.0, 1.0, 0.0, 1.0];
    let columns = empty_columns(3);
    let alive = AliveSet::new(3);
    let id = SegmentId::new(19, [3; 10]);
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join(id.file_name());
    write_segment_with_graph(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id,
            scheme: 4,
            dims: 2,
            codes: &segment_codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: &exact_values,
            columns: &columns,
            alive: &alive,
        },
        graph,
        ordered_policy(),
    )
    .expect("segment with graph");

    rewrite_graph_region(&path, id, |graph| graph[0] ^= 1);

    let damaged = SegmentReader::open(&StdVfs, &path, id).expect("bounded header remains valid");
    damaged
        .validate_all()
        .expect("frame, chunk, header, and whole-file checksums were repaired");
    let exact_rows = F32Rows::new(exact_values);
    let query = [1.0_f32, 0.0];
    let outcome = load_graph_or_exact_scan(
        &damaged,
        ScanRequest {
            query: ScanQuery::F32(&query),
            rows: ScanRows::F32RowMajor(&exact_rows),
            row_mask: None,
        },
        2,
    )
    .expect("exact fallback succeeds");
    match outcome {
        GraphLoadOutcome::Graph(_) => panic!("corrupt graph must not load"),
        GraphLoadOutcome::ExactScanFallback { error, candidates } => {
            assert!(matches!(&error, SegmentError::Graph(_)), "{error}");
            assert!(
                error
                    .to_string()
                    .contains("segment graph region is invalid")
            );
            assert!(std::error::Error::source(&error).is_some());
            assert_eq!(
                candidates,
                vec![
                    ScanCandidate {
                        row_id: 0,
                        score: 1.0,
                    },
                    ScanCandidate {
                        row_id: 1,
                        score: 0.0,
                    },
                    ScanCandidate {
                        row_id: 2,
                        score: 0.0,
                    },
                ]
            );
        }
    }

    let empty_rows = F32Rows::new(Vec::new());
    let empty_query = [];
    let fallback_error = load_graph_or_exact_scan(
        &damaged,
        ScanRequest {
            query: ScanQuery::F32(&empty_query),
            rows: ScanRows::F32RowMajor(&empty_rows),
            row_mask: None,
        },
        1,
    )
    .expect_err("invalid exact request remains typed");
    assert!(matches!(fallback_error, GraphQueryError::ExactScan(_)));
    assert!(fallback_error.to_string().contains("exact-scan fallback"));
    assert!(std::error::Error::source(&fallback_error).is_some());
}

#[test]
fn prop_graph_node_block_round_trip_and_corruption_is_typed() {
    let mut runner = TestRunner::new(Config {
        cases: 96,
        rng_seed: RngSeed::Fixed(0x19_02_f1_ed_57_12_1d_e0),
        ..Config::default()
    });
    let tuples = (1_u32..=768, 1_u8..=64, 0_u8..=64, any::<u32>());
    let result = runner.run(&tuples, |(dims, max_degree, requested_degree, id)| {
        let padded_dims = dims.div_ceil(128) * 128;
        let layout = GraphNodeLayout::new(dims, padded_dims, max_degree).expect("layout");
        let node_count = id % 7 + 1;
        let degree = requested_degree.min(max_degree).min(node_count as u8);
        let neighbors = (0..node_count)
            .map(|node_id| {
                (0..u32::from(degree))
                    .map(|slot| (id.wrapping_add(node_id).wrapping_add(slot)) % node_count)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let mut code_rows = Vec::new();
        for node_id in 0..node_count {
            let mut row = vec![0_u8; layout.code_bytes()];
            let logical_bytes = (dims as usize).div_ceil(2);
            for byte in row.iter_mut().take(logical_bytes) {
                *byte = (id as u8).wrapping_add(node_id as u8).wrapping_add(0x31);
            }
            if !dims.is_multiple_of(2) {
                row[logical_bytes - 1] &= 0xf0;
            }
            code_rows.push(row);
        }
        let factor_rows = (0..node_count)
            .map(|node_id| {
                Bit4Factors::from_persisted(
                    1.0 + node_id as f32,
                    2.0 + node_id as f32,
                    -0.5 + node_id as f32,
                )
            })
            .collect::<Vec<_>>();
        let inputs = (0..node_count as usize)
            .map(|node_id| GraphNodeBlockInput {
                codes: &code_rows[node_id],
                factors: factor_rows[node_id],
                flags: id as u8 & 0b11,
                neighbors: &neighbors[node_id],
            })
            .collect::<Vec<_>>();
        let encoded = encode_node_blocks(GraphNodeBlockBuild {
            layout,
            nodes: &inputs,
        })
        .expect("encode");
        let decoded = decode_node_blocks(encoded.as_bytes()).expect("decode");
        prop_assert_eq!(decoded.layout(), layout);
        prop_assert_eq!(decoded.node_count(), node_count);
        let selected_id = id % node_count;
        let selected = decoded.block(selected_id).expect("selected block");
        prop_assert_eq!(selected.codes(), code_rows[selected_id as usize].as_slice());
        prop_assert_eq!(selected.factors(), factor_rows[selected_id as usize]);
        prop_assert_eq!(selected.degree(), degree);
        prop_assert_eq!(selected.flags(), id as u8 & 0b11);
        prop_assert_eq!(selected.reserved_bytes(), [0, 0]);
        let padded = selected.neighbors_padded().collect::<Vec<_>>();
        prop_assert_eq!(
            &padded[..usize::from(degree)],
            neighbors[selected_id as usize].as_slice()
        );
        prop_assert!(
            padded[usize::from(degree)..]
                .iter()
                .all(|neighbor| *neighbor == u32::MAX)
        );
        prop_assert!(selected.padding_bytes().iter().all(|byte| *byte == 0));

        let bytes = encoded.as_bytes();
        let cut = id as usize % bytes.len();
        let truncated = &bytes[..cut];
        prop_assert!(decode_node_blocks(truncated).is_err());

        let mut flipped = bytes.to_vec();
        let flip_index = id as usize % flipped.len();
        flipped[flip_index] ^= 1_u8 << (id % 8);
        prop_assert!(decode_node_blocks(&flipped).is_err());
        Ok(())
    });
    assert!(result.is_ok(), "property result: {result:?}");
}

#[test]
fn graph_node_block_alignment_and_stride() {
    let cases = [
        (128, 128, 44, 256),
        (256, 256, 44, 384),
        (256, 256, 64, 512),
        (100, 128, 64, 384),
        (768, 768, 44, 640),
    ];

    for (dims, padded_dims, max_degree, expected_stride) in cases {
        let layout = GraphNodeLayout::new(dims, padded_dims, max_degree).expect("valid layout");
        assert_eq!(layout.stride(), expected_stride, "d={dims} R={max_degree}");
        assert_eq!(layout.stride() % CACHE_LINE_BYTES as u32, 0);
        assert_eq!(
            layout.block_offset(0).expect("node 0") % CACHE_LINE_BYTES as u32,
            0
        );
        assert_eq!(
            layout.block_offset(1).expect("node 1") % CACHE_LINE_BYTES as u32,
            0
        );

        let codes = vec![0_u8; layout.code_bytes()];
        let nodes = [
            GraphNodeBlockInput {
                codes: &codes,
                factors: Bit4Factors::from_persisted(1.0, 2.0, 3.0),
                flags: 0,
                neighbors: &[],
            },
            GraphNodeBlockInput {
                codes: &codes,
                factors: Bit4Factors::from_persisted(4.0, 5.0, 6.0),
                flags: 0,
                neighbors: &[],
            },
        ];
        let encoded = encode_node_blocks(GraphNodeBlockBuild {
            layout,
            nodes: &nodes,
        })
        .expect("node blocks encode");
        assert_eq!(
            encoded.block_offset(1).expect("encoded node 1"),
            expected_stride
        );
    }
}

#[test]
fn graph_node_block_writer_rejects_invalid_layouts_and_nodes() {
    assert!(GraphNodeLayout::new(0, 128, 1).is_err());
    assert!(GraphNodeLayout::new(129, 128, 1).is_err());
    assert!(GraphNodeLayout::new(100, 100, 1).is_err());

    let layout = GraphNodeLayout::new(100, 128, 1).expect("layout");
    let valid_codes = [0_u8; 64];
    let factors = Bit4Factors::from_persisted(1.0, 2.0, 3.0);
    let short_codes = [0_u8; 63];
    let node = [GraphNodeBlockInput {
        codes: &short_codes,
        factors,
        flags: 0,
        neighbors: &[],
    }];
    assert!(
        encode_node_blocks(GraphNodeBlockBuild {
            layout,
            nodes: &node
        })
        .is_err()
    );

    let mut padded_codes = valid_codes;
    padded_codes[50] = 1;
    let node = [GraphNodeBlockInput {
        codes: &padded_codes,
        factors,
        flags: 0,
        neighbors: &[],
    }];
    assert!(
        encode_node_blocks(GraphNodeBlockBuild {
            layout,
            nodes: &node
        })
        .is_err()
    );

    let odd_layout = GraphNodeLayout::new(99, 128, 1).expect("odd layout");
    let mut odd_codes = valid_codes;
    odd_codes[49] = 0x01;
    let node = [GraphNodeBlockInput {
        codes: &odd_codes,
        factors,
        flags: 0,
        neighbors: &[],
    }];
    assert!(
        encode_node_blocks(GraphNodeBlockBuild {
            layout: odd_layout,
            nodes: &node,
        })
        .is_err()
    );

    let invalid_inputs = [
        GraphNodeBlockInput {
            codes: &valid_codes,
            factors: Bit4Factors::from_persisted(f32::NAN, 2.0, 3.0),
            flags: 0,
            neighbors: &[],
        },
        GraphNodeBlockInput {
            codes: &valid_codes,
            factors,
            flags: 4,
            neighbors: &[],
        },
        GraphNodeBlockInput {
            codes: &valid_codes,
            factors,
            flags: 0,
            neighbors: &[0, 0],
        },
        GraphNodeBlockInput {
            codes: &valid_codes,
            factors,
            flags: 0,
            neighbors: &[1],
        },
    ];
    for input in invalid_inputs {
        let node = [input];
        let error = encode_node_blocks(GraphNodeBlockBuild {
            layout,
            nodes: &node,
        })
        .expect_err("invalid writer input");
        assert!(matches!(error, GraphNodeError::InvalidNode { .. }));
        assert!(error.to_string().contains("graph node 0 is invalid"));
    }
}

#[test]
fn graph_node_block_decoder_rejects_each_malformed_field() {
    let layout = GraphNodeLayout::new(100, 128, 2).expect("layout");
    let codes = [0_u8; 64];
    let nodes = [GraphNodeBlockInput {
        codes: &codes,
        factors: Bit4Factors::from_persisted(1.0, 2.0, 3.0),
        flags: 0,
        neighbors: &[0],
    }];
    let valid = encode_node_blocks(GraphNodeBlockBuild {
        layout,
        nodes: &nodes,
    })
    .expect("valid region")
    .into_bytes();
    assert!(decode_node_blocks(&valid[..127]).is_err());

    let trailer = valid.len() - NODE_BLOCK_TRAILER_LEN;
    let cases = [
        (trailer, 0xff, "magic"),
        (trailer + 8, 0xff, "version"),
        (trailer + 10, 0x10, "unknown refinement-pass bits"),
        (trailer + 21, 0x01, "degree padding"),
        (trailer + 32, 0x01, "xxh3-64"),
        (trailer + 40, 0x01, "reserved trailer"),
    ];
    for (offset, xor, expected) in cases {
        let mut damaged = valid.clone();
        damaged[offset] ^= xor;
        assert_decode_error(&damaged, expected);
    }

    for (offset, value, expected) in [
        (trailer + 12, 0_u32, "logical dimensions"),
        (trailer + 16, 99_u32, "below logical dimensions"),
        (trailer + 16, 100_u32, "not a multiple"),
        (trailer + 24, 256_u32, "stored stride"),
    ] {
        let mut damaged = valid.clone();
        damaged[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        repair_graph_checksum(&mut damaged);
        assert_decode_error(&damaged, expected);
    }

    let block_cases = [
        (50, 1_u8, "padded Bit4 dimensions"),
        (77, 4_u8, "reserved flag bits"),
        (78, 1_u8, "reserved bytes"),
        (88, 1_u8, "cache-line padding"),
        (76, 3_u8, "degree 3"),
    ];
    for (offset, value, expected) in block_cases {
        let mut damaged = valid.clone();
        damaged[offset] = value;
        repair_graph_checksum(&mut damaged);
        assert_decode_error(&damaged, expected);
    }

    let mut non_finite = valid.clone();
    non_finite[64..68].copy_from_slice(&f32::NAN.to_bits().to_le_bytes());
    repair_graph_checksum(&mut non_finite);
    assert_decode_error(&non_finite, "non-finite");

    let mut out_of_range = valid.clone();
    out_of_range[80..84].copy_from_slice(&1_u32.to_le_bytes());
    repair_graph_checksum(&mut out_of_range);
    assert_decode_error(&out_of_range, "outside row count");

    let mut bad_sentinel = valid.clone();
    bad_sentinel[84..88].copy_from_slice(&0_u32.to_le_bytes());
    repair_graph_checksum(&mut bad_sentinel);
    assert_decode_error(&bad_sentinel, "unused neighbour slot");

    let mut declared_two = valid.clone();
    declared_two[trailer + 28..trailer + 32].copy_from_slice(&2_u32.to_le_bytes());
    repair_graph_checksum(&mut declared_two);
    assert_decode_error(&declared_two, "truncated");

    let mut extra_block = valid[..trailer].to_vec();
    extra_block.extend_from_slice(&[0_u8; 128]);
    extra_block.extend_from_slice(&valid[trailer..]);
    repair_graph_checksum(&mut extra_block);
    assert_decode_error(&extra_block, "region length");
}
