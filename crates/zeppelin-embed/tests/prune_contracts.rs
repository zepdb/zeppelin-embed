//! Deterministic counter contracts for task 14.
//!
//! # Why counters and not a clock
//!
//! Wall-clock is evidence; counters are the gate. `docs_evaluated`,
//! `blocks_skipped`, `blocks_decoded`, and `postings_decoded` are exact
//! functions of the corpus, the query, and the strategy, so they gate with
//! a zero flake budget on any machine. A latency number measured on a
//! shared machine gates nothing.
//!
//! # Format: `key = value`, not TOML
//!
//! The task spec asks for TOML contracts. No TOML crate is in the
//! dependency budget, and hand-writing a TOML parser is exactly the kind of
//! incidental complexity this repository refuses. The contracts are flat
//! `key = value` text parsed by [`parse_contract`] below, which is nine
//! lines. If literal TOML ever matters, that is a recorded dev-dependency
//! decision for the owner, not a silent addition.
//!
//! # Re-baselining
//!
//! A contract file is a claim about behaviour. When a legitimate change
//! moves a counter, update the file **in the same commit** with the reason
//! in the message. Never widen a bound to make a red test green: a counter
//! that drifted without an explanation is a regression that has not been
//! diagnosed yet.
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zeppelin_embed::fts::bm25::Bm25Params;
use zeppelin_embed::fts::index::{DEFAULT_FIELD, Document, LexicalIndex, SegmentIndex};
use zeppelin_embed::fts::prune::{Strategy, search_pruned, select_strategy};
use zeppelin_embed::fts::search::{SearchCounters, TermQuery, search};
use zeppelin_embed::fts::tokenizer::{Analyzer, Profile};

fn contract_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/prune_contracts")
}

/// Parses a flat `key = value` contract file.
///
/// Blank lines and `#` comments are ignored. A malformed line is an error,
/// never a skipped assertion: a typo must not silently disable a gate.
fn parse_contract(text: &str) -> BTreeMap<String, u64> {
    let mut values = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            panic!("contract line {} is not `key = value`: {line:?}", index + 1);
        };
        let parsed = value
            .trim()
            .parse::<u64>()
            .unwrap_or_else(|_| panic!("contract line {} has a non-integer value", index + 1));
        values.insert(key.trim().to_owned(), parsed);
    }
    values
}

fn load_contract(name: &str) -> BTreeMap<String, u64> {
    let path = contract_dir().join(format!("{name}.contract"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("missing contract {}: {error}", path.display()));
    parse_contract(&text)
}

fn check(name: &str, counters: SearchCounters) {
    let contract = load_contract(name);
    let actual: BTreeMap<&str, u64> = [
        ("docs_evaluated", counters.docs_evaluated),
        ("blocks_decoded", counters.blocks_decoded),
        ("blocks_skipped", counters.blocks_skipped),
        ("postings_decoded", counters.postings_decoded),
    ]
    .into_iter()
    .collect();

    for (key, expected) in &contract {
        // A key suffixed `_max` is an upper bound; a bare key is exact.
        let (metric, bounded) = key
            .strip_suffix("_max")
            .map_or((key.as_str(), false), |base| (base, true));
        let Some(measured) = actual.get(metric).copied() else {
            panic!("contract {name} names an unknown counter {metric:?}");
        };
        if bounded {
            assert!(
                measured <= *expected,
                "contract {name}: {metric} was {measured}, above the contracted maximum {expected}"
            );
        } else {
            assert_eq!(
                measured, *expected,
                "contract {name}: {metric} was {measured}, contracted {expected}. \
                 If this change is legitimate, update the contract file in the \
                 SAME commit and say why; never widen it to reach green."
            );
        }
    }
    assert!(!contract.is_empty(), "contract {name} asserted nothing");
}

fn analyzer() -> Analyzer {
    Analyzer::new(Profile::Code.config()).expect("valid config")
}

fn index_of(texts: &[String]) -> LexicalIndex {
    let analyzer = analyzer();
    let mut segment = SegmentIndex::new();
    for text in texts {
        segment
            .push_document(&analyzer, &Document::with_text(text))
            .expect("indexable");
    }
    let mut index = LexicalIndex::new();
    index.push_segment(segment);
    index
}

/// A deterministic Zipf-shaped corpus.
///
/// Term `t{i}` appears in roughly `n / (i + 1)` documents, so bounds differ
/// sharply between terms — which is the condition under which pruning has
/// anything to skip. A uniform corpus would make every contract trivial.
fn zipf_corpus(documents: usize, terms: usize) -> Vec<String> {
    (0..documents)
        .map(|doc| {
            let mut words: Vec<String> = Vec::new();
            for term in 0..terms {
                let period = term + 1;
                if doc % period == 0 {
                    words.push(format!("t{term}"));
                }
            }
            if words.is_empty() {
                words.push(String::from("filler"));
            }
            words.join(" ")
        })
        .collect()
}

#[test]
fn short_list_query_decodes_zero_blocks() {
    let texts: Vec<String> = (0..20).map(|index| format!("alpha body{index}")).collect();
    let index = index_of(&texts);
    let query = TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]);
    let result = search_pruned(
        &index,
        &query,
        10,
        Bm25Params::default(),
        Strategy::BlockMaxWand,
    )
    .expect("scores");
    check("short_list_fast_path", result.counters);
}

#[test]
fn two_term_zipf_query_at_k10_evaluates_at_most_the_contracted_fraction() {
    let texts = zipf_corpus(2_000, 8);
    let index = index_of(&texts);
    let query = TermQuery::flat(vec![b"t0".to_vec(), b"t5".to_vec()], &[DEFAULT_FIELD]);
    let params = Bm25Params::default();

    let exhaustive = search(&index, &query, 10, params).expect("scores");
    let pruned = search_pruned(&index, &query, 10, params, Strategy::BlockMaxWand).expect("scores");

    // Results must be identical; the contract is only about cost.
    assert_eq!(pruned.hits, exhaustive.hits, "pruning changed the answer");
    check("zipf_two_term_k10_wand", pruned.counters);

    // The whole point: pruning evaluates strictly fewer documents.
    assert!(
        pruned.counters.docs_evaluated < exhaustive.counters.docs_evaluated,
        "pruning evaluated {} documents against exhaustive {}: no skipping happened",
        pruned.counters.docs_evaluated,
        exhaustive.counters.docs_evaluated
    );
}

#[test]
fn maxscore_on_a_long_query_also_evaluates_fewer_documents() {
    let texts = zipf_corpus(2_000, 8);
    let index = index_of(&texts);
    let terms: Vec<Vec<u8>> = (0..6)
        .map(|index| format!("t{index}").into_bytes())
        .collect();
    let query = TermQuery::flat(terms, &[DEFAULT_FIELD]);
    let params = Bm25Params::default();

    let exhaustive = search(&index, &query, 10, params).expect("scores");
    let pruned =
        search_pruned(&index, &query, 10, params, Strategy::BlockMaxMaxscore).expect("scores");
    assert_eq!(pruned.hits, exhaustive.hits, "pruning changed the answer");
    check("zipf_six_term_k10_maxscore", pruned.counters);
    assert!(
        pruned.counters.docs_evaluated <= exhaustive.counters.docs_evaluated,
        "maxscore evaluated more documents than exhaustive"
    );
}

#[test]
fn strategy_selection_follows_the_recorded_rule() {
    // The rule of record: WAND when terms <= 4 and k <= 10, else MAXSCORE.
    assert_eq!(select_strategy(1, 1), Strategy::BlockMaxWand);
    assert_eq!(select_strategy(4, 10), Strategy::BlockMaxWand);
    assert_eq!(select_strategy(5, 10), Strategy::BlockMaxMaxscore);
    assert_eq!(select_strategy(4, 11), Strategy::BlockMaxMaxscore);

    let contract = load_contract("strategy_rule");
    assert_eq!(contract.get("max_terms_for_wand").copied(), Some(4));
    assert_eq!(contract.get("max_k_for_wand").copied(), Some(10));
}

#[test]
fn the_exhaustive_path_never_skips_a_block() {
    let texts = zipf_corpus(500, 4);
    let index = index_of(&texts);
    let query = TermQuery::flat(vec![b"t0".to_vec()], &[DEFAULT_FIELD]);
    let result = search(&index, &query, 10, Bm25Params::default()).expect("scores");
    assert_eq!(
        result.counters.blocks_skipped, 0,
        "the oracle must never skip; it is the thing pruning is compared against"
    );
}

#[test]
fn a_malformed_contract_line_fails_loudly_rather_than_being_skipped() {
    let parsed = parse_contract("# comment\n\ndocs_evaluated = 12\nblocks_skipped=3\n");
    assert_eq!(parsed.get("docs_evaluated").copied(), Some(12));
    assert_eq!(parsed.get("blocks_skipped").copied(), Some(3));

    let malformed = std::panic::catch_unwind(|| parse_contract("docs_evaluated 12\n"));
    assert!(malformed.is_err(), "a line without `=` must fail loudly");

    let non_integer = std::panic::catch_unwind(|| parse_contract("docs_evaluated = many\n"));
    assert!(non_integer.is_err(), "a non-integer value must fail loudly");
}

/// Writes the contract files from the current behaviour. Not a gate.
///
/// ```text
/// cargo test -p zeppelin-embed --test prune_contracts \
///     capture_contracts -- --ignored
/// ```
///
/// Capture proposes; a human approves by committing. Read every changed
/// number and say in the commit message why it moved.
#[test]
#[ignore = "capture mode; a human approves the numbers by committing them"]
fn capture_contracts() {
    let dir = contract_dir();
    std::fs::create_dir_all(&dir).expect("contract directory");

    let write = |name: &str, body: String| {
        std::fs::write(dir.join(format!("{name}.contract")), body)
            .unwrap_or_else(|error| panic!("cannot write contract {name}: {error}"));
    };

    let render = |header: &str, counters: SearchCounters| -> String {
        format!(
            "# {header}\n\
             docs_evaluated = {}\n\
             blocks_decoded = {}\n\
             blocks_skipped = {}\n\
             postings_decoded = {}\n",
            counters.docs_evaluated,
            counters.blocks_decoded,
            counters.blocks_skipped,
            counters.postings_decoded
        )
    };

    let texts: Vec<String> = (0..20).map(|index| format!("alpha body{index}")).collect();
    let index = index_of(&texts);
    let query = TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]);
    let result = search_pruned(
        &index,
        &query,
        10,
        Bm25Params::default(),
        Strategy::BlockMaxWand,
    )
    .expect("scores");
    write(
        "short_list_fast_path",
        render(
            "20 documents, one term: below one block, so no pruning machinery runs",
            result.counters,
        ),
    );

    let texts = zipf_corpus(2_000, 8);
    let index = index_of(&texts);
    let query = TermQuery::flat(vec![b"t0".to_vec(), b"t5".to_vec()], &[DEFAULT_FIELD]);
    let result = search_pruned(
        &index,
        &query,
        10,
        Bm25Params::default(),
        Strategy::BlockMaxWand,
    )
    .expect("scores");
    write(
        "zipf_two_term_k10_wand",
        render(
            "Zipf 2,000 documents, terms t0 and t5, k=10, block-max WAND",
            result.counters,
        ),
    );

    let terms: Vec<Vec<u8>> = (0..6)
        .map(|index| format!("t{index}").into_bytes())
        .collect();
    let query = TermQuery::flat(terms, &[DEFAULT_FIELD]);
    let result = search_pruned(
        &index,
        &query,
        10,
        Bm25Params::default(),
        Strategy::BlockMaxMaxscore,
    )
    .expect("scores");
    write(
        "zipf_six_term_k10_maxscore",
        render(
            "Zipf 2,000 documents, terms t0..t5, k=10, block-max MAXSCORE",
            result.counters,
        ),
    );

    write(
        "strategy_rule",
        String::from(
            "# The v1 selection rule of record. Recalibrate from counter\n\
             # evidence and record the change; do not tune it silently.\n\
             max_terms_for_wand = 4\n\
             max_k_for_wand = 10\n",
        ),
    );
}
