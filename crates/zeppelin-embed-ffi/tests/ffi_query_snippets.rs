//! `ze_query_with_snippets`: per-hit excerpts whose highlighted ranges come
//! from the engine's own analyzer and the terms the query scored (ZE-215).
#![allow(clippy::expect_used, clippy::indexing_slicing)]

mod common;

use std::mem::size_of;

use zeppelin_embed_ffi::*;

const DIMENSION: usize = 2;

fn ingest(handle: ZeHandle, rows: &[(u64, [f32; 2], Option<&str>)]) {
    let documents = rows
        .iter()
        .map(|(id, vector, text)| ZeIngestDocument {
            abi_size: size_of::<ZeIngestDocument>() as u32,
            abi_reserved: 0,
            doc_id: ZeDocId { high: 0, low: *id },
            revision: 1,
            timestamp: *id as i64,
            vector: vector.as_ptr(),
            vector_len: vector.len(),
            metadata: std::ptr::null(),
            metadata_len: 0,
            text: text.map_or(std::ptr::null(), str::as_ptr),
            text_len: text.map_or(0, str::len),
        })
        .collect::<Vec<_>>();
    let request = ZeIngestRequest {
        abi_size: size_of::<ZeIngestRequest>() as u32,
        abi_reserved: 0,
        documents: documents.as_ptr(),
        document_count: documents.len(),
        dimension: DIMENSION,
    };
    let mut report: ZeMutationReport = common::sized_zeroed();
    assert_eq!(ze_ingest(handle, &request, &mut report), ZeErrorCode::ZeOk);
}

fn lexical(text: &[u8], last_as_prefix: bool) -> ZeQueryRequest {
    let mut request = common::valid_query_request(&[]);
    request.text = text.as_ptr();
    request.text_len = text.len();
    request.k = 10;
    request.lexical_flags = if last_as_prefix {
        ZE_QUERY_LAST_AS_PREFIX
    } else {
        0
    };
    request
}

fn last_error(handle: ZeHandle) -> String {
    let mut buffer = vec![0_u8; 512];
    let mut written = 0_usize;
    ze_last_error_message(
        handle,
        buffer.as_mut_ptr().cast(),
        buffer.len(),
        &mut written,
    );
    buffer.truncate(written.min(buffer.len()));
    String::from_utf8_lossy(&buffer).into_owned()
}

/// One hit as plain data: id, whether the lexical leg matched it (a positive
/// BM25; fusion reports zero for a row the lexical leg did not match), and its
/// snippet as (excerpt, marked substrings, truncated start, truncated end).
type Hit = (u64, bool, Option<(String, Vec<String>, bool, bool)>);

fn run(handle: ZeHandle, request: &ZeQueryRequest, snippet_bytes: usize) -> Vec<Hit> {
    let mut result: ZeQueryResult = common::sized_zeroed();
    let mut snippets: ZeQuerySnippets = common::sized_zeroed();
    let code = ze_query_with_snippets(handle, request, snippet_bytes, &mut result, &mut snippets);
    assert_eq!(code, ZeErrorCode::ZeOk, "{}", last_error(handle));
    assert_eq!(snippets.snippet_count, result.hit_count);
    let mut hits = Vec::new();
    for index in 0..result.hit_count {
        let hit = unsafe { *result.hits.add(index) };
        let snippet = unsafe { *snippets.snippets.add(index) };
        assert_eq!(snippet.reserved, 0);
        let mut source: ZeSnippetSourceRanges = common::sized_zeroed();
        assert_eq!(
            ze_query_snippet_source_ranges(&snippets, index, &mut source),
            ZeErrorCode::ZeOk
        );
        assert_eq!(source.highlight_count, snippet.highlight_count);
        assert_eq!(source.source_end - source.source_start, snippet.text_len);
        if snippet.has_snippet == 0 {
            assert_eq!((source.source_start, source.source_end), (0, 0));
        }
        if source.highlight_count == 0 {
            assert!(source.highlights.is_null());
        }
        let decoded = (snippet.has_snippet == 1).then(|| {
            let text = unsafe { std::slice::from_raw_parts(snippet.text, snippet.text_len) };
            let text = std::str::from_utf8(text).expect("excerpt is UTF-8");
            let highlights = if snippet.highlight_count == 0 {
                &[][..]
            } else {
                unsafe { std::slice::from_raw_parts(snippet.highlights, snippet.highlight_count) }
            };
            let marked = highlights
                .iter()
                .map(|range| {
                    text.get(range.start..range.end)
                        .expect("highlight is a character-boundary range")
                        .to_owned()
                })
                .collect();
            (
                text.to_owned(),
                marked,
                snippet.truncated_start == 1,
                snippet.truncated_end == 1,
            )
        });
        if snippet.has_snippet == 0 {
            assert!(snippet.text.is_null() && snippet.highlights.is_null());
        }
        let matched = hit.has_lexical_score == 1 && hit.lexical_bm25 > 0.0;
        hits.push((hit.doc_id.low, matched, decoded));
    }
    assert_eq!(ze_query_snippets_free(&mut snippets), ZeErrorCode::ZeOk);
    assert_eq!(ze_query_result_free(&mut result), ZeErrorCode::ZeOk);
    hits
}

fn marked(hits: &[Hit], id: u64) -> Vec<String> {
    hits.iter()
        .find(|hit| hit.0 == id)
        .and_then(|hit| hit.2.as_ref())
        .map(|snippet| snippet.1.clone())
        .expect("hit with a snippet")
}

#[test]
fn lexical_snippets_mark_the_analyzed_surface_forms_of_every_hit() {
    let store = common::TestStore::new();
    ingest(
        store.handle,
        &[
            (1, [1.0, 0.0], Some("Boats leave the HARBOURS at dawn")),
            (2, [0.0, 1.0], Some("a harbour light")),
            (3, [0.5, 0.5], Some("nothing relevant")),
        ],
    );
    // "harbours" stems to "harbour"; case folds. The engine marks the
    // original surface text, whatever form it took.
    let hits = run(store.handle, &lexical(b"harbours", false), 64);
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|hit| hit.1 && hit.2.is_some()));
    assert_eq!(marked(&hits, 1), vec!["HARBOURS"]);
    assert_eq!(marked(&hits, 2), vec!["harbour"]);
    let first = hits.iter().find(|hit| hit.0 == 1).expect("hit 1");
    let (text, _, truncated_start, truncated_end) = first.2.clone().expect("snippet");
    assert_eq!(text, "HARBOURS at dawn");
    assert!(
        truncated_start,
        "the excerpt starts after \"Boats leave the \""
    );
    assert!(!truncated_end, "the excerpt reaches the end of the text");
}

#[test]
fn prefix_snippets_mark_every_term_the_prefix_expanded_to() {
    let store = common::TestStore::new();
    ingest(
        store.handle,
        &[
            (1, [1.0, 0.0], Some("harbour and harbinger")),
            (2, [0.0, 1.0], Some("the harp")),
        ],
    );
    let hits = run(store.handle, &lexical(b"harb", true), 64);
    assert_eq!(hits.len(), 1);
    assert_eq!(marked(&hits, 1), vec!["harbour", "harbinger"]);
    // Without the prefix flag "harb" is a whole term that nothing carries.
    assert!(run(store.handle, &lexical(b"harb", false), 64).is_empty());
}

#[test]
fn hybrid_snippets_are_absent_only_for_hits_the_lexical_leg_did_not_score() {
    let store = common::TestStore::new();
    ingest(
        store.handle,
        &[
            (1, [1.0, 0.0], Some("harbour lights at dusk")),
            (2, [0.9, 0.1], Some("quantized vectors")),
            (3, [0.8, 0.2], None),
        ],
    );
    let vector = [1.0_f32, 0.0];
    let mut request = common::valid_query_request(&vector);
    request.text = b"harbour".as_ptr();
    request.text_len = b"harbour".len();
    request.k = 3;
    let hits = run(store.handle, &request, 64);
    assert_eq!(hits.len(), 3);
    for (id, lexical, snippet) in &hits {
        assert_eq!(*lexical, *id == 1, "hit {id}");
        assert_eq!(snippet.is_some(), *id == 1, "hit {id}");
    }
    assert_eq!(marked(&hits, 1), vec!["harbour"]);

    let prefix = {
        let mut request = request;
        request.text = b"harb".as_ptr();
        request.text_len = b"harb".len();
        request.lexical_flags = ZE_QUERY_LAST_AS_PREFIX;
        run(store.handle, &request, 64)
    };
    assert_eq!(marked(&prefix, 1), vec!["harbour"]);
}

#[test]
fn non_ascii_highlights_are_the_matched_surface_forms() {
    let store = common::TestStore::new();
    ingest(
        store.handle,
        &[
            (1, [1.0, 0.0], Some("Le CAFÉ est fermé")),
            (2, [0.0, 1.0], Some("🚀🚀 launch the 👩\u{200D}🚀 rocket")),
            (3, [0.5, 0.5], Some("東京 と 中文 の テスト")),
        ],
    );
    // Folding: "cafe" matches "CAFÉ", a two-byte É included.
    let cafe = run(store.handle, &lexical(b"cafe", false), 64);
    assert_eq!(marked(&cafe, 1), vec!["CAFÉ"]);
    let accented = run(store.handle, &lexical("fermé".as_bytes(), false), 64);
    assert_eq!(marked(&accented, 1), vec!["fermé"]);

    // Four-byte emoji and a ZWJ sequence before the match shift byte offsets.
    let rocket = run(store.handle, &lexical(b"rocket", false), 64);
    assert_eq!(marked(&rocket, 2), vec!["rocket"]);
    let launch = run(store.handle, &lexical(b"launch", false), 64);
    assert_eq!(marked(&launch, 2), vec!["launch"]);

    let cjk = run(store.handle, &lexical("中文".as_bytes(), false), 64);
    assert_eq!(cjk.len(), 1);
    let marks = marked(&cjk, 3);
    assert!(!marks.is_empty());
    assert!(
        marks.iter().all(|mark| "中文".contains(mark.as_str())),
        "CJK highlights {marks:?} are not the matched characters"
    );
}

#[test]
fn the_excerpt_is_bounded_by_snippet_bytes_and_reports_both_cuts() {
    let store = common::TestStore::new();
    let text = format!(
        "{}harbour lights{}",
        "alpha ".repeat(40),
        " omega".repeat(40)
    );
    ingest(store.handle, &[(1, [1.0, 0.0], Some(text.as_str()))]);
    for window in [1_usize, 7, 16, 64] {
        let hits = run(store.handle, &lexical(b"harbour", false), window);
        let (excerpt, marks, truncated_start, truncated_end) = hits[0].2.clone().expect("snippet");
        assert!(
            excerpt.len() <= window + 3,
            "window {window}: excerpt of {} bytes",
            excerpt.len()
        );
        assert!(excerpt.starts_with(&"harbour"[..window.min(7)]));
        assert!(truncated_start && truncated_end, "window {window}");
        // A window shorter than the matched token cannot contain it.
        let expected: Vec<String> = if window >= 7 {
            vec!["harbour".to_owned()]
        } else {
            Vec::new()
        };
        assert_eq!(marks, expected, "window {window}");
    }
    let whole = run(store.handle, &lexical(b"harbour", false), text.len());
    let (_, _, truncated_start, truncated_end) = whole[0].2.clone().expect("snippet");
    assert!(truncated_start && !truncated_end);
}

#[test]
fn snippet_requests_are_validated_and_leave_both_outputs_zeroed() {
    let store = common::TestStore::new();
    ingest(store.handle, &[(1, [1.0, 0.0], Some("harbour"))]);
    let text = lexical(b"harbour", false);
    let refuse = |request: &ZeQueryRequest, snippet_bytes: usize| {
        let mut result: ZeQueryResult = common::sized_zeroed();
        let mut snippets: ZeQuerySnippets = common::sized_zeroed();
        snippets.snippet_count = 7;
        let code = ze_query_with_snippets(
            store.handle,
            request,
            snippet_bytes,
            &mut result,
            &mut snippets,
        );
        assert!(result.hits.is_null() && result.hit_count == 0);
        assert!(snippets.snippets.is_null() && snippets.snippet_count == 0);
        code
    };
    assert_eq!(refuse(&text, 0), ZeErrorCode::ZeErrInvalidArgument);
    let vector = [1.0_f32, 0.0];
    let vector_only = common::valid_query_request(&vector);
    assert_eq!(refuse(&vector_only, 64), ZeErrorCode::ZeErrInvalidArgument);
    let mut unknown_flag = text;
    unknown_flag.lexical_flags = 2;
    assert_eq!(refuse(&unknown_flag, 64), ZeErrorCode::ZeErrInvalidArgument);

    let mut result: ZeQueryResult = common::sized_zeroed();
    assert_eq!(
        ze_query_with_snippets(store.handle, &text, 64, &mut result, std::ptr::null_mut()),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let mut undersized: ZeQuerySnippets = common::sized_zeroed();
    undersized.abi_size = 4;
    assert_eq!(
        ze_query_with_snippets(store.handle, &text, 64, &mut result, &mut undersized),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let mut snippets: ZeQuerySnippets = common::sized_zeroed();
    assert_eq!(
        ze_query_with_snippets(store.handle, &text, 64, std::ptr::null_mut(), &mut snippets),
        ZeErrorCode::ZeErrInvalidArgument
    );
}

#[test]
fn snippets_leave_the_ranking_and_its_counters_unchanged() {
    let store = common::TestStore::new();
    ingest(
        store.handle,
        &[
            (1, [1.0, 0.0], Some("harbour lights at dusk")),
            (2, [0.9, 0.1], Some("the harbour master and the harbour")),
            (3, [0.1, 0.9], Some("quantized vectors")),
        ],
    );
    let vector = [1.0_f32, 0.0];
    let mut hybrid = common::valid_query_request(&vector);
    hybrid.text = b"harbour".as_ptr();
    hybrid.text_len = b"harbour".len();
    hybrid.k = 3;
    for request in [lexical(b"harbour", false), lexical(b"harb", true), hybrid] {
        let mut plain: ZeQueryResult = common::sized_zeroed();
        assert_eq!(
            ze_query(store.handle, &request, &mut plain),
            ZeErrorCode::ZeOk
        );
        let mut with: ZeQueryResult = common::sized_zeroed();
        let mut snippets: ZeQuerySnippets = common::sized_zeroed();
        assert_eq!(
            ze_query_with_snippets(store.handle, &request, 32, &mut with, &mut snippets),
            ZeErrorCode::ZeOk
        );
        let hits = |result: &ZeQueryResult| {
            (0..result.hit_count)
                .map(|index| {
                    let hit = unsafe { *result.hits.add(index) };
                    (
                        hit.doc_id.low,
                        hit.has_revision,
                        hit.revision,
                        hit.score.to_bits(),
                        hit.lexical_bm25.to_bits(),
                        hit.vector_squared_l2.to_bits(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert!(plain.hit_count > 0);
        assert_eq!(hits(&plain), hits(&with));
        assert_eq!(
            (
                plain.mode,
                plain.generation,
                plain.docs_evaluated,
                plain.postings_decoded
            ),
            (
                with.mode,
                with.generation,
                with.docs_evaluated,
                with.postings_decoded
            )
        );
        assert_eq!(ze_query_snippets_free(&mut snippets), ZeErrorCode::ZeOk);
        assert_eq!(ze_query_result_free(&mut with), ZeErrorCode::ZeOk);
        assert_eq!(ze_query_result_free(&mut plain), ZeErrorCode::ZeOk);
    }
}

#[test]
fn snippets_free_is_safe_twice_and_refuses_what_it_did_not_allocate() {
    let store = common::TestStore::new();
    ingest(store.handle, &[(1, [1.0, 0.0], Some("harbour"))]);
    let request = lexical(b"harbour", false);
    let mut result: ZeQueryResult = common::sized_zeroed();
    let mut snippets: ZeQuerySnippets = common::sized_zeroed();
    assert_eq!(
        ze_query_with_snippets(store.handle, &request, 64, &mut result, &mut snippets),
        ZeErrorCode::ZeOk
    );
    let allocation = snippets.snippets;
    let count = snippets.snippet_count;

    // A query-result free cannot release the snippet arena.
    let mut forged: ZeQueryResult = common::sized_zeroed();
    forged.hits = allocation.cast::<ZeQueryHit>();
    forged.hit_count = count;
    assert_eq!(
        ze_query_result_free(&mut forged),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(ze_query_snippets_free(&mut snippets), ZeErrorCode::ZeOk);
    assert!(snippets.snippets.is_null() && snippets.snippet_count == 0);
    assert_eq!(ze_query_snippets_free(&mut snippets), ZeErrorCode::ZeOk);

    let mut stale: ZeQuerySnippets = common::sized_zeroed();
    stale.snippets = allocation;
    stale.snippet_count = count;
    assert_eq!(
        ze_query_snippets_free(&mut stale),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let mut disagreeing: ZeQuerySnippets = common::sized_zeroed();
    disagreeing.snippet_count = 1;
    assert_eq!(
        ze_query_snippets_free(&mut disagreeing),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let mut zeroed = unsafe { std::mem::zeroed::<ZeQuerySnippets>() };
    assert_eq!(ze_query_snippets_free(&mut zeroed), ZeErrorCode::ZeOk);
    assert_eq!(
        ze_query_snippets_free(std::ptr::null_mut()),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(ze_query_result_free(&mut result), ZeErrorCode::ZeOk);
}

#[test]
fn source_ranges_are_absolute_for_both_query_paths() {
    let store = common::TestStore::new();
    let source = "前 🚀 café harbour café tail";
    ingest(store.handle, &[(1, [1.0, 0.0], Some(source))]);
    for filtered in [false, true] {
        let request = lexical(b"harbour cafe", false);
        let mut result: ZeQueryResult = common::sized_zeroed();
        let mut snippets: ZeQuerySnippets = common::sized_zeroed();
        let mut constraints: ZeQueryFilter = common::sized_zeroed();
        constraints.has_timestamp_range = 1;
        constraints.start_ts = 0;
        constraints.end_ts = 2;
        let status = if filtered {
            ze_query_filtered(
                store.handle,
                &request,
                &constraints,
                22,
                &mut result,
                &mut snippets,
            )
        } else {
            ze_query_with_snippets(store.handle, &request, 22, &mut result, &mut snippets)
        };
        assert_eq!(status, ZeErrorCode::ZeOk);
        let snippet = unsafe { *snippets.snippets };
        let mut ranges: ZeSnippetSourceRanges = common::sized_zeroed();
        assert_eq!(
            ze_query_snippet_source_ranges(&snippets, 0, &mut ranges),
            ZeErrorCode::ZeOk
        );
        assert_eq!(ranges.source_start, "前 🚀 ".len());
        assert_eq!(ranges.source_end, ranges.source_start + snippet.text_len);
        let excerpt = unsafe { std::slice::from_raw_parts(snippet.text, snippet.text_len) };
        assert_eq!(
            &source.as_bytes()[ranges.source_start..ranges.source_end],
            excerpt
        );
        assert_eq!(ranges.highlight_count, snippet.highlight_count);
        assert!(ranges.highlight_count >= 3);
        for index in 0..ranges.highlight_count {
            let absolute = unsafe { *ranges.highlights.add(index) };
            let relative = unsafe { *snippet.highlights.add(index) };
            assert_eq!(absolute.start, ranges.source_start + relative.start);
            assert_eq!(absolute.end, ranges.source_start + relative.end);
            assert_eq!(
                &source.as_bytes()[absolute.start..absolute.end],
                &excerpt[relative.start..relative.end]
            );
        }
        ranges = common::sized_zeroed();
        assert_eq!(
            ze_query_snippet_source_ranges(&snippets, 1, &mut ranges),
            ZeErrorCode::ZeErrInvalidArgument
        );
        let stale = snippets;
        assert_eq!(ze_query_snippets_free(&mut snippets), ZeErrorCode::ZeOk);
        assert_eq!(
            ze_query_snippet_source_ranges(&stale, 0, &mut ranges),
            ZeErrorCode::ZeErrInvalidArgument
        );
        assert_eq!(ze_query_result_free(&mut result), ZeErrorCode::ZeOk);
    }
}
