#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use zeppelin_embed::format::golden::decode_hex;
use zeppelin_embed::fts::graph_build::{
    DecodedGraphLexical, GraphLexicalBuilder, GraphLexicalError, PreparedGraphLexical,
};
use zeppelin_embed::fts::index::{Document, FieldId, SegmentIndex};
use zeppelin_embed::fts::postings::{Posting, PostingsError, PostingsReader};
use zeppelin_embed::fts::sealed::{SealedSegment, SealedSegmentError};
use zeppelin_embed::fts::tokenizer::{Analyzer, TokenizerConfig};
use zeppelin_embed::lifecycle::SnapshotLease;
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::property_graph::query::resources::QueryMemory;
use zeppelin_embed::property_graph::query::runtime::{
    RetainedView, RuntimeContext, RuntimeError, RuntimeLimits, WorkKind,
};
use zeppelin_embed::property_graph::query::{QueryError, QueryView};
use zeppelin_embed::property_graph::resources::GraphResources;
use zeppelin_embed::property_graph::staging::{WriteLimits, WriteMemory};
use zeppelin_embed::property_graph::storage::memory::StorageMemory;
use zeppelin_embed::property_graph::storage::tree::directory::TreeResources;
use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};

struct View {
    token: QueryView,
    lease: SnapshotLease,
}

impl RetainedView for View {
    fn query_view(&self) -> &QueryView {
        &self.token
    }
    fn check_active(&self) -> Result<(), QueryError> {
        self.lease
            .check_active()
            .map_err(|_| QueryError::ReadCancelled)
    }
}

fn view(store: &Store) -> View {
    View {
        token: QueryView::new(
            StoreInstanceId::new(1).expect("store identity"),
            GraphGeneration::new(0),
        ),
        lease: store.snapshot().expect("snapshot"),
    }
}

fn build<'m>(
    analyzer: &Analyzer,
    memory: &'m StorageMemory<'m>,
    resources: &mut TreeResources<'_>,
    texts: &[&str],
) -> PreparedGraphLexical<'m> {
    let mut builder = GraphLexicalBuilder::new(analyzer, memory, resources).expect("builder");
    for text in texts {
        builder.push_text(text, resources).expect("lexical row");
    }
    builder
        .finish(resources)
        .expect("prepared lexical fragment")
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("u32"))
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("u64"))
}

fn literal_postings(region: &[u8]) -> Vec<(String, Vec<Posting>)> {
    let rows = read_u32(region, 8) as usize;
    let spans = read_u32(region, 12) as usize;
    let fields = read_u32(region, 16) as usize;
    let terms_len = read_u64(region, 24) as usize;
    let span_start = 40;
    let fields_start = span_start + spans * 48;
    let terms_start = fields_start + fields * (8 + rows * 4);
    let blob_start = terms_start + terms_len;
    let mut decoded = Vec::new();
    for index in 0..spans {
        let span = span_start + index * 48;
        let term_start = read_u32(region, span + 4) as usize;
        let term_len = read_u32(region, span + 8) as usize;
        let list_start = read_u32(region, span + 12) as usize - 16;
        let list_end = if index + 1 == spans {
            region.len() - blob_start
        } else {
            read_u32(region, span_start + (index + 1) * 48 + 12) as usize - 16
        };
        let term = std::str::from_utf8(
            &region[terms_start + term_start..terms_start + term_start + term_len],
        )
        .expect("term utf8")
        .to_owned();
        let reader = PostingsReader::open(&region[blob_start + list_start..blob_start + list_end])
            .expect("posting list");
        decoded.push((
            term,
            reader.decode_all().expect("postings").postings().to_vec(),
        ));
    }
    decoded
}

fn term_postings<'a>(postings: &'a [(String, Vec<Posting>)], term: &str) -> &'a [Posting] {
    postings
        .iter()
        .find(|(candidate, _)| candidate == term)
        .map(|(_, postings)| postings.as_slice())
        .unwrap_or(&[])
}

#[test]
fn lexical_rows_skip_analyzed_empty_and_match_region_golden() {
    let directory = tempfile::tempdir().expect("store fixture");
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(128 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("graph resources");
    let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("write memory");
    let control = QueryControl::Cancel(CancelToken::new());
    let memory =
        StorageMemory::new(&writer, &control, 32 * 1024 * 1024).expect("preparation memory");
    let mut resources = TreeResources::for_prepare(&memory, 100_000_000).expect("tree resources");
    let analyzer = Analyzer::new(TokenizerConfig::text_default()).expect("valid analyzer");
    let empty = GraphLexicalBuilder::new(&analyzer, &memory, &mut resources)
        .expect("empty builder")
        .finish(&mut resources)
        .expect("empty fragment");
    assert_eq!(empty.decoded().row_count(), 0);
    assert_eq!(empty.decoded().row_lengths(), &[]);
    assert_eq!(empty.decoded().total_tokens(), 0);
    let empty_decoded = DecodedGraphLexical::decode_prepare(
        empty.region(),
        analyzer.epoch(),
        &memory,
        &mut resources,
    )
    .expect("empty decode");
    assert_eq!(empty_decoded.row_count(), 0);
    drop(empty_decoded);
    drop(empty);
    let mut builder =
        GraphLexicalBuilder::new(&analyzer, &memory, &mut resources).expect("builder");
    for text in ["", " \n\t ", "the and"] {
        assert_eq!(
            builder.push_text(text, &mut resources).expect("empty row"),
            None
        );
    }
    assert_eq!(
        builder
            .push_text("bronze zeppelin", &mut resources)
            .expect("row 0"),
        Some(0)
    );
    assert_eq!(
        builder
            .push_text("silver zeppelin", &mut resources)
            .expect("row 1"),
        Some(1)
    );
    let prepared = builder
        .finish(&mut resources)
        .expect("prepared lexical fragment");
    let expected = decode_hex(include_str!(
        "fixtures/format/postings_segment_region_v1.hex"
    ))
    .expect("frozen region");
    assert_eq!(prepared.region(), expected);
    assert_eq!(prepared.decoded().epoch(), analyzer.epoch());
    assert_eq!(prepared.decoded().row_count(), 2);
    assert_eq!(prepared.decoded().row_lengths(), &[2, 2]);
    assert_eq!(prepared.decoded().total_tokens(), 4);

    let decoded = DecodedGraphLexical::decode_prepare(
        prepared.region(),
        analyzer.epoch(),
        &memory,
        &mut resources,
    )
    .expect("preparation decode");
    assert_eq!(decoded.row_count(), 2);
    assert_eq!(decoded.row_lengths(), &[2, 2]);
    assert_eq!(decoded.total_tokens(), 4);

    let retained = view(&store);
    let query_memory = QueryMemory::new(&shared, 1024 * 1024).expect("query memory");
    let query_control = QueryControl::Cancel(CancelToken::new());
    let mut context = RuntimeContext::new(
        &retained,
        &query_control,
        &query_memory,
        RuntimeLimits::default(),
    )
    .expect("runtime");
    let query_decoded = DecodedGraphLexical::decode_query(
        prepared.region(),
        analyzer.epoch(),
        &query_memory,
        &mut context,
    )
    .expect("query decode");
    assert_eq!(query_decoded.row_count(), 2);
    assert_eq!(query_decoded.row_lengths(), &[2, 2]);

    drop(query_decoded);
    drop(context);
    drop(retained);
    drop(decoded);
    drop(prepared);
    drop(resources);
    drop(memory);
    drop(shared);
    store.close().expect("close store");
}

#[test]
fn lexical_positions_keep_gaps_and_stacked_variants() {
    let directory = tempfile::tempdir().expect("store fixture");
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(128 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("resources");
    let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("writer");
    let control = QueryControl::Cancel(CancelToken::new());
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).expect("memory");
    let mut resources = TreeResources::for_prepare(&memory, 100_000_000).expect("tree");
    let analyzer = Analyzer::new(TokenizerConfig::text_default()).expect("analyzer");
    let prepared = build(
        &analyzer,
        &memory,
        &mut resources,
        &["the bronze and zeppelin", "twenty five"],
    );
    assert_eq!(prepared.decoded().row_lengths(), &[4, 2]);
    let postings = literal_postings(prepared.region());
    assert_eq!(
        term_postings(&postings, "bronz"),
        &[Posting {
            docid: 0,
            tf: 1,
            positions: vec![1]
        }]
    );
    assert_eq!(
        term_postings(&postings, "zeppelin"),
        &[Posting {
            docid: 0,
            tf: 1,
            positions: vec![3]
        }]
    );
    assert_eq!(
        term_postings(&postings, "25"),
        &[Posting {
            docid: 1,
            tf: 1,
            positions: vec![0]
        }]
    );
    assert_eq!(
        term_postings(&postings, "twentyfive"),
        &[Posting {
            docid: 1,
            tf: 1,
            positions: vec![0]
        }]
    );
    assert_eq!(
        term_postings(&postings, "five"),
        &[Posting {
            docid: 1,
            tf: 1,
            positions: vec![1]
        }]
    );
    drop(prepared);
    drop(resources);
    drop(memory);
    drop(shared);
    store.close().expect("close");
}

#[test]
fn lexical_owner_mismatch_and_failed_builder_are_terminal() {
    let directory = tempfile::tempdir().expect("store fixture");
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(128 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("resources");
    let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("writer");
    let control = QueryControl::Cancel(CancelToken::new());
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).expect("memory");
    let other = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).expect("other memory");
    let mut resources = TreeResources::for_prepare(&memory, 100_000_000).expect("tree");
    let analyzer = Analyzer::new(TokenizerConfig::text_default()).expect("analyzer");
    let other_before = other.reserved_bytes();
    assert!(matches!(
        GraphLexicalBuilder::new(&analyzer, &other, &mut resources),
        Err(GraphLexicalError::Resource(
            zeppelin_embed::property_graph::storage::tree::directory::TreeError::Invalid(
                "storage preparation owner mismatch"
            )
        ))
    ));
    assert_eq!(other.reserved_bytes(), other_before);

    let baseline = memory.reserved_bytes();
    let mut builder =
        GraphLexicalBuilder::new(&analyzer, &memory, &mut resources).expect("builder");
    builder
        .push_text("bronze", &mut resources)
        .expect("first row");
    let mut exhausted = TreeResources::for_prepare(&memory, 1).expect("tight work");
    assert!(matches!(
        builder.push_text("a very long second lexical row", &mut exhausted),
        Err(GraphLexicalError::Resource(_))
    ));
    assert!(matches!(
        builder.finish(&mut resources),
        Err(GraphLexicalError::Failed)
    ));
    drop(exhausted);
    assert_eq!(memory.reserved_bytes(), baseline);

    let retained = view(&store);
    let query_memory = QueryMemory::new(&shared, 1024 * 1024).expect("query memory");
    let other_query = QueryMemory::new(&shared, 1024 * 1024).expect("other query");
    let query_control = QueryControl::Cancel(CancelToken::new());
    let mut context = RuntimeContext::new(
        &retained,
        &query_control,
        &query_memory,
        RuntimeLimits::default(),
    )
    .expect("runtime");
    let bytes = decode_hex(include_str!(
        "fixtures/format/postings_segment_region_v1.hex"
    ))
    .expect("region");
    let before = other_query.reserved_bytes();
    assert!(matches!(
        DecodedGraphLexical::decode_query(&bytes, analyzer.epoch(), &other_query, &mut context),
        Err(GraphLexicalError::Resource(
            zeppelin_embed::property_graph::storage::tree::directory::TreeError::Invalid(
                "lexical query owner mismatch"
            )
        ))
    ));
    assert_eq!(other_query.reserved_bytes(), before);
    drop(context);
    drop(retained);
    drop(resources);
    drop(other);
    drop(memory);
    drop(shared);
    store.close().expect("close");
}

#[test]
fn lexical_query_decode_charges_actual_work() {
    let directory = tempfile::tempdir().expect("store fixture");
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(128 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("resources");
    let memory = QueryMemory::new(&shared, 1024 * 1024).expect("query memory");
    let retained = view(&store);
    let control = QueryControl::Cancel(CancelToken::new());
    let bytes = decode_hex(include_str!(
        "fixtures/format/postings_segment_region_v1.hex"
    ))
    .expect("region");
    let epoch = TokenizerConfig::text_default().epoch();
    let mut context = RuntimeContext::new(&retained, &control, &memory, RuntimeLimits::default())
        .expect("runtime");
    let decoded =
        DecodedGraphLexical::decode_query(&bytes, epoch, &memory, &mut context).expect("decode");
    let counters = context.counters();
    assert_eq!(counters.get(WorkKind::LexicalBlocks), 6);
    assert_eq!(counters.get(WorkKind::LexicalPostings), 4);
    assert_eq!(counters.get(WorkKind::CopiedBytes), 349);
    context.checkpoint().expect("runtime remains usable");
    drop(decoded);
    drop(context);

    for kind in [WorkKind::LexicalBlocks, WorkKind::LexicalPostings] {
        let limits = RuntimeLimits::default()
            .with_limit(kind, 0)
            .expect("zero limit");
        let mut limited =
            RuntimeContext::new(&retained, &control, &memory, limits).expect("limited runtime");
        assert!(matches!(
            DecodedGraphLexical::decode_query(&bytes, epoch, &memory, &mut limited),
            Err(GraphLexicalError::Resource(zeppelin_embed::property_graph::storage::tree::directory::TreeError::Runtime(
                RuntimeError::Limit(actual)
            ))) if actual == kind
        ));
    }

    let tight_limit =
        std::mem::size_of::<QueryMemory<'_>>() + std::mem::size_of::<RuntimeContext<'_, '_, '_>>();
    let tight_memory = QueryMemory::new(&shared, tight_limit).expect("tight query memory");
    let tight_before = tight_memory.reserved_bytes();
    let mut tight_context =
        RuntimeContext::new(&retained, &control, &tight_memory, RuntimeLimits::default())
            .expect("tight runtime");
    let context_bytes = tight_memory.reserved_bytes();
    assert!(matches!(
        DecodedGraphLexical::decode_query(&bytes, epoch, &tight_memory, &mut tight_context),
        Err(GraphLexicalError::Resource(
            zeppelin_embed::property_graph::storage::tree::directory::TreeError::Runtime(
                RuntimeError::Memory(
                    zeppelin_embed::property_graph::query::resources::MemoryError::Limit
                )
            )
        ))
    ));
    assert_eq!(tight_memory.reserved_bytes(), context_bytes);
    drop(tight_context);
    assert_eq!(tight_memory.reserved_bytes(), tight_before);
    drop(tight_memory);

    drop(retained);
    drop(memory);
    drop(shared);
    store.close().expect("close");

    let closing_store = std::sync::Arc::new(
        Store::open(
            directory.path().join("close-first"),
            OpenOptions::new()
                .with_max_resident_bytes(128 * 1024 * 1024)
                .with_reader_drain_timeout(std::time::Duration::ZERO),
        )
        .expect("close-first store"),
    );
    let closing_shared = GraphResources::from_store(&closing_store).expect("closing resources");
    let closing_memory = QueryMemory::new(&closing_shared, 1024 * 1024).expect("closing memory");
    let closing_view = view(&closing_store);
    let token = CancelToken::new();
    let closing_control = QueryControl::Cancel(token.clone());
    let mut closing_context = RuntimeContext::new(
        &closing_view,
        &closing_control,
        &closing_memory,
        RuntimeLimits::default(),
    )
    .expect("closing context");
    let close_store = std::sync::Arc::clone(&closing_store);
    let closing = std::thread::spawn(move || close_store.close());
    closing_view
        .lease
        .wait_for_close_cancellation()
        .expect("close cancellation");
    token.cancel();
    assert!(matches!(
        DecodedGraphLexical::decode_query(&bytes, epoch, &closing_memory, &mut closing_context),
        Err(GraphLexicalError::Resource(
            zeppelin_embed::property_graph::storage::tree::directory::TreeError::Runtime(
                RuntimeError::Value(QueryError::ReadCancelled)
            )
        ))
    ));
    drop(closing_context);
    drop(closing_view);
    closing
        .join()
        .expect("close thread")
        .expect("close-first close");
}

#[test]
fn lexical_decode_rejects_corruption_without_retaining_memory() {
    let directory = tempfile::tempdir().expect("store fixture");
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(128 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("resources");
    let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("writer");
    let control = QueryControl::Cancel(CancelToken::new());
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).expect("memory");
    let mut resources = TreeResources::for_prepare(&memory, 100_000_000).expect("tree");
    let analyzer = Analyzer::new(TokenizerConfig::text_default()).expect("analyzer");
    let clean = decode_hex(include_str!(
        "fixtures/format/postings_segment_region_v1.hex"
    ))
    .expect("region");
    let baseline = memory.reserved_bytes();

    let mut reserved = clean.clone();
    reserved[20] = 1;
    assert!(matches!(
        DecodedGraphLexical::decode_prepare(&reserved, analyzer.epoch(), &memory, &mut resources),
        Err(GraphLexicalError::Region(SealedSegmentError::Geometry(
            "header reserved field is nonzero"
        )))
    ));
    assert_eq!(memory.reserved_bytes(), baseline);
    let mut mismatched_block_geometry = clean.clone();
    mismatched_block_geometry[6..8].copy_from_slice(&1_u16.to_le_bytes());
    assert!(matches!(
        DecodedGraphLexical::decode_prepare(
            &mismatched_block_geometry,
            analyzer.epoch(),
            &memory,
            &mut resources,
        ),
        Err(GraphLexicalError::Region(SealedSegmentError::Geometry(
            "posting-list summary does not match span"
        )))
    ));
    assert_eq!(memory.reserved_bytes(), baseline);
    assert!(matches!(
        DecodedGraphLexical::decode_prepare(
            &clean[..clean.len() - 1],
            analyzer.epoch(),
            &memory,
            &mut resources
        ),
        Err(GraphLexicalError::Region(SealedSegmentError::Geometry(
            "region is truncated"
        )))
    ));
    assert_eq!(memory.reserved_bytes(), baseline);

    let repeated = build(&analyzer, &memory, &mut resources, &["zeppelin zeppelin"]);
    let mut corrupt_positions = repeated.region().to_vec();
    // Frozen one-span layout: 40-byte header + 48-byte span + 12-byte field,
    // 8 term bytes, then posting header/meta/tf. The position byte is 157.
    const SECOND_POSITION_BYTE: usize = 157;
    corrupt_positions[SECOND_POSITION_BYTE] &= !2;
    assert!(matches!(
        DecodedGraphLexical::decode_prepare(
            &corrupt_positions,
            analyzer.epoch(),
            &memory,
            &mut resources
        ),
        Err(GraphLexicalError::Postings(
            PostingsError::PositionsNotAscending { position: 0 }
        ))
    ));
    drop(repeated);
    assert_eq!(memory.reserved_bytes(), baseline);

    let mut document = Document::new();
    document.set(FieldId(1), "zeppelin");
    let mut legacy = SegmentIndex::new();
    legacy
        .push_document(&analyzer, &document)
        .expect("legacy row");
    let legacy = SealedSegment::seal(&legacy)
        .expect("legacy seal")
        .encode_region()
        .expect("legacy region");
    assert!(matches!(
        DecodedGraphLexical::decode_prepare(&legacy, analyzer.epoch(), &memory, &mut resources),
        Err(GraphLexicalError::Region(SealedSegmentError::Geometry(
            "field header is invalid"
        )))
    ));
    assert_eq!(memory.reserved_bytes(), baseline);
    drop(resources);
    drop(memory);
    drop(shared);
    store.close().expect("close");
}
