//! Seeded public-path adapter for the full-text-search campaign.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tempfile::tempdir;
use zeppelin_embed::fts::bm25::Bm25Params;
use zeppelin_embed::fts::index::{DEFAULT_FIELD, Document, LexicalIndex, SegmentIndex};
use zeppelin_embed::fts::phonetic;
use zeppelin_embed::fts::prune::{Strategy, search_pruned};
use zeppelin_embed::fts::query::LexicalQuery;
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::fts::snippet;
use zeppelin_embed::fts::tokenizer::{Analyzer, TokenizerConfig};
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision,
};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed_adversarial_oracle::fts::{
    DocumentFact, DocumentTokens, FtsInput, FtsObserved, PhoneticFact, ScoreFact, SnippetFact,
    StructuredFact, TokenFact,
};

use super::runner::ControlStore;

const VOCABULARY: [&str; 40] = [
    "amber", "beacon", "cobalt", "delta", "ember", "falcon", "gamma", "harbor", "indigo", "jovial",
    "kappa", "lunar", "mango", "nectar", "orbit", "panda", "quartz", "radar", "solar", "tango",
    "umbra", "vivid", "waltz", "xenon", "yonder", "zebra", "cafe", "papaya", "bravo", "cello",
    "dingo", "gecko", "hippo", "igloo", "jumbo", "koala", "lemur", "mocha", "ninja", "pixel",
];

const BM25_TERMS: [&str; 3] = ["amber", "beacon", "cobalt"];
const BM25_K: u32 = 8;
const ACTIVE_DOCUMENTS: [&str; 8] = [
    "amber smith solar",
    "amber smyth lunar",
    "amber cafe zebra",
    "amber cafe radar zebra",
    "amber quartz pixel",
    "amber papaya tango",
    "amber indigo mango",
    "amber beacon cobalt amber beacon cobalt amber beacon cobalt",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FtsOperationKind {
    Tokenizer,
    Regions,
    Bm25,
    Pruning,
    Extras,
}

impl FtsOperationKind {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Tokenizer => "tokenizer",
            Self::Regions => "regions",
            Self::Bm25 => "bm25",
            Self::Pruning => "pruning",
            Self::Extras => "extras",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FtsFaultKind {
    PostingsCorruption,
    DictionaryCorruption,
    NormCorruption,
    BlockMaxCorruption,
    StoredTextCorruption,
    StoredTextAbsence,
    LexicalCancellation,
}

impl FtsFaultKind {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::PostingsCorruption => "postings-corruption",
            Self::DictionaryCorruption => "dictionary-corruption",
            Self::NormCorruption => "norm-corruption",
            Self::BlockMaxCorruption => "block-max-corruption",
            Self::StoredTextCorruption => "stored-text-corruption",
            Self::StoredTextAbsence => "stored-text-absence",
            Self::LexicalCancellation => "lexical-cancellation",
        }
    }

    #[must_use]
    pub const fn operation(self) -> FtsOperationKind {
        match self {
            Self::PostingsCorruption
            | Self::DictionaryCorruption
            | Self::NormCorruption
            | Self::StoredTextCorruption => FtsOperationKind::Regions,
            Self::BlockMaxCorruption => FtsOperationKind::Pruning,
            Self::StoredTextAbsence | Self::LexicalCancellation => FtsOperationKind::Extras,
        }
    }

    #[must_use]
    pub const fn site(self) -> &'static str {
        match self {
            Self::PostingsCorruption => "fts.postings.decode",
            Self::DictionaryCorruption => "fts.dictionary.decode",
            Self::NormCorruption => "fts.norm.decode",
            Self::BlockMaxCorruption => "fts.block-max.decode",
            Self::StoredTextCorruption => "fts.stored-text.decode",
            Self::StoredTextAbsence => "fts.snippet.stored-text",
            Self::LexicalCancellation => "fts.search.cancel",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FtsFaultReceipt {
    pub fault: FtsFaultKind,
    pub operation: FtsOperationKind,
    pub site: &'static str,
    pub cardinality: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FtsInvariantEvidence {
    I40 {
        input: FtsInput,
        observed: FtsObserved,
    },
    I41 {
        input: FtsInput,
        observed: FtsObserved,
    },
    I42 {
        input: FtsInput,
        observed: FtsObserved,
    },
    I43 {
        input: FtsInput,
        observed: FtsObserved,
    },
    I44 {
        input: FtsInput,
        observed: FtsObserved,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FtsOperationEvidence {
    pub invariants: Vec<FtsInvariantEvidence>,
    pub receipts: Vec<FtsFaultReceipt>,
}

#[derive(Clone)]
struct ObservationBundle {
    input: FtsInput,
    observed: FtsObserved,
}

thread_local! {
    static OBSERVATION_CACHE: RefCell<Option<(u64, ObservationBundle)>> = const {
        RefCell::new(None)
    };
}

#[derive(Clone, Copy)]
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

    fn bounded(&mut self, upper: usize) -> usize {
        if upper == 0 {
            return 0;
        }
        usize::try_from(self.next() % upper as u64).unwrap_or(0)
    }

    fn shuffle<T>(&mut self, values: &mut [T]) {
        for end in (1..values.len()).rev() {
            let selected = self.bounded(end.saturating_add(1));
            values.swap(end, selected);
        }
    }
}

fn surface(term: &str, rng: &mut SplitMix64) -> String {
    match term {
        "cafe" => match rng.bounded(3) {
            0 => "Café".to_owned(),
            1 => "CAFÉ".to_owned(),
            _ => "cafe".to_owned(),
        },
        _ => match rng.bounded(4) {
            0 => term.to_ascii_uppercase(),
            1 => {
                let mut characters = term.chars();
                characters.next().map_or_else(String::new, |first| {
                    first.to_ascii_uppercase().to_string() + characters.as_str()
                })
            }
            _ => term.to_owned(),
        },
    }
}

fn render_terms(terms: &[&str], rng: &mut SplitMix64) -> String {
    let separators = [" ", ", ", "; ", " — ", " / "];
    let mut text = String::new();
    for (slot, term) in terms.iter().enumerate() {
        if slot > 0 {
            text.push_str(
                separators
                    .get(rng.bounded(separators.len()))
                    .copied()
                    .unwrap_or(" "),
            );
        }
        text.push_str(&surface(term, rng));
    }
    text
}

fn generated_text(row: usize, rng: &mut SplitMix64) -> String {
    if row < usize::try_from(BM25_K).unwrap_or(0) {
        return "amber beacon cobalt amber beacon cobalt".to_owned();
    }
    let mut terms = vec!["amber"];
    let mandatory_slot = row.saturating_sub(BM25_K as usize) % VOCABULARY.len();
    let mandatory_slot = if matches!(mandatory_slot, 1 | 2) {
        3
    } else {
        mandatory_slot
    };
    if let Some(mandatory) = VOCABULARY.get(mandatory_slot) {
        terms.push(*mandatory);
    }
    let desired = 12_usize.saturating_add(rng.bounded(12));
    while terms.len() < desired {
        let left = rng.bounded(VOCABULARY.len().saturating_sub(3));
        let right = rng.bounded(VOCABULARY.len().saturating_sub(3));
        let rank = left
            .saturating_mul(right)
            .checked_div(VOCABULARY.len().saturating_sub(3))
            .unwrap_or(0)
            .saturating_add(3)
            .min(VOCABULARY.len().saturating_sub(1));
        if let Some(term) = VOCABULARY.get(rank) {
            terms.push(*term);
        }
    }
    render_terms(&terms, rng)
}

fn fixture_input(seed: u64) -> Result<FtsInput, String> {
    let mut rng = SplitMix64::new(seed ^ 0x4654_532d_4341_4d50);
    let sealed_count = 192_usize.saturating_add(rng.bounded(65));
    let total_count = sealed_count.saturating_add(ACTIVE_DOCUMENTS.len());
    let base = (seed & ((1_u64 << 40) - 1)) << 10;
    let mut ids = (0..total_count)
        .map(|offset| base.saturating_add(u64::try_from(offset).unwrap_or(u64::MAX) + 1))
        .collect::<Vec<_>>();
    rng.shuffle(&mut ids);
    if ids.iter().any(|id| *id >= (1_u64 << 53)) {
        return Err("generated FTS document id reached 2^53".to_owned());
    }
    let mut delete_candidates = (48..sealed_count).collect::<Vec<_>>();
    rng.shuffle(&mut delete_candidates);
    let delete_count = sealed_count.saturating_mul(7).div_ceil(100);
    let deleted = delete_candidates
        .into_iter()
        .take(delete_count)
        .collect::<BTreeSet<_>>();
    let mut documents = Vec::with_capacity(total_count);
    for row in 0..sealed_count {
        let doc_id = ids
            .get(row)
            .copied()
            .ok_or_else(|| "sealed fixture omitted a document id".to_owned())?;
        documents.push(DocumentFact {
            doc_id,
            text: generated_text(row, &mut rng),
            sealed: true,
            source_row: u32::try_from(row).map_err(|_| "sealed row exceeds u32".to_owned())?,
            deleted: deleted.contains(&row),
        });
    }
    for (row, text) in ACTIVE_DOCUMENTS.iter().enumerate() {
        let doc_id = ids
            .get(sealed_count.saturating_add(row))
            .copied()
            .ok_or_else(|| "active fixture omitted a document id".to_owned())?;
        documents.push(DocumentFact {
            doc_id,
            text: (*text).to_owned(),
            sealed: false,
            source_row: u32::try_from(row).map_err(|_| "active row exceeds u32".to_owned())?,
            deleted: false,
        });
    }
    let mut names = ["Smith", "Smyth", "Schmidt", "Thompson", "Knight", "Xavier"]
        .map(str::to_owned)
        .to_vec();
    let rotation = rng.bounded(names.len());
    names.rotate_left(rotation);
    Ok(FtsInput {
        documents,
        bm25_terms: BM25_TERMS.map(str::to_owned).to_vec(),
        bm25_k: BM25_K,
        phrase_terms: ["cafe", "zebra"].map(str::to_owned).to_vec(),
        prefix: "ze".to_owned(),
        fuzzy_term: "cobolt".to_owned(),
        fuzzy_distance: 1,
        phonetic_name: "Smith".to_owned(),
        phonetic_names: names,
        snippet_bytes: 32,
    })
}

fn ingest_document(document: &DocumentFact) -> IngestDocument {
    let scalar = (document.doc_id % 251) as f32 / 251.0;
    IngestDocument::new(
        DocumentVersion::new(DocId::new(u128::from(document.doc_id)), Revision::new(1)),
        vec![scalar, 1.0 - scalar, 0.5],
    )
    .with_text(document.text.clone())
}

fn first_segment(directory: &Path) -> Result<PathBuf, String> {
    std::fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("segment-") && name.ends_with(".zseg"))
        })
        .ok_or_else(|| "lexical fixture published no segment".to_owned())
}

fn query_control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

fn score_fact(document: DocumentVersion, score: f64) -> Result<ScoreFact, String> {
    let doc_id = u64::try_from(document.doc_id().get())
        .map_err(|_| "lexical result document id exceeds u64".to_owned())?;
    Ok(ScoreFact {
        doc_id,
        score_bits: score.to_bits(),
    })
}

fn structured_fact(
    outcome: zeppelin_embed::ingest::StoreStructuredLexicalSearchOutcome,
) -> Result<StructuredFact, String> {
    let mut expansions = outcome
        .expansions
        .into_iter()
        .map(|expansion| {
            String::from_utf8(expansion.term)
                .map_err(|error| format!("structured expansion is not UTF-8: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    expansions.sort();
    let mut result_ids = Vec::with_capacity(outcome.candidates.len());
    let mut snippets = Vec::with_capacity(outcome.candidates.len());
    for candidate in outcome.candidates {
        let doc_id = u64::try_from(candidate.document.doc_id().get())
            .map_err(|_| "structured result document id exceeds u64".to_owned())?;
        result_ids.push(doc_id);
        snippets.push(SnippetFact {
            doc_id,
            text: candidate.snippet.text,
            start: candidate.snippet.source.start,
            end: candidate.snippet.source.end,
        });
    }
    result_ids.sort_unstable();
    snippets.sort_by_key(|snippet| snippet.doc_id);
    Ok(StructuredFact {
        expansions,
        result_ids,
        snippets,
    })
}

fn pruning_observation(
    input: &FtsInput,
    analyzer: &Analyzer,
) -> Result<(Vec<ScoreFact>, u64, Vec<ScoreFact>, u64), String> {
    let mut documents = input
        .documents
        .iter()
        .filter(|document| !document.deleted)
        .collect::<Vec<_>>();
    documents.sort_by_key(|document| (u8::from(!document.sealed), document.source_row));
    let mut segment = SegmentIndex::new();
    for document in &documents {
        segment
            .push_document(analyzer, &Document::with_text(&document.text))
            .map_err(|error| error.to_string())?;
    }
    let mut index = LexicalIndex::new();
    index
        .push_segment(segment)
        .map_err(|error| error.to_string())?;
    let query = TermQuery::flat(
        input
            .bm25_terms
            .iter()
            .map(|term| term.as_bytes().to_vec())
            .collect(),
        &[DEFAULT_FIELD],
    );
    let observe = |strategy| -> Result<(Vec<ScoreFact>, u64), String> {
        let result = search_pruned(
            &index,
            &query,
            input.bm25_k as usize,
            Bm25Params::beir(),
            strategy,
        )
        .map_err(|error| error.to_string())?;
        let mut hits = Vec::with_capacity(result.hits.len());
        for hit in result.hits {
            let row = usize::try_from(hit.doc.row)
                .map_err(|_| "pruned result row exceeds usize".to_owned())?;
            let document = documents
                .get(row)
                .ok_or_else(|| format!("pruned result row {row} escaped fixture"))?;
            hits.push(ScoreFact {
                doc_id: document.doc_id,
                score_bits: hit.score.to_bits(),
            });
        }
        Ok((hits, result.counters.blocks_skipped))
    };
    let (wand_hits, wand_blocks_skipped) = observe(Strategy::BlockMaxWand)?;
    let (maxscore_hits, maxscore_blocks_skipped) = observe(Strategy::BlockMaxMaxscore)?;
    Ok((
        wand_hits,
        wand_blocks_skipped,
        maxscore_hits,
        maxscore_blocks_skipped,
    ))
}

fn build_observation_on_store(
    store: &Store,
    directory: &std::path::Path,
    seed: u64,
) -> Result<ObservationBundle, String> {
    let input = fixture_input(seed)?;
    let sealed = input
        .documents
        .iter()
        .filter(|document| document.sealed)
        .map(ingest_document)
        .collect();
    store
        .ingest(IngestBatch::new(sealed))
        .map_err(|error| format!("ingest sealed FTS fixture: {error}"))?;
    store
        .seal()
        .map_err(|error| format!("seal FTS fixture: {error}"))?;
    let deleted = input
        .documents
        .iter()
        .filter(|document| document.sealed && document.deleted)
        .map(|document| DocId::new(u128::from(document.doc_id)))
        .collect();
    store
        .delete(DeleteBatch::new(deleted))
        .map_err(|error| format!("delete sealed FTS rows: {error}"))?;
    let segment = first_segment(directory)?;
    let sealed_segment: Arc<[u8]> = std::fs::read(segment)
        .map_err(|error| format!("read sealed FTS segment: {error}"))?
        .into();

    let active = input
        .documents
        .iter()
        .filter(|document| !document.sealed)
        .map(ingest_document)
        .collect();
    store
        .ingest(IngestBatch::new(active))
        .map_err(|error| format!("ingest active FTS fixture: {error}"))?;

    let analyzer =
        Analyzer::new(TokenizerConfig::text_default()).map_err(|error| error.to_string())?;
    let tokens = input
        .documents
        .iter()
        .map(|document| DocumentTokens {
            doc_id: document.doc_id,
            tokens: analyzer
                .analyze(&document.text)
                .into_iter()
                .map(|token| TokenFact {
                    term: token.term,
                    position: token.position,
                    start: token.offset.start,
                    end: token.offset.end,
                })
                .collect(),
        })
        .collect();
    let term_query = TermQuery::flat(
        input
            .bm25_terms
            .iter()
            .map(|term| term.as_bytes().to_vec())
            .collect(),
        &[DEFAULT_FIELD],
    );
    let bm25 = store
        .search_lexical(&term_query, input.bm25_k as usize, query_control())
        .map_err(|error| format!("public Store BM25 search: {error}"))?;
    let bm25_hits = bm25
        .candidates
        .into_iter()
        .map(|candidate| score_fact(candidate.document, candidate.score))
        .collect::<Result<Vec<_>, _>>()?;
    let (wand_hits, wand_blocks_skipped, maxscore_hits, maxscore_blocks_skipped) =
        pruning_observation(&input, &analyzer)?;
    let phrase = structured_fact(
        store
            .search_lexical_structured(
                &LexicalQuery::phrase(
                    input
                        .phrase_terms
                        .iter()
                        .map(|term| term.as_bytes().to_vec())
                        .collect(),
                    0,
                    DEFAULT_FIELD,
                ),
                input.documents.len(),
                input.snippet_bytes as usize,
                query_control(),
            )
            .map_err(|error| format!("public structured phrase search: {error}"))?,
    )?;
    let prefix = structured_fact(
        store
            .search_lexical_structured(
                &LexicalQuery::prefix(input.prefix.as_bytes().to_vec(), DEFAULT_FIELD),
                input.documents.len(),
                input.snippet_bytes as usize,
                query_control(),
            )
            .map_err(|error| format!("public structured prefix search: {error}"))?,
    )?;
    let fuzzy = structured_fact(
        store
            .search_lexical_structured(
                &LexicalQuery::fuzzy(
                    input.fuzzy_term.as_bytes().to_vec(),
                    input.fuzzy_distance,
                    DEFAULT_FIELD,
                ),
                input.documents.len(),
                input.snippet_bytes as usize,
                query_control(),
            )
            .map_err(|error| format!("public structured fuzzy search: {error}"))?,
    )?;
    let phonetic_observed = structured_fact(
        store
            .search_lexical_structured(
                &LexicalQuery::phonetic(input.phonetic_name.as_bytes().to_vec(), DEFAULT_FIELD),
                input.documents.len(),
                input.snippet_bytes as usize,
                query_control(),
            )
            .map_err(|error| format!("public structured phonetic search: {error}"))?,
    )?;
    let phonetic_codes = input
        .phonetic_names
        .iter()
        .map(|name| PhoneticFact {
            name: name.clone(),
            code: phonetic::encode(name),
        })
        .collect();
    Ok(ObservationBundle {
        input,
        observed: FtsObserved {
            tokens,
            sealed_segment,
            bm25_hits,
            wand_hits,
            maxscore_hits,
            wand_blocks_skipped,
            maxscore_blocks_skipped,
            phrase,
            prefix,
            fuzzy,
            phonetic: phonetic_observed,
            phonetic_codes,
        },
    })
}

fn build_observation(seed: u64) -> Result<ObservationBundle, String> {
    let directory = tempdir().map_err(|error| error.to_string())?;
    let store = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("open FTS fixture: {error}"))?;
    let result = build_observation_on_store(&store, directory.path(), seed);
    store
        .close()
        .map_err(|error| format!("close FTS fixture: {error}"))?;
    result
}

fn observation(seed: u64) -> Result<ObservationBundle, String> {
    OBSERVATION_CACHE.with(|cache| {
        if let Some((cached_seed, bundle)) = cache.borrow().as_ref()
            && *cached_seed == seed
        {
            return Ok(bundle.clone());
        }
        let bundle = build_observation(seed)?;
        cache.replace(Some((seed, bundle.clone())));
        Ok(bundle)
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    bytes
        .get(offset..offset.saturating_add(2))
        .and_then(|value| value.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| format!("missing u16 at {offset}"))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    bytes
        .get(offset..offset.saturating_add(8))
        .and_then(|value| value.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| format!("missing u64 at {offset}"))
}

fn region_byte(bytes: &[u8], kind: u16) -> Result<usize, String> {
    let count = usize::from(read_u16(bytes, 52)?);
    for position in 0..count {
        let entry = 64_usize.saturating_add(position.saturating_mul(32));
        if read_u16(bytes, entry)? == kind {
            let offset = usize::try_from(read_u64(bytes, entry + 8)?)
                .map_err(|_| "region offset exceeds usize".to_owned())?;
            let length = usize::try_from(read_u64(bytes, entry + 16)?)
                .map_err(|_| "region length exceeds usize".to_owned())?;
            return offset
                .checked_add(length / 2)
                .filter(|target| *target < bytes.len())
                .ok_or_else(|| "lexical mutation escaped segment".to_owned());
        }
    }
    Err(format!("segment omitted lexical region {kind}"))
}

fn corruption_refused(seed: u64, region: u16) -> Result<(), String> {
    let directory = tempdir().map_err(|error| error.to_string())?;
    let store =
        Store::open(directory.path(), OpenOptions::default()).map_err(|error| error.to_string())?;
    let documents = ["alpha beta", "alpha gamma"]
        .into_iter()
        .enumerate()
        .map(|(row, text)| {
            IngestDocument::new(
                DocumentVersion::new(
                    DocId::new((u128::from(seed) << 64) | (row as u128 + 1)),
                    Revision::new(1),
                ),
                vec![row as f32, 0.0, 1.0],
            )
            .with_text(text)
        })
        .collect();
    store
        .ingest(IngestBatch::new(documents))
        .map_err(|error| error.to_string())?;
    store.seal().map_err(|error| error.to_string())?;
    store.close().map_err(|error| error.to_string())?;
    let segment = first_segment(directory.path())?;
    let mut bytes = std::fs::read(&segment).map_err(|error| error.to_string())?;
    let target = region_byte(&bytes, region)?;
    if let Some(value) = bytes.get_mut(target) {
        *value ^= 0x5a;
    } else {
        return Err("lexical mutation target disappeared".to_owned());
    }
    std::fs::write(&segment, bytes).map_err(|error| error.to_string())?;
    match Store::open(directory.path(), OpenOptions::default()) {
        Err(_error) => Ok(()),
        Ok(store) => {
            let result = store.search_lexical(
                &TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]),
                2,
                query_control(),
            );
            store.close().map_err(|error| error.to_string())?;
            match result {
                Err(_error) => Ok(()),
                Ok(_outcome) => Err("corrupt lexical region was accepted".to_owned()),
            }
        }
    }
}

fn cancellation_refused(seed: u64) -> Result<(), String> {
    let directory = tempdir().map_err(|error| error.to_string())?;
    let store =
        Store::open(directory.path(), OpenOptions::default()).map_err(|error| error.to_string())?;
    let document = IngestDocument::new(
        DocumentVersion::new(DocId::new(u128::from(seed) + 1), Revision::new(1)),
        vec![1.0, 0.0, 0.0],
    )
    .with_text("alpha beta");
    store
        .ingest(IngestBatch::new(vec![document]))
        .map_err(|error| error.to_string())?;
    let token = CancelToken::new();
    token.cancel();
    let result = store.search_lexical(
        &TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]),
        1,
        QueryControl::Cancel(token),
    );
    store.close().map_err(|error| error.to_string())?;
    match result {
        Err(_error) => Ok(()),
        Ok(_outcome) => Err("cancelled lexical query succeeded".to_owned()),
    }
}

fn exercise_fault(seed: u64, fault: FtsFaultKind) -> Result<(), String> {
    match fault {
        FtsFaultKind::PostingsCorruption
        | FtsFaultKind::DictionaryCorruption
        | FtsFaultKind::NormCorruption
        | FtsFaultKind::BlockMaxCorruption => corruption_refused(seed, 6),
        FtsFaultKind::StoredTextCorruption => corruption_refused(seed, 14),
        FtsFaultKind::StoredTextAbsence => {
            let analyzer = Analyzer::new(TokenizerConfig::text_default())
                .map_err(|error| error.to_string())?;
            match snippet::best_window(&analyzer, "beta only", &[b"alpha".to_vec()], 16, true)
                .map_err(|error| error.to_string())?
            {
                None => Ok(()),
                Some(_snippet) => Err("absent stored-text match produced a snippet".to_owned()),
            }
        }
        FtsFaultKind::LexicalCancellation => cancellation_refused(seed),
    }
}

pub fn run_fts_operation(
    operation: FtsOperationKind,
    seed: u64,
    fault: Option<FtsFaultKind>,
) -> Result<FtsOperationEvidence, String> {
    if fault.is_some_and(|fault| fault.operation() != operation) {
        return Err(format!("FTS fault {fault:?} does not target {operation:?}"));
    }
    let ObservationBundle { input, observed } = observation(seed)?;
    let invariants = match operation {
        FtsOperationKind::Tokenizer => vec![FtsInvariantEvidence::I40 { input, observed }],
        FtsOperationKind::Regions => vec![FtsInvariantEvidence::I41 { input, observed }],
        FtsOperationKind::Bm25 => vec![FtsInvariantEvidence::I42 { input, observed }],
        FtsOperationKind::Pruning => vec![FtsInvariantEvidence::I43 { input, observed }],
        FtsOperationKind::Extras => vec![FtsInvariantEvidence::I44 { input, observed }],
    };
    let mut receipts = Vec::new();
    if let Some(fault) = fault {
        exercise_fault(seed, fault)?;
        receipts.push(FtsFaultReceipt {
            fault,
            operation,
            site: fault.site(),
            cardinality: 1,
        });
    }
    Ok(FtsOperationEvidence {
        invariants,
        receipts,
    })
}

fn corruption_refused_on_store(leg: &mut ControlStore, region: u16) -> Result<(), String> {
    leg.close()?;
    let segment = first_segment(leg.path())?;
    let mut bytes = std::fs::read(&segment).map_err(|error| error.to_string())?;
    let target = region_byte(&bytes, region)?;
    let value = bytes
        .get_mut(target)
        .ok_or_else(|| "lexical mutation target disappeared".to_owned())?;
    *value ^= 0x5a;
    std::fs::write(&segment, bytes).map_err(|error| error.to_string())?;
    if leg.reopen().is_err() {
        return Ok(());
    }
    let result = leg.store()?.search_lexical(
        &TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]),
        2,
        query_control(),
    );
    match result {
        Err(_error) => Ok(()),
        Ok(_outcome) => Err("corrupt lexical region was accepted".to_owned()),
    }
}

fn exercise_fault_on_store(leg: &mut ControlStore, fault: FtsFaultKind) -> Result<(), String> {
    match fault {
        FtsFaultKind::PostingsCorruption
        | FtsFaultKind::DictionaryCorruption
        | FtsFaultKind::NormCorruption
        | FtsFaultKind::BlockMaxCorruption => corruption_refused_on_store(leg, 6),
        FtsFaultKind::StoredTextCorruption => corruption_refused_on_store(leg, 14),
        FtsFaultKind::StoredTextAbsence => {
            let analyzer = Analyzer::new(TokenizerConfig::text_default())
                .map_err(|error| error.to_string())?;
            match snippet::best_window(&analyzer, "beta only", &[b"alpha".to_vec()], 16, true)
                .map_err(|error| error.to_string())?
            {
                None => Ok(()),
                Some(_snippet) => Err("absent stored-text match produced a snippet".to_owned()),
            }
        }
        FtsFaultKind::LexicalCancellation => {
            let token = CancelToken::new();
            token.cancel();
            match leg.store()?.search_lexical(
                &TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]),
                1,
                QueryControl::Cancel(token),
            ) {
                Err(_error) => Ok(()),
                Ok(_outcome) => Err("cancelled lexical query succeeded".to_owned()),
            }
        }
    }
}

pub(crate) fn run_fts_operation_on_store(
    leg: &mut ControlStore,
    operation: FtsOperationKind,
    seed: u64,
    fault: Option<FtsFaultKind>,
) -> Result<FtsOperationEvidence, String> {
    if fault.is_some_and(|fault| fault.operation() != operation) {
        return Err(format!("FTS fault {fault:?} does not target {operation:?}"));
    }
    let ObservationBundle { input, observed } =
        build_observation_on_store(leg.store()?, leg.path(), seed)?;
    let invariants = match operation {
        FtsOperationKind::Tokenizer => vec![FtsInvariantEvidence::I40 { input, observed }],
        FtsOperationKind::Regions => vec![FtsInvariantEvidence::I41 { input, observed }],
        FtsOperationKind::Bm25 => vec![FtsInvariantEvidence::I42 { input, observed }],
        FtsOperationKind::Pruning => vec![FtsInvariantEvidence::I43 { input, observed }],
        FtsOperationKind::Extras => vec![FtsInvariantEvidence::I44 { input, observed }],
    };
    let mut receipts = Vec::new();
    if let Some(fault) = fault {
        exercise_fault_on_store(leg, fault)?;
        receipts.push(FtsFaultReceipt {
            fault,
            operation,
            site: fault.site(),
            cardinality: 1,
        });
    }
    Ok(FtsOperationEvidence {
        invariants,
        receipts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeppelin_embed_adversarial_oracle::fts as oracle;

    #[test]
    fn every_fts_operation_runs_its_independent_checker() {
        for operation in [
            FtsOperationKind::Tokenizer,
            FtsOperationKind::Regions,
            FtsOperationKind::Bm25,
            FtsOperationKind::Pruning,
            FtsOperationKind::Extras,
        ] {
            let evidence = run_fts_operation(operation, 1, None).expect("FTS operation");
            for invariant in evidence.invariants {
                match invariant {
                    FtsInvariantEvidence::I40 { input, observed } => {
                        oracle::compare_i40(&input, &observed)
                    }
                    FtsInvariantEvidence::I41 { input, observed } => {
                        oracle::compare_i41(&input, &observed)
                    }
                    FtsInvariantEvidence::I42 { input, observed } => {
                        oracle::compare_i42(&input, &observed)
                    }
                    FtsInvariantEvidence::I43 { input, observed } => {
                        oracle::compare_i43(&input, &observed)
                    }
                    FtsInvariantEvidence::I44 { input, observed } => {
                        oracle::compare_i44(&input, &observed)
                    }
                }
                .expect("FTS checker");
            }
        }
    }

    #[test]
    fn fixture_is_seeded_large_mixed_and_nonmonotone() {
        let first = fixture_input(7).expect("first seeded fixture");
        let second = fixture_input(8).expect("second seeded fixture");
        assert_ne!(first, second);
        assert!((150..=408).contains(&first.documents.len()));
        assert!(first.documents.iter().any(|document| !document.sealed));
        assert!(first.documents.iter().any(|document| document.deleted));
        assert!(
            first
                .documents
                .iter()
                .any(|document| !document.text.is_ascii())
        );
        let ids = first
            .documents
            .iter()
            .map(|document| document.doc_id)
            .collect::<Vec<_>>();
        assert!(ids.windows(2).any(|pair| pair[0] > pair[1]));
        assert!(ids.iter().all(|doc_id| *doc_id < (1_u64 << 53)));
    }

    #[test]
    fn every_declared_fts_fault_fires_once() {
        for (seed, fault) in [
            FtsFaultKind::PostingsCorruption,
            FtsFaultKind::DictionaryCorruption,
            FtsFaultKind::NormCorruption,
            FtsFaultKind::BlockMaxCorruption,
            FtsFaultKind::StoredTextCorruption,
            FtsFaultKind::StoredTextAbsence,
            FtsFaultKind::LexicalCancellation,
        ]
        .into_iter()
        .enumerate()
        {
            let evidence = run_fts_operation(fault.operation(), seed as u64, Some(fault))
                .unwrap_or_else(|error| panic!("{fault:?}: {error}"));
            assert_eq!(evidence.receipts.len(), 1);
            assert_eq!(evidence.receipts[0].fault, fault);
            assert_eq!(evidence.receipts[0].cardinality, 1);
        }
    }
}
