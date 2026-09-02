//! Independent, std-only Vamana graph fixture and checks.

use std::cmp::Ordering;
use std::collections::BTreeSet;

pub const I28_CHECKER_ID: &str = "graph.i28.shape.v2";
pub const I29_CHECKER_ID: &str = "graph.i29.entry-points.v2";
pub const I30_CHECKER_ID: &str = "graph.i30.reachability.v2";
pub const I31_CHECKER_ID: &str = "graph.i31.result-soundness.v2";
pub const I32_CHECKER_ID: &str = "graph.i32.bounded-work.v2";
pub const I33_CHECKER_ID: &str = "graph.i33.atomic-publication.v2";
pub const I34_CHECKER_ID: &str = "graph.i34.segment-alignment.v2";
pub const I35_CHECKER_ID: &str = "graph.i35.filtered-soundness.v2";

pub const RECALL_FLOOR_NUMERATOR: usize = 80;
pub const RECALL_FLOOR_DENOMINATOR: usize = 100;
pub const ALLOW_LIST_ROWS_THRESHOLD: u64 = 64;
pub const CHECKPOINT_ROWS: u64 = 64;
pub const MAX_DEGREE: u8 = 44;

const DOCUMENT_LIMIT: u64 = 1_u64 << 53;
const MAINTENANCE_SEED: u64 = 0x20_00c0_ffee;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphRow {
    pub document: u64,
    pub vector_bits: Vec<u32>,
    pub timestamp: i64,
    pub deleted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphInput {
    pub seed: u64,
    pub dims: u32,
    pub k: u32,
    pub max_degree: u8,
    pub large_filter_value: i64,
    pub small_filter_value: i64,
    pub query_bits: Vec<u32>,
    pub rows: Vec<GraphRow>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphCandidate {
    pub document: u64,
    pub score_bits: u32,
    pub segment: [u8; 16],
    pub row: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphObserved {
    pub graph_node_count: u32,
    pub production_live_rows: u64,
    pub graph_max_degree: u8,
    pub maximum_observed_degree: u8,
    pub entry_rows: Vec<u32>,
    pub entry_documents: Vec<u64>,
    pub graphs_built: u64,
    pub graph_segments: u64,
    pub entry_discoveries: u64,
    pub exact_rescore: bool,
    pub graph: Vec<GraphCandidate>,
    pub exact: Vec<GraphCandidate>,
    pub exact_all: Vec<GraphCandidate>,
    pub bounded_budget_exhausted: bool,
    pub checkpoint_exists_after_bounded: bool,
    pub bounded_bytes_consumed: u64,
    pub work_stride: u64,
    pub checkpoints_resumed: u64,
    pub checkpoint_removed_after_resume: bool,
    pub manifest_segments: Vec<[u8; 16]>,
    pub graph_segments_on_disk: u32,
    pub source_segment: [u8; 16],
    pub source_file_exists: bool,
    pub source_manifest_referenced: bool,
    pub temporary_orphans: u32,
    pub reopened_graph: Vec<GraphCandidate>,
    pub filtered_graph: Vec<GraphCandidate>,
    pub filtered_graph_exact: Vec<GraphCandidate>,
    pub filtered_large_cardinality: u64,
    pub filtered_graph_branch: bool,
    pub filtered_graph_exact_rescore: bool,
    pub filtered_small: Vec<GraphCandidate>,
    pub filtered_small_exact: Vec<GraphCandidate>,
    pub filtered_small_cardinality: u64,
    pub filtered_small_exact_allow_list: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedGraphShape {
    pub segment: [u8; 16],
    pub node_count: u32,
    pub max_degree: u8,
    pub maximum_observed_degree: u8,
    pub entry_rows: Vec<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExpectedCandidate {
    document: u64,
    score_bits: u32,
    row: u32,
}

#[derive(Clone, Copy, Debug)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }
}

fn shuffle<T>(values: &mut [T], random: &mut SplitMix64) {
    for end in (1..values.len()).rev() {
        let bound = u64::try_from(end + 1).unwrap_or(u64::MAX);
        let selected = usize::try_from(random.next() % bound).unwrap_or(0);
        values.swap(end, selected);
    }
}

fn generated_f32(random: &mut SplitMix64) -> f32 {
    let mantissa = u32::try_from(random.next() >> 40).unwrap_or(1) | 1;
    let unit = f64::from(mantissa) / f64::from((1_u32 << 24) - 1);
    (unit.mul_add(2.0, -1.0)) as f32
}

fn squared_l2(left: &[u32], right: &[u32]) -> f64 {
    left.iter()
        .zip(right)
        .map(|(left, right)| {
            let delta = f64::from(f32::from_bits(*left)) - f64::from(f32::from_bits(*right));
            delta * delta
        })
        .sum()
}

fn fixture_entry_rows(rows: &[GraphRow]) -> Vec<u32> {
    let mut random = SplitMix64::new(MAINTENANCE_SEED);
    let mut order = (0..u32::try_from(rows.len()).unwrap_or(0)).collect::<Vec<_>>();
    shuffle(&mut order, &mut random);
    let mut scored = order
        .iter()
        .copied()
        .map(|candidate| {
            let candidate_row = &rows[usize::try_from(candidate).unwrap_or(0)];
            let distance = order
                .iter()
                .copied()
                .map(|other| {
                    squared_l2(
                        &candidate_row.vector_bits,
                        &rows[usize::try_from(other).unwrap_or(0)].vector_bits,
                    )
                })
                .sum::<f64>();
            (candidate, distance)
        })
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| {
        left.1
            .total_cmp(&right.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    let Some((medoid, _)) = scored.first().copied() else {
        return Vec::new();
    };
    let mut entries = vec![medoid];
    while entries.len() < 4 && entries.len() < order.len() {
        let mut farthest = None::<(u32, f64)>;
        for candidate in order.iter().copied() {
            if entries.contains(&candidate) {
                continue;
            }
            let nearest = entries
                .iter()
                .copied()
                .fold(f64::INFINITY, |nearest, entry| {
                    nearest.min(squared_l2(
                        &rows[usize::try_from(candidate).unwrap_or(0)].vector_bits,
                        &rows[usize::try_from(entry).unwrap_or(0)].vector_bits,
                    ))
                });
            if farthest.is_none_or(|current| {
                nearest > current.1 || (nearest == current.1 && candidate < current.0)
            }) {
                farthest = Some((candidate, nearest));
            }
        }
        let Some((entry, _)) = farthest else {
            break;
        };
        entries.push(entry);
    }
    entries
}

#[must_use]
pub fn fixture(seed: u64) -> GraphInput {
    let mut random = SplitMix64::new(seed ^ 0x76_61_6d_61_6e_61_02);
    let row_count = 96 + usize::try_from(random.next() % 225).unwrap_or(0);
    let dimensions = [8_usize, 32, 128][usize::try_from(random.next() % 3).unwrap_or(0)];
    let document_base = (random.next() & ((1_u64 << 40) - 1)) << 12;
    let mut documents = (0..row_count)
        .map(|row| document_base + u64::try_from(row).unwrap_or(0) + 1)
        .collect::<Vec<_>>();
    shuffle(&mut documents, &mut random);
    if documents.windows(2).all(|pair| pair[0] < pair[1]) && documents.len() > 1 {
        documents.swap(0, 1);
    }
    let high_end = row_count * 9 / 10;
    let mut rows = (0..row_count)
        .map(|row| {
            let vector_bits = (0..dimensions)
                .map(|_| generated_f32(&mut random).to_bits())
                .collect::<Vec<_>>();
            GraphRow {
                document: documents[row],
                vector_bits,
                timestamp: if row < 4 {
                    2
                } else if row < high_end {
                    0
                } else {
                    1
                },
                deleted: false,
            }
        })
        .collect::<Vec<_>>();
    if rows.len() > 1 {
        rows[1].vector_bits = rows[0].vector_bits.clone();
    }
    let protected = fixture_entry_rows(&rows)
        .into_iter()
        .collect::<BTreeSet<_>>();
    let delete_percent = 5 + usize::try_from(random.next() % 6).unwrap_or(0);
    let delete_count = row_count * delete_percent / 100;
    let mut candidates = (4..row_count)
        .filter(|row| !protected.contains(&u32::try_from(*row).unwrap_or(u32::MAX)))
        .collect::<Vec<_>>();
    shuffle(&mut candidates, &mut random);
    for row in candidates.into_iter().take(delete_count) {
        rows[row].deleted = true;
    }
    let query_bits = (0..dimensions)
        .map(|_| generated_f32(&mut random).to_bits())
        .collect::<Vec<_>>();
    GraphInput {
        seed,
        dims: u32::try_from(dimensions).unwrap_or(0),
        k: 10,
        max_degree: MAX_DEGREE,
        large_filter_value: 0,
        small_filter_value: 2,
        query_bits,
        rows: rows
            .into_iter()
            .map(|mut row| {
                row.vector_bits.shrink_to_fit();
                row
            })
            .collect(),
    }
}

fn expected(input: &GraphInput, predicate: Option<i64>, k: usize) -> Vec<ExpectedCandidate> {
    let query = &input.query_bits;
    let mut candidates = input
        .rows
        .iter()
        .enumerate()
        .filter(|(_, row)| !row.deleted && predicate.is_none_or(|value| row.timestamp == value))
        .map(|(row, input_row)| ExpectedCandidate {
            document: input_row.document,
            score_bits: (-(squared_l2(query, &input_row.vector_bits) as f32)).to_bits(),
            row: u32::try_from(row).unwrap_or(u32::MAX),
        })
        .collect::<Vec<_>>();
    candidates.sort_by(expected_order);
    candidates.truncate(k.min(candidates.len()));
    candidates
}

fn expected_order(left: &ExpectedCandidate, right: &ExpectedCandidate) -> Ordering {
    f32::from_bits(right.score_bits)
        .total_cmp(&f32::from_bits(left.score_bits))
        .then_with(|| left.document.cmp(&right.document))
        .then_with(|| left.row.cmp(&right.row))
}

fn observed_order(left: &GraphCandidate, right: &GraphCandidate) -> Ordering {
    f32::from_bits(right.score_bits)
        .total_cmp(&f32::from_bits(left.score_bits))
        .then_with(|| left.document.cmp(&right.document))
        .then_with(|| left.row.cmp(&right.row))
}

fn mismatch(checker: &str, detail: impl std::fmt::Display) -> Result<(), String> {
    Err(format!("{checker}: {detail}"))
}

fn compare_exact_sequence(
    checker: &str,
    label: &str,
    expected: &[ExpectedCandidate],
    observed: &[GraphCandidate],
) -> Result<(), String> {
    if expected.is_empty() || observed.is_empty() {
        return mismatch(checker, format!("{label} minimum cardinality guard failed"));
    }
    if expected.len() != observed.len() {
        return mismatch(
            checker,
            format!(
                "{label} cardinality expected={} observed={}",
                expected.len(),
                observed.len()
            ),
        );
    }
    for (position, (expected, observed)) in expected.iter().zip(observed).enumerate() {
        if expected.document != observed.document || expected.score_bits != observed.score_bits {
            return mismatch(
                checker,
                format!(
                    "{label}[{position}] expected=({}, {:08x}) observed=({}, {:08x})",
                    expected.document, expected.score_bits, observed.document, observed.score_bits
                ),
            );
        }
    }
    Ok(())
}

fn check_graph_soundness(
    checker: &str,
    label: &str,
    input: &GraphInput,
    predicate: Option<i64>,
    observed: &[GraphCandidate],
) -> Result<(), String> {
    let k = usize::try_from(input.k).unwrap_or(0);
    if observed.len() != k {
        return mismatch(
            checker,
            format!(
                "{label} returned {} candidates, expected {k}",
                observed.len()
            ),
        );
    }
    let mut documents = BTreeSet::new();
    for candidate in observed {
        if !documents.insert(candidate.document) {
            return mismatch(
                checker,
                format!("{label} returned duplicate document {}", candidate.document),
            );
        }
        let Some(row) = input
            .rows
            .iter()
            .find(|row| row.document == candidate.document)
        else {
            return mismatch(
                checker,
                format!("{label} returned unknown document {}", candidate.document),
            );
        };
        if row.deleted || predicate.is_some_and(|value| row.timestamp != value) {
            return mismatch(
                checker,
                format!(
                    "{label} returned deleted or predicate-false document {}",
                    candidate.document
                ),
            );
        }
        let expected_score = (-(squared_l2(&input.query_bits, &row.vector_bits) as f32)).to_bits();
        if candidate.score_bits != expected_score {
            return mismatch(
                checker,
                format!(
                    "{label} document {} score expected={expected_score:08x} observed={:08x}",
                    candidate.document, candidate.score_bits
                ),
            );
        }
    }
    if observed
        .windows(2)
        .any(|pair| observed_order(&pair[0], &pair[1]) == Ordering::Greater)
    {
        return mismatch(
            checker,
            format!("{label} is not best-first with document-id tie ordering"),
        );
    }
    Ok(())
}

fn check_recall(
    checker: &str,
    label: &str,
    expected: &[ExpectedCandidate],
    observed: &[GraphCandidate],
) -> Result<(), String> {
    if expected.is_empty() || observed.is_empty() {
        return mismatch(
            checker,
            format!("{label} recall minimum cardinality guard failed"),
        );
    }
    let expected_documents = expected
        .iter()
        .map(|candidate| candidate.document)
        .collect::<BTreeSet<_>>();
    let retained = observed
        .iter()
        .filter(|candidate| expected_documents.contains(&candidate.document))
        .count();
    if retained * RECALL_FLOOR_DENOMINATOR < expected.len() * RECALL_FLOOR_NUMERATOR {
        return mismatch(
            checker,
            format!("{label} recall below 0.80: {retained}/{}", expected.len()),
        );
    }
    Ok(())
}

pub fn compare_i28(input: &GraphInput, observed: &GraphObserved) -> Result<(), String> {
    let rows = u32::try_from(input.rows.len()).unwrap_or(u32::MAX);
    let live =
        u64::try_from(input.rows.iter().filter(|row| !row.deleted).count()).unwrap_or(u64::MAX);
    if !(96..=320).contains(&rows)
        || ![8, 32, 128].contains(&input.dims)
        || input.rows.iter().any(|row| row.document >= DOCUMENT_LIMIT)
    {
        return mismatch(I28_CHECKER_ID, "seed fixture shape is outside its contract");
    }
    if observed.graph_node_count != rows
        || observed.production_live_rows != live
        || observed.graph_max_degree != input.max_degree
        || observed.maximum_observed_degree > input.max_degree
        || observed.graphs_built != 1
    {
        return mismatch(
            I28_CHECKER_ID,
            format!(
                "published shape nodes={}/{} live={}/{} degree={}/{} observed_max={} built={}",
                observed.graph_node_count,
                rows,
                observed.production_live_rows,
                live,
                observed.graph_max_degree,
                input.max_degree,
                observed.maximum_observed_degree,
                observed.graphs_built
            ),
        );
    }
    Ok(())
}

pub fn compare_i29(input: &GraphInput, observed: &GraphObserved) -> Result<(), String> {
    if observed.entry_rows.len() != 4
        || observed.entry_documents.len() != observed.entry_rows.len()
        || observed.graph_segments == 0
        || observed.entry_discoveries == 0
    {
        return mismatch(
            I29_CHECKER_ID,
            "entry-point minimum/cardinality guard failed",
        );
    }
    let mut entry_rows = BTreeSet::new();
    let mut entry_documents = BTreeSet::new();
    for (row, document) in observed.entry_rows.iter().zip(&observed.entry_documents) {
        if usize::try_from(*row).map_or(true, |row| row >= input.rows.len())
            || !entry_rows.insert(*row)
        {
            return mismatch(I29_CHECKER_ID, format!("entry row {row} is out of range"));
        }
        let Some(input_row) = input.rows.iter().find(|input| input.document == *document) else {
            return mismatch(
                I29_CHECKER_ID,
                format!("entry row {row} names unknown document {document}"),
            );
        };
        if input_row.deleted || !entry_documents.insert(*document) {
            return mismatch(
                I29_CHECKER_ID,
                format!("entry row {row} is deleted or duplicates a document"),
            );
        }
    }
    Ok(())
}

pub fn compare_i30(input: &GraphInput, observed: &GraphObserved) -> Result<(), String> {
    if observed.graph_segments == 0 || observed.entry_discoveries == 0 {
        return mismatch(
            I30_CHECKER_ID,
            "Graph tier did not traverse a graph from persisted entries",
        );
    }
    let expected = expected(input, None, usize::try_from(input.k).unwrap_or(0));
    check_recall(I30_CHECKER_ID, "graph", &expected, &observed.graph)
}

pub fn compare_i31(input: &GraphInput, observed: &GraphObserved) -> Result<(), String> {
    if !observed.exact_rescore {
        return mismatch(I31_CHECKER_ID, "Graph tier did not report exact rescore");
    }
    check_graph_soundness(I31_CHECKER_ID, "graph", input, None, &observed.graph)?;
    let expected = expected(input, None, usize::try_from(input.k).unwrap_or(0));
    compare_exact_sequence(I31_CHECKER_ID, "Exact tier", &expected, &observed.exact)
}

pub fn compare_i32(input: &GraphInput, observed: &GraphObserved) -> Result<(), String> {
    let expected_stride = graph_work_stride(input)?;
    let expected_bytes = expected_stride
        .checked_mul(CHECKPOINT_ROWS)
        .ok_or_else(|| format!("{I32_CHECKER_ID}: work ledger overflow"))?;
    if observed.work_stride != expected_stride
        || !observed.bounded_budget_exhausted
        || !observed.checkpoint_exists_after_bounded
        || observed.bounded_bytes_consumed != expected_bytes
        || observed.checkpoints_resumed != 1
        || !observed.checkpoint_removed_after_resume
        || observed.graphs_built != 1
    {
        return mismatch(
            I32_CHECKER_ID,
            format!(
                "bounded ledger stride={} bytes={}/{} exhausted={} checkpoint={} resumed={} removed={} built={}",
                observed.work_stride,
                observed.bounded_bytes_consumed,
                expected_bytes,
                observed.bounded_budget_exhausted,
                observed.checkpoint_exists_after_bounded,
                observed.checkpoints_resumed,
                observed.checkpoint_removed_after_resume,
                observed.graphs_built
            ),
        );
    }
    Ok(())
}

fn graph_work_stride(input: &GraphInput) -> Result<u64, String> {
    let padded_dims = input
        .dims
        .checked_add(127)
        .map(|dims| dims / 128 * 128)
        .ok_or_else(|| format!("{I32_CHECKER_ID}: padded dimensions overflow"))?;
    let code_bytes = u64::from(padded_dims.div_ceil(2));
    let neighbor_bytes = u64::from(input.max_degree) * 4;
    let unaligned = code_bytes
        .checked_add(16)
        .and_then(|bytes| bytes.checked_add(neighbor_bytes))
        .ok_or_else(|| format!("{I32_CHECKER_ID}: stride arithmetic overflow"))?;
    Ok(unaligned.div_ceil(128) * 128)
}

pub fn compare_i33(_input: &GraphInput, observed: &GraphObserved) -> Result<(), String> {
    if observed.manifest_segments.len() != 1
        || observed.graph_segments_on_disk != 1
        || observed
            .manifest_segments
            .contains(&observed.source_segment)
        || observed.source_file_exists
        || observed.source_manifest_referenced
        || observed.temporary_orphans != 0
    {
        return mismatch(
            I33_CHECKER_ID,
            "publication inventory is not one atomic graph replacement",
        );
    }
    if observed.graph.is_empty() || observed.reopened_graph.is_empty() {
        return mismatch(
            I33_CHECKER_ID,
            "reopen comparison minimum cardinality guard failed",
        );
    }
    if observed.graph != observed.reopened_graph {
        return mismatch(I33_CHECKER_ID, "reopen changed Graph results bit-for-bit");
    }
    Ok(())
}

pub fn compare_i34(input: &GraphInput, observed: &GraphObserved) -> Result<(), String> {
    let live = input.rows.iter().filter(|row| !row.deleted).count();
    if observed.graph.is_empty() || observed.exact_all.len() != live {
        return mismatch(I34_CHECKER_ID, "alignment minimum-cardinality guard failed");
    }
    for graph in &observed.graph {
        let Some(exact) = observed
            .exact_all
            .iter()
            .find(|exact| exact.document == graph.document)
        else {
            return mismatch(
                I34_CHECKER_ID,
                format!(
                    "Graph document {} absent from Exact identity map",
                    graph.document
                ),
            );
        };
        if graph.segment != exact.segment || graph.row != exact.row {
            return mismatch(
                I34_CHECKER_ID,
                format!(
                    "Graph document {} physical identity differs from Exact tier",
                    graph.document
                ),
            );
        }
    }
    Ok(())
}

pub fn compare_i35(input: &GraphInput, observed: &GraphObserved) -> Result<(), String> {
    let k = usize::try_from(input.k).unwrap_or(0);
    let large_expected = expected(input, Some(input.large_filter_value), k);
    let small_expected = expected(input, Some(input.small_filter_value), k);
    if observed.filtered_large_cardinality <= ALLOW_LIST_ROWS_THRESHOLD
        || observed.filtered_small_cardinality == 0
        || observed.filtered_small_cardinality > ALLOW_LIST_ROWS_THRESHOLD
        || !observed.filtered_graph_branch
        || !observed.filtered_small_exact_allow_list
        || !observed.filtered_graph_exact_rescore
    {
        return mismatch(I35_CHECKER_ID, "filtered plan/cardinality guard failed");
    }
    check_graph_soundness(
        I35_CHECKER_ID,
        "FilteredGraph",
        input,
        Some(input.large_filter_value),
        &observed.filtered_graph,
    )?;
    check_recall(
        I35_CHECKER_ID,
        "FilteredGraph",
        &large_expected,
        &observed.filtered_graph,
    )?;
    compare_exact_sequence(
        I35_CHECKER_ID,
        "filtered Exact tier",
        &large_expected,
        &observed.filtered_graph_exact,
    )?;
    compare_exact_sequence(
        I35_CHECKER_ID,
        "small ExactAllowList",
        &small_expected,
        &observed.filtered_small,
    )?;
    compare_exact_sequence(
        I35_CHECKER_ID,
        "small filtered Exact tier",
        &small_expected,
        &observed.filtered_small_exact,
    )
}

fn take<const N: usize>(bytes: &[u8], offset: usize, label: &str) -> Result<[u8; N], String> {
    bytes
        .get(offset..offset.saturating_add(N))
        .and_then(|value| value.try_into().ok())
        .ok_or_else(|| format!("{label} is truncated at {offset}"))
}

fn read_u16(bytes: &[u8], offset: usize, label: &str) -> Result<u16, String> {
    take(bytes, offset, label).map(u16::from_le_bytes)
}

fn read_u32(bytes: &[u8], offset: usize, label: &str) -> Result<u32, String> {
    take(bytes, offset, label).map(u32::from_le_bytes)
}

fn read_u64(bytes: &[u8], offset: usize, label: &str) -> Result<u64, String> {
    take(bytes, offset, label).map(u64::from_le_bytes)
}

pub fn parse_manifest_segment_ids(bytes: &[u8]) -> Result<Vec<[u8; 16]>, String> {
    if bytes.get(..8) != Some(b"ZEPEMBED") {
        return Err("manifest magic differs".to_owned());
    }
    let file_length = usize::try_from(read_u64(bytes, 24, "manifest file length")?)
        .map_err(|_| "manifest file length exceeds usize".to_owned())?;
    if file_length != bytes.len() || read_u64(bytes, 16, "manifest header length")? != 32 {
        return Err("manifest frame geometry differs".to_owned());
    }
    let payload_length = usize::try_from(read_u64(bytes, 32, "manifest payload length")?)
        .map_err(|_| "manifest payload length exceeds usize".to_owned())?;
    let payload = bytes
        .get(40..40_usize.saturating_add(payload_length))
        .ok_or_else(|| "manifest payload is truncated".to_owned())?;
    let segment_count = usize::try_from(read_u32(payload, 16, "manifest segment count")?)
        .map_err(|_| "manifest segment count exceeds usize".to_owned())?;
    let mut ids = Vec::with_capacity(segment_count);
    let mut cursor = 56_usize;
    for _ in 0..segment_count {
        ids.push(take(payload, cursor, "manifest segment id")?);
        cursor = cursor
            .checked_add(52)
            .ok_or_else(|| "manifest segment cursor overflow".to_owned())?;
    }
    Ok(ids)
}

pub fn parse_graph_segment(bytes: &[u8]) -> Result<ParsedGraphShape, String> {
    if bytes.get(..8) != Some(b"ZEPEMBED") {
        return Err("segment magic differs".to_owned());
    }
    let segment = take(bytes, 32, "segment id")?;
    let region_count = usize::from(read_u16(bytes, 52, "segment region count")?);
    let mut graph = None;
    for position in 0..region_count {
        let entry = 64_usize
            .checked_add(position.saturating_mul(32))
            .ok_or_else(|| "segment directory cursor overflow".to_owned())?;
        if read_u16(bytes, entry, "segment region kind")? == 7 {
            let start = usize::try_from(read_u64(bytes, entry + 8, "graph region offset")?)
                .map_err(|_| "graph region offset exceeds usize".to_owned())?;
            let length = usize::try_from(read_u64(bytes, entry + 16, "graph region length")?)
                .map_err(|_| "graph region length exceeds usize".to_owned())?;
            graph = bytes.get(start..start.saturating_add(length));
            break;
        }
    }
    let graph = graph.ok_or_else(|| "segment omitted graph node-block region".to_owned())?;
    let trailer_start = graph
        .len()
        .checked_sub(128)
        .ok_or_else(|| "graph region omitted 128-byte trailer".to_owned())?;
    let trailer = graph
        .get(trailer_start..)
        .ok_or_else(|| "graph trailer is truncated".to_owned())?;
    if trailer.get(..8) != Some(b"ZEGRNB01") {
        return Err("graph trailer magic differs".to_owned());
    }
    let padded_dims = read_u32(trailer, 16, "graph padded dims")?;
    let max_degree = *trailer
        .get(20)
        .ok_or_else(|| "graph max degree is truncated".to_owned())?;
    let stride = usize::try_from(read_u32(trailer, 24, "graph stride")?)
        .map_err(|_| "graph stride exceeds usize".to_owned())?;
    let node_count = read_u32(trailer, 28, "graph node count")?;
    if stride.saturating_mul(usize::try_from(node_count).unwrap_or(usize::MAX)) != trailer_start {
        return Err("graph node geometry differs from region length".to_owned());
    }
    let code_bytes = usize::try_from(padded_dims)
        .unwrap_or(usize::MAX)
        .div_ceil(2);
    let degree_offset = code_bytes.saturating_add(12);
    let flags_offset = degree_offset.saturating_add(1);
    let mut maximum_observed_degree = 0_u8;
    let mut entry_rows = Vec::new();
    for row in 0..node_count {
        let block = usize::try_from(row)
            .unwrap_or(usize::MAX)
            .saturating_mul(stride);
        let degree = *graph
            .get(block.saturating_add(degree_offset))
            .ok_or_else(|| format!("graph row {row} degree is truncated"))?;
        let flags = *graph
            .get(block.saturating_add(flags_offset))
            .ok_or_else(|| format!("graph row {row} flags are truncated"))?;
        if degree > max_degree {
            return Err(format!(
                "graph row {row} degree {degree} exceeds {max_degree}"
            ));
        }
        maximum_observed_degree = maximum_observed_degree.max(degree);
        if flags & 1 != 0 {
            entry_rows.push(row);
        }
    }
    Ok(ParsedGraphShape {
        segment,
        node_count,
        max_degree,
        maximum_observed_degree,
        entry_rows,
    })
}

#[must_use]
pub fn fixture_digest(input: &GraphInput) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut push = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    push(&input.seed.to_le_bytes());
    push(&input.dims.to_le_bytes());
    push(&input.k.to_le_bytes());
    for bits in &input.query_bits {
        push(&bits.to_le_bytes());
    }
    for row in &input.rows {
        push(&row.document.to_le_bytes());
        push(&row.timestamp.to_le_bytes());
        push(&[u8::from(row.deleted)]);
        for bits in &row.vector_bits {
            push(&bits.to_le_bytes());
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observed(input: &GraphInput) -> GraphObserved {
        let exact = expected(input, None, usize::try_from(input.k).expect("k"));
        let live = input.rows.iter().filter(|row| !row.deleted).count();
        let exact_all = expected(input, None, live);
        let large = expected(
            input,
            Some(input.large_filter_value),
            usize::try_from(input.k).expect("k"),
        );
        let small = expected(
            input,
            Some(input.small_filter_value),
            usize::try_from(input.k).expect("k"),
        );
        let segment = [7; 16];
        let candidates = |values: &[ExpectedCandidate]| {
            values
                .iter()
                .map(|candidate| GraphCandidate {
                    document: candidate.document,
                    score_bits: candidate.score_bits,
                    segment,
                    row: candidate.row,
                })
                .collect::<Vec<_>>()
        };
        let entries = fixture_entry_rows(&input.rows);
        GraphObserved {
            graph_node_count: u32::try_from(input.rows.len()).expect("rows"),
            production_live_rows: u64::try_from(live).expect("live"),
            graph_max_degree: MAX_DEGREE,
            maximum_observed_degree: MAX_DEGREE,
            entry_documents: entries
                .iter()
                .map(|row| input.rows[usize::try_from(*row).expect("entry")].document)
                .collect(),
            entry_rows: entries,
            graphs_built: 1,
            graph_segments: 1,
            entry_discoveries: 1,
            exact_rescore: true,
            graph: candidates(&exact),
            exact: candidates(&exact),
            exact_all: candidates(&exact_all),
            bounded_budget_exhausted: true,
            checkpoint_exists_after_bounded: true,
            bounded_bytes_consumed: 64 * 256,
            work_stride: 256,
            checkpoints_resumed: 1,
            checkpoint_removed_after_resume: true,
            manifest_segments: vec![segment],
            graph_segments_on_disk: 1,
            source_segment: [6; 16],
            source_file_exists: false,
            source_manifest_referenced: false,
            temporary_orphans: 0,
            reopened_graph: candidates(&exact),
            filtered_graph: candidates(&large),
            filtered_graph_exact: candidates(&large),
            filtered_large_cardinality: u64::try_from(
                input
                    .rows
                    .iter()
                    .filter(|row| !row.deleted && row.timestamp == input.large_filter_value)
                    .count(),
            )
            .expect("large"),
            filtered_graph_branch: true,
            filtered_graph_exact_rescore: true,
            filtered_small: candidates(&small),
            filtered_small_exact: candidates(&small),
            filtered_small_cardinality: u64::try_from(small.len()).expect("small"),
            filtered_small_exact_allow_list: true,
        }
    }

    #[test]
    fn fixture_is_seeded_nontrivial_and_json_safe() {
        let input = fixture(19);
        assert!((96..=320).contains(&input.rows.len()));
        assert!([8, 32, 128].contains(&input.dims));
        assert!(input.rows.iter().all(|row| row.document < DOCUMENT_LIMIT));
        assert!(
            input
                .rows
                .windows(2)
                .any(|pair| pair[0].document > pair[1].document)
        );
        assert_eq!(input.rows[0].vector_bits, input.rows[1].vector_bits);
        let deleted = input.rows.iter().filter(|row| row.deleted).count();
        assert!(deleted * 100 >= input.rows.len() * 4);
        assert!(deleted * 100 <= input.rows.len() * 10);
        assert!(fixture_digest(&input) != fixture_digest(&fixture(20)));
    }

    #[test]
    fn every_graph_invariant_accepts_a_valid_observation() {
        let input = fixture(7);
        let observed = observed(&input);
        for check in [
            compare_i28,
            compare_i29,
            compare_i30,
            compare_i31,
            compare_i32,
            compare_i33,
            compare_i34,
            compare_i35,
        ] {
            check(&input, &observed).expect("valid graph observation");
        }
    }

    #[test]
    fn every_graph_invariant_rejects_its_deliberate_plant() {
        let input = fixture(7);
        let valid = observed(&input);

        let mut plant = valid.clone();
        plant.graph_node_count -= 1;
        assert!(compare_i28(&input, &plant).is_err());
        let mut plant = valid.clone();
        plant.entry_rows.clear();
        assert!(compare_i29(&input, &plant).is_err());
        let mut plant = valid.clone();
        plant.graph.clear();
        assert!(compare_i30(&input, &plant).is_err());
        let mut plant = valid.clone();
        plant.graph[0].score_bits ^= 1;
        assert!(compare_i31(&input, &plant).is_err());
        let mut plant = valid.clone();
        plant.checkpoint_exists_after_bounded = false;
        assert!(compare_i32(&input, &plant).is_err());
        let mut plant = valid.clone();
        plant.source_file_exists = true;
        assert!(compare_i33(&input, &plant).is_err());
        let mut plant = valid.clone();
        plant.graph[0].row ^= 1;
        assert!(compare_i34(&input, &plant).is_err());
        let mut plant = valid;
        plant.filtered_graph_branch = false;
        assert!(compare_i35(&input, &plant).is_err());
    }
}
