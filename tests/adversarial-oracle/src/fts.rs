//! Independent, std-only full-text-search campaign oracle.
//!
//! The generated corpus deliberately uses a documented subset of the text
//! analyzer: simple alphanumeric spans separated by punctuation, ASCII case
//! folding, and precomposed `é`/`ï` search folding. It contains no stopwords,
//! compounds, number words, or Porter suffixes. Keeping that subset small is
//! what makes this implementation independent and reviewable.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub const I40_CHECKER_ID: &str = "fts.i40.tokenizer.v2";
pub const I41_CHECKER_ID: &str = "fts.i41.regions.v2";
pub const I42_CHECKER_ID: &str = "fts.i42.bm25.v2";
pub const I43_CHECKER_ID: &str = "fts.i43.pruning.v2";
pub const I44_CHECKER_ID: &str = "fts.i44.extras.v2";

pub const POSTINGS_PER_BLOCK: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TokenFact {
    pub term: String,
    pub position: u32,
    pub start: u32,
    pub end: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentFact {
    pub doc_id: u64,
    pub text: String,
    pub sealed: bool,
    pub source_row: u32,
    pub deleted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentTokens {
    pub doc_id: u64,
    pub tokens: Vec<TokenFact>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScoreFact {
    pub doc_id: u64,
    pub score_bits: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnippetFact {
    pub doc_id: u64,
    pub text: String,
    pub start: u32,
    pub end: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StructuredFact {
    pub expansions: Vec<String>,
    pub result_ids: Vec<u64>,
    pub snippets: Vec<SnippetFact>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhoneticFact {
    pub name: String,
    pub code: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FtsInput {
    pub documents: Vec<DocumentFact>,
    pub bm25_terms: Vec<String>,
    pub bm25_k: u32,
    pub phrase_terms: Vec<String>,
    pub prefix: String,
    pub fuzzy_term: String,
    pub fuzzy_distance: u32,
    pub phonetic_name: String,
    pub phonetic_names: Vec<String>,
    pub snippet_bytes: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FtsObserved {
    pub tokens: Vec<DocumentTokens>,
    pub sealed_segment: Arc<[u8]>,
    pub bm25_hits: Vec<ScoreFact>,
    pub wand_hits: Vec<ScoreFact>,
    pub maxscore_hits: Vec<ScoreFact>,
    pub wand_blocks_skipped: u64,
    pub maxscore_blocks_skipped: u64,
    pub phrase: StructuredFact,
    pub prefix: StructuredFact,
    pub fuzzy: StructuredFact,
    pub phonetic: StructuredFact,
    pub phonetic_codes: Vec<PhoneticFact>,
}

fn fail(checker: &str, detail: impl std::fmt::Display) -> Result<(), String> {
    Err(format!("{checker}: {detail}"))
}

fn fold_char(value: char) -> Result<char, String> {
    match value {
        'é' | 'É' => Ok('e'),
        'ï' | 'Ï' => Ok('i'),
        value if value.is_ascii_alphanumeric() => Ok(value.to_ascii_lowercase()),
        _ => Err(format!(
            "fixture left the independently specified tokenizer subset at {value:?}"
        )),
    }
}

/// Tokenizes the campaign's intentionally small text-profile subset.
pub fn tokenize(text: &str) -> Result<Vec<TokenFact>, String> {
    let mut tokens = Vec::new();
    let mut start = None;
    let mut end = 0_usize;
    for (offset, value) in text.char_indices() {
        if value.is_alphanumeric() {
            start.get_or_insert(offset);
            end = offset.saturating_add(value.len_utf8());
        } else if let Some(open) = start.take() {
            tokens.push(token(text, open, end, tokens.len())?);
        }
    }
    if let Some(open) = start {
        tokens.push(token(text, open, end, tokens.len())?);
    }
    Ok(tokens)
}

fn token(text: &str, start: usize, end: usize, position: usize) -> Result<TokenFact, String> {
    let surface = text
        .get(start..end)
        .ok_or_else(|| "oracle token offsets are not UTF-8 boundaries".to_owned())?;
    let term = surface
        .chars()
        .map(fold_char)
        .collect::<Result<String, _>>()?;
    Ok(TokenFact {
        term,
        position: u32::try_from(position).map_err(|_| "token position exceeds u32".to_owned())?,
        start: u32::try_from(start).map_err(|_| "token start exceeds u32".to_owned())?,
        end: u32::try_from(end).map_err(|_| "token end exceeds u32".to_owned())?,
    })
}

fn ordered_documents(input: &FtsInput, live_only: bool) -> Vec<&DocumentFact> {
    let mut documents = input
        .documents
        .iter()
        .filter(|document| !live_only || !document.deleted)
        .collect::<Vec<_>>();
    documents.sort_by_key(|document| (u8::from(!document.sealed), document.source_row));
    documents
}

fn analyzed_documents(
    input: &FtsInput,
    live_only: bool,
) -> Result<Vec<(&DocumentFact, Vec<TokenFact>)>, String> {
    ordered_documents(input, live_only)
        .into_iter()
        .map(|document| Ok((document, tokenize(&document.text)?)))
        .collect()
}

fn term_frequency(tokens: &[TokenFact], term: &str) -> u32 {
    u32::try_from(tokens.iter().filter(|token| token.term == term).count()).unwrap_or(u32::MAX)
}

fn idf(df: u32, document_count: u64) -> f64 {
    let count = document_count as f64;
    let frequency = f64::from(df).min(count);
    (1.0 + (count - frequency + 0.5) / (frequency + 0.5)).ln()
}

fn term_score(tf: u32, df: u32, length: u32, document_count: u64, average: f64) -> f64 {
    if tf == 0 {
        return 0.0;
    }
    let frequency = f64::from(tf);
    let normalization = 1.0 - 0.75 + 0.75 * f64::from(length) / average;
    let denominator = frequency + 1.2 * normalization;
    idf(df, document_count) * (frequency * (1.2 + 1.0)) / denominator
}

pub fn exhaustive_scores(input: &FtsInput) -> Result<Vec<ScoreFact>, String> {
    let analyzed = analyzed_documents(input, true)?;
    if analyzed.len() < 150 {
        return Err(format!(
            "live corpus has {}, expected at least 150 documents",
            analyzed.len()
        ));
    }
    let document_count =
        u64::try_from(analyzed.len()).map_err(|_| "live document count exceeds u64".to_owned())?;
    let total_tokens = analyzed.iter().try_fold(0_u64, |total, (_, tokens)| {
        let count = u64::try_from(tokens.len()).map_err(|_| "token count exceeds u64")?;
        total.checked_add(count).ok_or("total token count overflow")
    })?;
    if total_tokens == 0 {
        return Err("live corpus has no analyzed tokens".to_owned());
    }
    let average = total_tokens as f64 / document_count as f64;
    let frequencies = input
        .bm25_terms
        .iter()
        .map(|term| {
            u32::try_from(
                analyzed
                    .iter()
                    .filter(|(_, tokens)| term_frequency(tokens, term) > 0)
                    .count(),
            )
            .unwrap_or(u32::MAX)
        })
        .collect::<Vec<_>>();
    let mut scored = Vec::<(&DocumentFact, f64)>::new();
    for (document, tokens) in &analyzed {
        let length = u32::try_from(tokens.len()).unwrap_or(u32::MAX);
        let mut score = 0.0_f64;
        let mut matched = false;
        for (slot, term) in input.bm25_terms.iter().enumerate() {
            let tf = term_frequency(tokens, term);
            if tf > 0 {
                matched = true;
            }
            let df = frequencies.get(slot).copied().unwrap_or(0);
            score += term_score(tf, df, length, document_count, average);
        }
        if matched {
            scored.push((document, score));
        }
    }
    scored.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                (u8::from(!left.0.sealed), left.0.source_row)
                    .cmp(&(u8::from(!right.0.sealed), right.0.source_row))
            })
    });
    scored.truncate(usize::try_from(input.bm25_k).unwrap_or(usize::MAX));
    Ok(scored
        .into_iter()
        .map(|(document, score)| ScoreFact {
            doc_id: document.doc_id,
            score_bits: score.to_bits(),
        })
        .collect())
}

fn has_score_tie(hits: &[ScoreFact]) -> bool {
    hits.iter().enumerate().any(|(slot, hit)| {
        hits.iter()
            .skip(slot.saturating_add(1))
            .any(|other| other.score_bits == hit.score_bits)
    })
}

fn independently_certifies_skippable_block(input: &FtsInput) -> Result<(bool, f64, f64), String> {
    let analyzed = analyzed_documents(input, true)?;
    let expected = exhaustive_scores(input)?;
    let threshold = expected
        .last()
        .map(|hit| f64::from_bits(hit.score_bits))
        .ok_or_else(|| "pruning oracle produced no top-k threshold".to_owned())?;
    let document_count =
        u64::try_from(analyzed.len()).map_err(|_| "live document count exceeds u64".to_owned())?;
    let total_tokens = analyzed.iter().try_fold(0_u64, |total, (_, tokens)| {
        let count = u64::try_from(tokens.len()).map_err(|_| "token count exceeds u64")?;
        total.checked_add(count).ok_or("total token count overflow")
    })?;
    let average = total_tokens as f64 / document_count as f64;
    let frequencies = input
        .bm25_terms
        .iter()
        .map(|term| {
            u32::try_from(
                analyzed
                    .iter()
                    .filter(|(_, tokens)| term_frequency(tokens, term) > 0)
                    .count(),
            )
            .unwrap_or(u32::MAX)
        })
        .collect::<Vec<_>>();
    let Some(driver) = input.bm25_terms.first() else {
        return Ok((false, threshold, f64::INFINITY));
    };
    let postings = analyzed
        .iter()
        .enumerate()
        .filter_map(|(row, (_, tokens))| (term_frequency(tokens, driver) > 0).then_some(row))
        .collect::<Vec<_>>();
    if postings.len() <= POSTINGS_PER_BLOCK.saturating_mul(2) {
        return Ok((false, threshold, f64::INFINITY));
    }
    let mut smallest_bound = f64::INFINITY;
    for block in postings.chunks(POSTINGS_PER_BLOCK).skip(1) {
        let (Some(first), Some(last)) = (block.first().copied(), block.last().copied()) else {
            continue;
        };
        let mut bound = 0.0_f64;
        for (slot, term) in input.bm25_terms.iter().enumerate() {
            let mut max_tf = 0_u32;
            let mut min_len = u32::MAX;
            for (_, tokens) in analyzed.iter().take(last.saturating_add(1)).skip(first) {
                let tf = term_frequency(tokens, term);
                if tf > 0 {
                    max_tf = max_tf.max(tf);
                    min_len = min_len.min(u32::try_from(tokens.len()).unwrap_or(u32::MAX));
                }
            }
            if max_tf > 0 {
                bound += term_score(
                    max_tf,
                    frequencies.get(slot).copied().unwrap_or(0),
                    min_len,
                    document_count,
                    average,
                );
            }
        }
        smallest_bound = smallest_bound.min(bound);
        if bound < threshold {
            return Ok((true, threshold, bound));
        }
    }
    Ok((false, threshold, smallest_bound))
}

#[derive(Debug, Eq, PartialEq)]
struct ParsedRegions {
    row_count: u32,
    postings_per_block: u16,
    dictionary: BTreeMap<String, u32>,
    lengths: Vec<u32>,
    stored_text: Vec<Option<String>>,
    maximum_block_count: u32,
}

struct ParsedPostings {
    row_count: u32,
    postings_per_block: u16,
    dictionary: BTreeMap<String, u32>,
    lengths: Vec<u32>,
    maximum_block_count: u32,
}

fn take(bytes: &[u8], offset: usize, length: usize) -> Result<&[u8], String> {
    let end = offset
        .checked_add(length)
        .ok_or_else(|| "byte range overflow".to_owned())?;
    bytes
        .get(offset..end)
        .ok_or_else(|| format!("missing bytes {offset}..{end}"))
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    take(bytes, offset, 2)?
        .try_into()
        .map(u16::from_le_bytes)
        .map_err(|_| format!("missing u16 at {offset}"))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    take(bytes, offset, 4)?
        .try_into()
        .map(u32::from_le_bytes)
        .map_err(|_| format!("missing u32 at {offset}"))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    take(bytes, offset, 8)?
        .try_into()
        .map(u64::from_le_bytes)
        .map_err(|_| format!("missing u64 at {offset}"))
}

fn region(bytes: &[u8], kind: u16) -> Result<&[u8], String> {
    if take(bytes, 0, 8)? != b"ZEPEMBED" {
        return Err("segment file magic differs".to_owned());
    }
    let file_length = usize::try_from(read_u64(bytes, 24)?)
        .map_err(|_| "segment file length exceeds usize".to_owned())?;
    if file_length != bytes.len() {
        return Err(format!(
            "segment file length {file_length} differs from {}",
            bytes.len()
        ));
    }
    let header_length = usize::try_from(read_u64(bytes, 16)?)
        .map_err(|_| "segment header length exceeds usize".to_owned())?;
    let region_count = usize::from(read_u16(bytes, 52)?);
    let implied = 64_usize
        .checked_add(region_count.saturating_mul(32))
        .and_then(|value| value.checked_add(8))
        .ok_or_else(|| "segment directory length overflow".to_owned())?;
    if header_length != implied {
        return Err(format!(
            "segment header {header_length} differs from directory geometry {implied}"
        ));
    }
    for slot in 0..region_count {
        let entry = 64_usize.saturating_add(slot.saturating_mul(32));
        if read_u16(bytes, entry)? == kind {
            let offset = usize::try_from(read_u64(bytes, entry + 8)?)
                .map_err(|_| "region offset exceeds usize".to_owned())?;
            let length = usize::try_from(read_u64(bytes, entry + 16)?)
                .map_err(|_| "region length exceeds usize".to_owned())?;
            return take(bytes, offset, length);
        }
    }
    Err(format!("segment omitted region {kind}"))
}

fn parse_postings(bytes: &[u8]) -> Result<ParsedPostings, String> {
    if take(bytes, 0, 4)? != b"ZFTS" {
        return Err("postings region magic differs".to_owned());
    }
    if read_u16(bytes, 4)? != 1 {
        return Err("postings region version differs".to_owned());
    }
    let per_block = read_u16(bytes, 6)?;
    let row_count = read_u32(bytes, 8)?;
    let span_count =
        usize::try_from(read_u32(bytes, 12)?).map_err(|_| "span count exceeds usize".to_owned())?;
    let field_count = usize::try_from(read_u32(bytes, 16)?)
        .map_err(|_| "field count exceeds usize".to_owned())?;
    if read_u32(bytes, 20)? != 0 {
        return Err("postings header reserved word is nonzero".to_owned());
    }
    let terms_len =
        usize::try_from(read_u64(bytes, 24)?).map_err(|_| "term bytes exceed usize".to_owned())?;
    let blob_len = usize::try_from(read_u64(bytes, 32)?)
        .map_err(|_| "posting blob exceeds usize".to_owned())?;
    let fields_start = 40_usize.saturating_add(span_count.saturating_mul(48));
    let mut cursor = fields_start;
    let mut lengths = Vec::new();
    for field_slot in 0..field_count {
        let field = read_u16(bytes, cursor)?;
        let declared_rows = read_u32(bytes, cursor + 4)?;
        if read_u16(bytes, cursor + 2)? != 0 || declared_rows != row_count {
            return Err("postings field header differs".to_owned());
        }
        cursor = cursor.saturating_add(8);
        let mut field_lengths = Vec::new();
        for _ in 0..row_count {
            field_lengths.push(read_u32(bytes, cursor)?);
            cursor = cursor.saturating_add(4);
        }
        if field_slot == 0 && field == 0 {
            lengths = field_lengths;
        }
    }
    if field_count != 1 || lengths.len() != row_count as usize {
        return Err(format!(
            "campaign expected one default field, decoded {field_count} fields and {} lengths",
            lengths.len()
        ));
    }
    let terms = take(bytes, cursor, terms_len)?;
    cursor = cursor.saturating_add(terms_len);
    take(bytes, cursor, blob_len)?;
    cursor = cursor.saturating_add(blob_len);
    if cursor != bytes.len() {
        return Err(format!(
            "postings parser consumed {cursor} of {} bytes",
            bytes.len()
        ));
    }
    let mut dictionary = BTreeMap::new();
    let mut maximum_block_count = 0_u32;
    for slot in 0..span_count {
        let span = 40_usize.saturating_add(slot.saturating_mul(48));
        if read_u16(bytes, span)? != 0 || read_u16(bytes, span + 2)? != 0 {
            return Err("campaign postings span is not the default field".to_owned());
        }
        let start = usize::try_from(read_u32(bytes, span + 4)?)
            .map_err(|_| "term start exceeds usize".to_owned())?;
        let length = usize::try_from(read_u32(bytes, span + 8)?)
            .map_err(|_| "term length exceeds usize".to_owned())?;
        let term = std::str::from_utf8(take(terms, start, length)?)
            .map_err(|_| "dictionary term is not UTF-8".to_owned())?
            .to_owned();
        let block_count = read_u32(bytes, span + 24)?;
        let document_frequency = read_u32(bytes, span + 40)?;
        if read_u32(bytes, span + 44)? != 1 {
            return Err("campaign term did not have exactly one field".to_owned());
        }
        maximum_block_count = maximum_block_count.max(block_count);
        if dictionary.insert(term, document_frequency).is_some() {
            return Err("duplicate dictionary term".to_owned());
        }
    }
    Ok(ParsedPostings {
        row_count,
        postings_per_block: per_block,
        dictionary,
        lengths,
        maximum_block_count,
    })
}

fn parse_stored_text(bytes: &[u8], expected_rows: u32) -> Result<Vec<Option<String>>, String> {
    let rows = read_u32(bytes, 0)?;
    if rows != expected_rows || read_u32(bytes, 4)? != 0 {
        return Err(format!(
            "stored-text rows/reserved {rows}/{}, expected {expected_rows}/0",
            read_u32(bytes, 4)?
        ));
    }
    let row_count = usize::try_from(rows).map_err(|_| "stored rows exceed usize".to_owned())?;
    let bitmap_len = row_count.div_ceil(8);
    let bitmap = take(bytes, 8, bitmap_len)?;
    let offsets_start = 8_usize.saturating_add(bitmap_len);
    let payload_start = offsets_start.saturating_add(row_count.saturating_mul(8));
    let payload = bytes
        .get(payload_start..)
        .ok_or_else(|| "stored-text payload is truncated".to_owned())?;
    let mut previous = 0_usize;
    let mut values = Vec::with_capacity(row_count);
    for row in 0..row_count {
        let end = usize::try_from(read_u64(bytes, offsets_start + row.saturating_mul(8))?)
            .map_err(|_| "stored-text offset exceeds usize".to_owned())?;
        if end < previous || end > payload.len() {
            return Err(format!(
                "stored-text row {row} has invalid range {previous}..{end}"
            ));
        }
        let present = bitmap
            .get(row / 8)
            .is_some_and(|byte| byte & (1 << (row % 8)) != 0);
        if present {
            values.push(Some(
                std::str::from_utf8(take(payload, previous, end - previous)?)
                    .map_err(|_| format!("stored-text row {row} is not UTF-8"))?
                    .to_owned(),
            ));
        } else {
            if end != previous {
                return Err(format!("absent stored-text row {row} owns bytes"));
            }
            values.push(None);
        }
        previous = end;
    }
    if previous != payload.len() {
        return Err("stored-text final offset differs from payload".to_owned());
    }
    Ok(values)
}

fn parse_regions(segment: &[u8]) -> Result<ParsedRegions, String> {
    let postings = region(segment, 6)?;
    let stored = region(segment, 14)?;
    let parsed = parse_postings(postings)?;
    let stored_text = parse_stored_text(stored, parsed.row_count)?;
    Ok(ParsedRegions {
        row_count: parsed.row_count,
        postings_per_block: parsed.postings_per_block,
        dictionary: parsed.dictionary,
        lengths: parsed.lengths,
        stored_text,
        maximum_block_count: parsed.maximum_block_count,
    })
}

fn expected_sealed_regions(input: &FtsInput) -> Result<ParsedRegions, String> {
    let documents = ordered_documents(input, false)
        .into_iter()
        .filter(|document| document.sealed)
        .collect::<Vec<_>>();
    let row_count = u32::try_from(documents.len())
        .map_err(|_| "sealed document count exceeds u32".to_owned())?;
    let mut dictionary = BTreeMap::<String, u32>::new();
    let mut lengths = Vec::with_capacity(documents.len());
    let mut stored_text = Vec::with_capacity(documents.len());
    for document in documents {
        let tokens = tokenize(&document.text)?;
        lengths.push(u32::try_from(tokens.len()).unwrap_or(u32::MAX));
        let terms = tokens
            .iter()
            .map(|token| token.term.clone())
            .collect::<BTreeSet<_>>();
        for term in terms {
            let frequency = dictionary.entry(term).or_default();
            *frequency = frequency.saturating_add(1);
        }
        stored_text.push(Some(document.text.clone()));
    }
    let maximum_frequency = dictionary.values().copied().max().unwrap_or(0);
    Ok(ParsedRegions {
        row_count,
        postings_per_block: POSTINGS_PER_BLOCK as u16,
        dictionary,
        lengths,
        stored_text,
        maximum_block_count: maximum_frequency.div_ceil(POSTINGS_PER_BLOCK as u32),
    })
}

fn vocabulary(input: &FtsInput) -> Result<BTreeSet<String>, String> {
    let mut vocabulary = BTreeSet::new();
    for (_, tokens) in analyzed_documents(input, true)? {
        vocabulary.extend(tokens.into_iter().map(|token| token.term));
    }
    Ok(vocabulary)
}

fn edit_distance(left: &[u8], right: &[u8]) -> u32 {
    let mut previous = (0..=right.len())
        .map(|value| u32::try_from(value).unwrap_or(u32::MAX))
        .collect::<Vec<_>>();
    for (row, left_byte) in left.iter().enumerate() {
        let mut current = Vec::with_capacity(right.len().saturating_add(1));
        current.push(u32::try_from(row.saturating_add(1)).unwrap_or(u32::MAX));
        for (column, right_byte) in right.iter().enumerate() {
            let insertion = current
                .get(column)
                .copied()
                .unwrap_or(u32::MAX)
                .saturating_add(1);
            let deletion = previous
                .get(column.saturating_add(1))
                .copied()
                .unwrap_or(u32::MAX)
                .saturating_add(1);
            let substitution = previous
                .get(column)
                .copied()
                .unwrap_or(u32::MAX)
                .saturating_add(u32::from(left_byte != right_byte));
            current.push(insertion.min(deletion).min(substitution));
        }
        previous = current;
    }
    previous.last().copied().unwrap_or(u32::MAX)
}

fn phrase_matches(tokens: &[TokenFact], terms: &[String]) -> bool {
    let Some(first) = terms.first() else {
        return false;
    };
    tokens
        .iter()
        .filter(|token| token.term == *first)
        .any(|start| {
            terms.iter().enumerate().all(|(offset, term)| {
                let position = start
                    .position
                    .saturating_add(u32::try_from(offset).unwrap_or(u32::MAX));
                tokens
                    .iter()
                    .any(|token| token.position == position && token.term == *term)
            })
        })
}

fn expected_structured(
    input: &FtsInput,
    expansions: Vec<String>,
    phrase: bool,
) -> Result<StructuredFact, String> {
    let mut result_ids = Vec::new();
    let mut snippets = Vec::new();
    for (document, tokens) in analyzed_documents(input, true)? {
        let matches = if phrase {
            phrase_matches(&tokens, &input.phrase_terms)
        } else {
            tokens
                .iter()
                .any(|token| expansions.binary_search(&token.term).is_ok())
        };
        if !matches {
            continue;
        }
        let terms = if phrase {
            input.phrase_terms.clone()
        } else {
            tokens
                .iter()
                .filter(|token| expansions.binary_search(&token.term).is_ok())
                .map(|token| token.term.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        };
        result_ids.push(document.doc_id);
        snippets.push(best_window(
            document.doc_id,
            &document.text,
            &tokens,
            &terms,
            usize::try_from(input.snippet_bytes).unwrap_or(usize::MAX),
        )?);
    }
    result_ids.sort_unstable();
    snippets.sort_by_key(|snippet| snippet.doc_id);
    Ok(StructuredFact {
        expansions,
        result_ids,
        snippets,
    })
}

fn best_window(
    doc_id: u64,
    text: &str,
    tokens: &[TokenFact],
    terms: &[String],
    window_bytes: usize,
) -> Result<SnippetFact, String> {
    let mut ranges = tokens
        .iter()
        .filter(|token| terms.contains(&token.term))
        .map(|token| (token.start, token.end))
        .collect::<Vec<_>>();
    ranges.sort_unstable();
    ranges.dedup();
    if ranges.is_empty() {
        return Err(format!(
            "snippet oracle found no match for document {doc_id}"
        ));
    }
    let mut best = None::<(usize, u32, u32)>;
    for (start, _) in &ranges {
        let start_usize = usize::try_from(*start).map_err(|_| "snippet start exceeds usize")?;
        let mut end = start_usize.saturating_add(window_bytes).min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end = end.saturating_add(1);
        }
        let end_u32 = u32::try_from(end).map_err(|_| "snippet end exceeds u32")?;
        let covered = ranges
            .iter()
            .filter(|(candidate_start, candidate_end)| {
                *candidate_start >= *start && *candidate_end <= end_u32
            })
            .count();
        if best.is_none_or(|(best_count, best_start, _)| {
            covered > best_count || covered == best_count && *start < best_start
        }) {
            best = Some((covered, *start, end_u32));
        }
    }
    let (_, start, end) = best.ok_or_else(|| "snippet oracle omitted a window".to_owned())?;
    let snippet = text
        .get(
            usize::try_from(start).map_err(|_| "snippet start exceeds usize")?
                ..usize::try_from(end).map_err(|_| "snippet end exceeds usize")?,
        )
        .ok_or_else(|| "snippet window is not UTF-8 aligned".to_owned())?;
    Ok(SnippetFact {
        doc_id,
        text: snippet.to_owned(),
        start,
        end,
    })
}

fn expected_phrase(input: &FtsInput) -> Result<StructuredFact, String> {
    let mut expansions = input.phrase_terms.clone();
    expansions.sort();
    expected_structured(input, expansions, true)
}

fn expected_prefix(input: &FtsInput) -> Result<StructuredFact, String> {
    let expansions = vocabulary(input)?
        .into_iter()
        .filter(|term| term.starts_with(&input.prefix))
        .collect();
    expected_structured(input, expansions, false)
}

fn expected_fuzzy(input: &FtsInput) -> Result<StructuredFact, String> {
    let expansions = vocabulary(input)?
        .into_iter()
        .filter(|term| {
            edit_distance(input.fuzzy_term.as_bytes(), term.as_bytes()) <= input.fuzzy_distance
        })
        .collect();
    expected_structured(input, expansions, false)
}

fn expected_phonetic(input: &FtsInput) -> Result<StructuredFact, String> {
    let code = phonetic_code(&input.phonetic_name);
    let expansions = vocabulary(input)?
        .into_iter()
        .filter(|term| phonetic_code(term) == code)
        .collect();
    expected_structured(input, expansions, false)
}

fn is_vowel(value: u8) -> bool {
    matches!(value, b'A' | b'E' | b'I' | b'O' | b'U' | b'Y')
}

fn at(word: &[u8], index: usize) -> u8 {
    word.get(index).copied().unwrap_or(0)
}

fn starts_with_at(word: &[u8], index: usize, text: &str) -> bool {
    word.get(index..index.saturating_add(text.len()))
        .is_some_and(|slice| slice == text.as_bytes())
}

/// Independent implementation of the product's declared primary
/// Double-Metaphone subset.
pub fn phonetic_code(term: &str) -> String {
    let word = term
        .chars()
        .filter(char::is_ascii_alphabetic)
        .map(|value| value.to_ascii_uppercase() as u8)
        .collect::<Vec<_>>();
    if word.is_empty() {
        return String::new();
    }
    let mut code = String::with_capacity(4);
    let mut index = 0_usize;
    if matches!(
        word.get(..2),
        Some(b"GN" | b"KN" | b"PN" | b"WR" | b"PS" | b"AE")
    ) {
        index = 1;
    }
    if at(&word, 0) == b'X' {
        code.push('S');
        index = 1;
    }
    while index < word.len() && code.len() < 4 {
        let current = at(&word, index);
        let next = at(&word, index.saturating_add(1));
        let mut step = 1_usize;
        match current {
            value if is_vowel(value) => {
                if index == 0 {
                    code.push('A');
                }
            }
            b'B' => {
                code.push('P');
                step += usize::from(next == b'B');
            }
            b'C' => {
                if starts_with_at(&word, index, "CIA") {
                    code.push('X');
                    step = 3;
                } else if starts_with_at(&word, index, "CH") {
                    code.push(
                        if starts_with_at(&word, index, "CHR")
                            || index == 0 && starts_with_at(&word, index, "CHA")
                        {
                            'K'
                        } else {
                            'X'
                        },
                    );
                    step = 2;
                } else if starts_with_at(&word, index, "CK") {
                    code.push('K');
                    step = 2;
                } else if matches!(next, b'I' | b'E' | b'Y') {
                    code.push('S');
                    step = 2;
                } else {
                    code.push('K');
                    step += usize::from(next == b'C');
                }
            }
            b'D' => {
                if starts_with_at(&word, index, "DG") {
                    if matches!(at(&word, index.saturating_add(2)), b'I' | b'E' | b'Y') {
                        code.push('J');
                        step = 3;
                    } else {
                        code.push('T');
                        step = 2;
                    }
                } else {
                    code.push('T');
                    step += usize::from(matches!(next, b'D' | b'T'));
                }
            }
            b'F' => {
                code.push('F');
                step += usize::from(next == b'F');
            }
            b'G' => {
                if next == b'H' {
                    if index == 0 || !is_vowel(at(&word, index.saturating_sub(1))) {
                        code.push('K');
                    }
                    step = 2;
                } else if next == b'N' {
                    step = 2;
                } else if matches!(next, b'I' | b'E' | b'Y') {
                    code.push('J');
                    step = 2;
                } else {
                    code.push('K');
                    step += usize::from(next == b'G');
                }
            }
            b'H' => {
                if (index == 0 || is_vowel(at(&word, index.saturating_sub(1)))) && is_vowel(next) {
                    code.push('H');
                }
            }
            b'J' => {
                code.push('J');
                step += usize::from(next == b'J');
            }
            b'K' => {
                code.push('K');
                step += usize::from(next == b'K');
            }
            b'L' => {
                code.push('L');
                step += usize::from(next == b'L');
            }
            b'M' => {
                code.push('M');
                step += usize::from(next == b'M');
            }
            b'N' => {
                code.push('N');
                step += usize::from(next == b'N');
            }
            b'P' => {
                if next == b'H' {
                    code.push('F');
                    step = 2;
                } else {
                    code.push('P');
                    step += usize::from(matches!(next, b'P' | b'B'));
                }
            }
            b'Q' => {
                code.push('K');
                step += usize::from(next == b'Q');
            }
            b'R' => {
                code.push('R');
                step += usize::from(next == b'R');
            }
            b'S' => {
                if starts_with_at(&word, index, "SCH") {
                    code.push('X');
                    step = 3;
                } else if starts_with_at(&word, index, "SH") {
                    code.push('X');
                    step = 2;
                } else if starts_with_at(&word, index, "SIO") || starts_with_at(&word, index, "SIA")
                {
                    code.push('X');
                    step = 3;
                } else {
                    code.push('S');
                    step += usize::from(next == b'S');
                }
            }
            b'T' => {
                if starts_with_at(&word, index, "TIO") || starts_with_at(&word, index, "TIA") {
                    code.push('X');
                    step = 3;
                } else if starts_with_at(&word, index, "TH") {
                    code.push(
                        if starts_with_at(&word, index.saturating_add(2), "OM")
                            || starts_with_at(&word, index.saturating_add(2), "AM")
                        {
                            'T'
                        } else {
                            '0'
                        },
                    );
                    step = 2;
                } else {
                    code.push('T');
                    step += usize::from(matches!(next, b'T' | b'D'));
                }
            }
            b'V' => {
                code.push('F');
                step += usize::from(next == b'V');
            }
            b'W' => {
                if is_vowel(next) {
                    code.push('W');
                }
            }
            b'X' => {
                code.push('K');
                if code.len() < 4 {
                    code.push('S');
                }
                step += usize::from(next == b'X');
            }
            b'Z' => {
                code.push('S');
                step += usize::from(next == b'Z');
            }
            _ => {}
        }
        index = index.saturating_add(step);
    }
    code.truncate(4);
    code
}

pub fn compare_i40(input: &FtsInput, observed: &FtsObserved) -> Result<(), String> {
    let expected = input
        .documents
        .iter()
        .map(|document| {
            Ok(DocumentTokens {
                doc_id: document.doc_id,
                tokens: tokenize(&document.text)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    if expected != observed.tokens {
        let mismatch = expected
            .iter()
            .zip(&observed.tokens)
            .find(|(expected, observed)| expected != observed)
            .map_or_else(
                || {
                    format!(
                        "document counts expected={} observed={}",
                        expected.len(),
                        observed.tokens.len()
                    )
                },
                |(expected, observed)| {
                    format!(
                        "doc_id={} expected={:?} observed={:?}",
                        expected.doc_id, expected.tokens, observed.tokens
                    )
                },
            );
        return fail(
            I40_CHECKER_ID,
            format!("full seeded-corpus term/position/UTF-8 offset stream differs: {mismatch}"),
        );
    }
    if expected.len() < 150 || expected.iter().any(|document| document.tokens.is_empty()) {
        return fail(I40_CHECKER_ID, "tokenizer minimum cardinality guard failed");
    }
    Ok(())
}

pub fn compare_i41(input: &FtsInput, observed: &FtsObserved) -> Result<(), String> {
    let expected = expected_sealed_regions(input)?;
    let parsed = parse_regions(&observed.sealed_segment)
        .map_err(|detail| format!("{I41_CHECKER_ID}: {detail}"))?;
    if parsed != expected {
        return fail(
            I41_CHECKER_ID,
            format!("sealed region facts differ: expected {expected:?}, parsed {parsed:?}"),
        );
    }
    if parsed.dictionary.len() < 40
        || parsed.maximum_block_count < 3
        || parsed.postings_per_block != POSTINGS_PER_BLOCK as u16
    {
        return fail(I41_CHECKER_ID, "region minimum cardinality guard failed");
    }
    Ok(())
}

pub fn compare_i42(input: &FtsInput, observed: &FtsObserved) -> Result<(), String> {
    let expected = exhaustive_scores(input)?;
    if expected.is_empty() || expected.len() != usize::try_from(input.bm25_k).unwrap_or(usize::MAX)
    {
        return fail(
            I42_CHECKER_ID,
            "BM25 top-k minimum cardinality guard failed",
        );
    }
    if !has_score_tie(&expected) {
        return fail(
            I42_CHECKER_ID,
            "seeded BM25 top-k contains no exact score tie",
        );
    }
    if observed.bm25_hits != expected {
        return fail(
            I42_CHECKER_ID,
            format!(
                "public Store BM25 ids/order/f64 score bits differ: expected {expected:?}, observed {:?}",
                observed.bm25_hits
            ),
        );
    }
    Ok(())
}

pub fn compare_i43(input: &FtsInput, observed: &FtsObserved) -> Result<(), String> {
    let expected = exhaustive_scores(input)?;
    if observed.wand_hits != expected {
        return fail(
            I43_CHECKER_ID,
            "BlockMaxWand differs from independent exhaustive top-k",
        );
    }
    if observed.maxscore_hits != expected {
        return fail(
            I43_CHECKER_ID,
            "MaxScore differs from independent exhaustive top-k",
        );
    }
    if observed.wand_blocks_skipped == 0 || observed.maxscore_blocks_skipped == 0 {
        return fail(
            I43_CHECKER_ID,
            format!(
                "pruning did not fire: wand={} maxscore={}",
                observed.wand_blocks_skipped, observed.maxscore_blocks_skipped
            ),
        );
    }
    let (certified, threshold, smallest_bound) = independently_certifies_skippable_block(input)?;
    if !certified {
        return fail(
            I43_CHECKER_ID,
            format!(
                "oracle found no block whose impact bound is below top-k: threshold={threshold} smallest_bound={smallest_bound}"
            ),
        );
    }
    Ok(())
}

pub fn compare_i44(input: &FtsInput, observed: &FtsObserved) -> Result<(), String> {
    let expected = [
        ("phrase", expected_phrase(input)?, &observed.phrase),
        ("prefix", expected_prefix(input)?, &observed.prefix),
        ("fuzzy", expected_fuzzy(input)?, &observed.fuzzy),
        ("phonetic", expected_phonetic(input)?, &observed.phonetic),
    ];
    for (kind, expected, actual) in expected {
        if expected.expansions.is_empty() || expected.result_ids.is_empty() {
            return fail(
                I44_CHECKER_ID,
                format!("{kind} minimum cardinality guard failed"),
            );
        }
        if actual != &expected {
            return fail(
                I44_CHECKER_ID,
                format!("public structured {kind} results/expansions/snippets differ"),
            );
        }
    }
    let expected_codes = input
        .phonetic_names
        .iter()
        .map(|name| PhoneticFact {
            name: name.clone(),
            code: phonetic_code(name),
        })
        .collect::<Vec<_>>();
    if observed.phonetic_codes != expected_codes {
        return fail(I44_CHECKER_ID, "independent Double-Metaphone codes differ");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restricted_tokenizer_pins_case_folding_and_utf8_offsets() {
        assert_eq!(
            tokenize("Café, NAÏVE / Amber").expect("oracle tokenizes subset"),
            vec![
                TokenFact {
                    term: "cafe".to_owned(),
                    position: 0,
                    start: 0,
                    end: 5,
                },
                TokenFact {
                    term: "naive".to_owned(),
                    position: 1,
                    start: 7,
                    end: 13,
                },
                TokenFact {
                    term: "amber".to_owned(),
                    position: 2,
                    start: 16,
                    end: 21,
                },
            ]
        );
    }

    #[test]
    fn independent_phonetic_rules_cover_seeded_name_classes() {
        for (name, expected) in [
            ("Smith", "SM0"),
            ("Smyth", "SM0"),
            ("Schmidt", "XMT"),
            ("Thompson", "TMPS"),
            ("Knight", "NT"),
            ("Xavier", "SFR"),
        ] {
            assert_eq!(phonetic_code(name), expected, "encoding of {name}");
        }
    }

    #[test]
    fn byte_edit_distance_is_independent_and_exact() {
        assert_eq!(edit_distance(b"cobolt", b"cobalt"), 1);
        assert_eq!(edit_distance(b"amber", b"amber"), 0);
        assert_eq!(edit_distance(b"amber", b"zebra"), 4);
    }
}
