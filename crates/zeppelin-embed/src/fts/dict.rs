//! The sorted, front-coded term dictionary.
//!
//! # Why not an FST (task 13 D4)
//!
//! `fst` and `tantivy-fst` are outside the dependency allowlist, and adding
//! one is an owner decision this plan recommends against. A sorted
//! front-coded dictionary reaches the same asymptotics for the operations
//! this engine needs: exact lookup is a binary search over block heads then
//! a short linear walk; prefix enumeration is a range scan; and task 15's
//! Levenshtein matching runs as automaton-guided lexicographic seek, which
//! needs `seek_to` and forward iteration, both of which a sorted array
//! gives directly.
//!
//! What an FST buys over this is a smaller dictionary and shared-suffix
//! compression. Under the cardinal rule that is the wrong trade: the FST
//! wins on bytes, the sorted array wins on decode simplicity and predictable
//! cache behaviour, and bytes are no longer the scarce resource.
//!
//! # Layout
//!
//! Terms are sorted bytewise ascending and grouped into blocks of
//! [`TERMS_PER_BLOCK`]. Within a block the first term is stored in full and
//! every later term stores `(shared_prefix_len, suffix)` against its
//! predecessor. A materialized block index holds each block's full head term
//! and its byte offset, so a lookup binary-searches the heads — touching one
//! small contiguous array — and then decodes at most one block.
//!
//! The block index is materialized rather than recomputed precisely because
//! it is the faster shape; it is the "denser structure" the 5 MB directive
//! asks for.

use std::collections::BTreeMap;

/// Terms per front-coded block.
///
/// Sixteen keeps the linear walk inside one block short while amortizing the
/// full head term over enough entries to matter.
pub const TERMS_PER_BLOCK: usize = 16;

/// What the dictionary stores for each term.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TermInfo {
    /// Documents containing the term, within this segment.
    pub document_frequency: u32,
    /// Byte offset of the term's encoded posting list.
    pub postings_offset: u64,
    /// Byte length of the term's encoded posting list.
    pub postings_length: u32,
}

/// A dictionary rejection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DictError {
    /// Terms were supplied out of order.
    NotSorted {
        /// The term that broke the ordering.
        term: String,
    },
    /// The same term was supplied twice.
    Duplicate {
        /// The repeated term.
        term: String,
    },
    /// A seek or prefix scan was given an empty term.
    EmptyTerm,
}

impl std::fmt::Display for DictError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSorted { term } => write!(formatter, "term {term:?} is out of order"),
            Self::Duplicate { term } => write!(formatter, "term {term:?} appears twice"),
            Self::EmptyTerm => formatter.write_str("an empty term is not a valid query"),
        }
    }
}

impl std::error::Error for DictError {}

/// One block's entry in the materialized index.
#[derive(Clone, Debug, Eq, PartialEq)]
struct BlockIndexEntry {
    /// The block's first term, stored in full.
    head: Vec<u8>,
    /// Index of the block's first term in the flat term order.
    first_term: usize,
}

/// A sorted, front-coded term dictionary.
#[derive(Clone, Debug, Default)]
pub struct TermDictionary {
    /// Every term, in sorted order, decoded.
    terms: Vec<Vec<u8>>,
    /// Per-term information, parallel to `terms`.
    infos: Vec<TermInfo>,
    /// Materialized block heads for binary search.
    index: Vec<BlockIndexEntry>,
    /// Front-coded encoding of the terms, one run per block.
    encoded: Vec<u8>,
}

impl TermDictionary {
    /// Builds a dictionary from an ordered map of terms.
    ///
    /// A `BTreeMap` is required rather than accepted for convenience: it
    /// makes the sorted precondition a type-level fact, so the ordering
    /// cannot be violated by a caller iterating a hash map.
    #[must_use]
    pub fn from_map(entries: &BTreeMap<Vec<u8>, TermInfo>) -> Self {
        let mut dictionary = Self::default();
        for (term, info) in entries {
            // BTreeMap iteration is sorted and unique, so push cannot fail.
            let _ = dictionary.push(term, *info);
        }
        dictionary
    }

    /// Appends one term. Terms must arrive sorted and unique.
    ///
    /// # Errors
    ///
    /// Returns [`DictError::NotSorted`] or [`DictError::Duplicate`].
    pub fn push(&mut self, term: &[u8], info: TermInfo) -> Result<(), DictError> {
        if let Some(last) = self.terms.last() {
            match term.cmp(last.as_slice()) {
                std::cmp::Ordering::Less => {
                    return Err(DictError::NotSorted {
                        term: String::from_utf8_lossy(term).into_owned(),
                    });
                }
                std::cmp::Ordering::Equal => {
                    return Err(DictError::Duplicate {
                        term: String::from_utf8_lossy(term).into_owned(),
                    });
                }
                std::cmp::Ordering::Greater => {}
            }
        }

        let position = self.terms.len();
        if position.is_multiple_of(TERMS_PER_BLOCK) {
            self.index.push(BlockIndexEntry {
                head: term.to_vec(),
                first_term: position,
            });
            // A block head is stored in full: shared prefix zero.
            self.encoded.push(0);
            push_varint(&mut self.encoded, term.len());
            self.encoded.extend_from_slice(term);
        } else {
            let shared = self
                .terms
                .last()
                .map(|previous| shared_prefix_len(previous, term))
                .unwrap_or(0)
                .min(255);
            let suffix = term.get(shared..).unwrap_or(&[]);
            self.encoded.push(u8::try_from(shared).unwrap_or(u8::MAX));
            push_varint(&mut self.encoded, suffix.len());
            self.encoded.extend_from_slice(suffix);
        }

        self.terms.push(term.to_vec());
        self.infos.push(info);
        Ok(())
    }

    /// Returns the number of terms.
    #[must_use]
    pub fn len(&self) -> usize {
        self.terms.len()
    }

    /// Returns true when the dictionary holds no terms.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Returns the front-coded bytes.
    #[must_use]
    pub fn encoded_bytes(&self) -> &[u8] {
        &self.encoded
    }

    /// Returns the term at a flat index.
    #[must_use]
    pub fn term_at(&self, index: usize) -> Option<&[u8]> {
        self.terms.get(index).map(Vec::as_slice)
    }

    /// Returns the information at a flat index.
    #[must_use]
    pub fn info_at(&self, index: usize) -> Option<TermInfo> {
        self.infos.get(index).copied()
    }

    /// Looks one term up exactly.
    #[must_use]
    pub fn get(&self, term: &[u8]) -> Option<TermInfo> {
        let index = self.seek_exact(term)?;
        self.infos.get(index).copied()
    }

    /// Returns the flat index of `term`, if present.
    ///
    /// Binary-searches the materialized block heads, then walks at most one
    /// block. This is the operation task 15's automaton-guided seek repeats.
    #[must_use]
    pub fn seek_exact(&self, term: &[u8]) -> Option<usize> {
        let block = self.block_for(term)?;
        let start = self.index.get(block)?.first_term;
        let end = (start + TERMS_PER_BLOCK).min(self.terms.len());
        for index in start..end {
            match self.terms.get(index)?.as_slice().cmp(term) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => return Some(index),
                std::cmp::Ordering::Greater => return None,
            }
        }
        None
    }

    /// Returns the flat index of the first term at or after `term`.
    ///
    /// This is the primitive task 15's Levenshtein walk drives: from an
    /// automaton state it computes the smallest acceptable next term and
    /// seeks here, rather than enumerating the dictionary.
    #[must_use]
    pub fn seek_ceiling(&self, term: &[u8]) -> Option<usize> {
        // Binary search over the flat sorted terms via the block index.
        let mut low = 0_usize;
        let mut high = self.terms.len();
        while low < high {
            let middle = low + (high - low) / 2;
            let candidate = self.terms.get(middle)?;
            if candidate.as_slice() < term {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        (low < self.terms.len()).then_some(low)
    }

    /// Returns every flat index whose term starts with `prefix`.
    ///
    /// # Errors
    ///
    /// Returns [`DictError::EmptyTerm`] for an empty prefix. An empty prefix
    /// is a full scan wearing a query's clothes, so it is refused rather
    /// than served.
    pub fn prefix_range(&self, prefix: &[u8]) -> Result<std::ops::Range<usize>, DictError> {
        if prefix.is_empty() {
            return Err(DictError::EmptyTerm);
        }
        let Some(start) = self.seek_ceiling(prefix) else {
            return Ok(self.terms.len()..self.terms.len());
        };
        let mut end = start;
        while let Some(term) = self.terms.get(end) {
            if term.starts_with(prefix) {
                end += 1;
            } else {
                break;
            }
        }
        Ok(start..end)
    }

    /// Returns the block whose range could contain `term`.
    fn block_for(&self, term: &[u8]) -> Option<usize> {
        if self.index.is_empty() {
            return None;
        }
        // Find the last block whose head is <= term.
        let mut low = 0_usize;
        let mut high = self.index.len();
        while low < high {
            let middle = low + (high - low) / 2;
            let head = &self.index.get(middle)?.head;
            if head.as_slice() <= term {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low.checked_sub(1)
    }

    /// Decodes the front-coded bytes back into the term list.
    ///
    /// Used by the golden test to prove the encoding is self-describing:
    /// the in-memory `terms` vector is a decode cache, not the source of
    /// truth for the persisted form.
    #[must_use]
    pub fn decode_terms(&self) -> Option<Vec<Vec<u8>>> {
        let mut out: Vec<Vec<u8>> = Vec::with_capacity(self.terms.len());
        let mut cursor = 0_usize;
        let mut previous: Vec<u8> = Vec::new();
        let mut position = 0_usize;
        while cursor < self.encoded.len() {
            let shared = usize::from(*self.encoded.get(cursor)?);
            cursor += 1;
            let (length, consumed) = read_varint(self.encoded.get(cursor..)?)?;
            cursor += consumed;
            let suffix = self.encoded.get(cursor..cursor + length)?;
            cursor += length;
            let mut term = if position.is_multiple_of(TERMS_PER_BLOCK) {
                Vec::new()
            } else {
                previous.get(..shared)?.to_vec()
            };
            term.extend_from_slice(suffix);
            previous = term.clone();
            out.push(term);
            position += 1;
        }
        Some(out)
    }
}

fn shared_prefix_len(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right.iter())
        .take_while(|(a, b)| a == b)
        .count()
}

fn push_varint(output: &mut Vec<u8>, mut value: usize) {
    loop {
        let byte = u8::try_from(value & 0x7F).unwrap_or(0);
        value >>= 7;
        if value == 0 {
            output.push(byte);
            return;
        }
        output.push(byte | 0x80);
    }
}

fn read_varint(input: &[u8]) -> Option<(usize, usize)> {
    let mut value = 0_usize;
    let mut shift = 0_u32;
    for (index, byte) in input.iter().enumerate() {
        if shift >= usize::BITS {
            return None;
        }
        value |= usize::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some((value, index + 1));
        }
        shift += 7;
    }
    None
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use super::*;

    fn build(terms: &[&str]) -> TermDictionary {
        let mut dictionary = TermDictionary::default();
        for (index, term) in terms.iter().enumerate() {
            dictionary
                .push(
                    term.as_bytes(),
                    TermInfo {
                        document_frequency: u32::try_from(index + 1).expect("small"),
                        postings_offset: index as u64 * 16,
                        postings_length: 16,
                    },
                )
                .expect("sorted fixture");
        }
        dictionary
    }

    fn sorted_sample() -> Vec<String> {
        let mut terms: Vec<String> = Vec::new();
        for first in b'a'..=b'f' {
            for second in b'a'..=b'z' {
                terms.push(format!("{}{}", char::from(first), char::from(second)));
            }
        }
        terms.sort();
        terms
    }

    #[test]
    fn exact_lookup_finds_every_term_and_only_those() {
        let sample = sorted_sample();
        let refs: Vec<&str> = sample.iter().map(String::as_str).collect();
        let dictionary = build(&refs);
        assert_eq!(dictionary.len(), sample.len());
        for (index, term) in sample.iter().enumerate() {
            let info = dictionary.get(term.as_bytes()).expect("term present");
            assert_eq!(
                info.document_frequency,
                u32::try_from(index + 1).expect("small")
            );
        }
        for absent in ["", "zzz", "aa0", "g", "ab_"] {
            assert_eq!(dictionary.get(absent.as_bytes()), None, "{absent:?}");
        }
    }

    #[test]
    fn front_coding_round_trips_through_its_own_bytes() {
        let sample = sorted_sample();
        let refs: Vec<&str> = sample.iter().map(String::as_str).collect();
        let dictionary = build(&refs);
        let decoded = dictionary.decode_terms().expect("decodes");
        let expected: Vec<Vec<u8>> = sample.iter().map(|t| t.as_bytes().to_vec()).collect();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn front_coding_actually_shares_prefixes() {
        let dictionary = build(&["engine", "engineer", "engineering", "engines"]);
        // Four terms averaging nine bytes would be 36 bytes stored flat; the
        // front-coded run must be materially smaller.
        assert!(
            dictionary.encoded_bytes().len() < 30,
            "front coding did not compress: {} bytes",
            dictionary.encoded_bytes().len()
        );
        assert_eq!(dictionary.decode_terms().expect("decodes").len(), 4);
    }

    #[test]
    fn prefix_range_equals_brute_force() {
        let sample = sorted_sample();
        let refs: Vec<&str> = sample.iter().map(String::as_str).collect();
        let dictionary = build(&refs);
        for prefix in ["a", "b", "ab", "zz", "f"] {
            let range = dictionary.prefix_range(prefix.as_bytes()).expect("prefix");
            let actual: Vec<&str> = range
                .clone()
                .filter_map(|index| dictionary.term_at(index))
                .filter_map(|term| std::str::from_utf8(term).ok())
                .collect();
            let expected: Vec<&str> = sample
                .iter()
                .map(String::as_str)
                .filter(|term| term.starts_with(prefix))
                .collect();
            assert_eq!(actual, expected, "prefix {prefix:?}");
        }
    }

    #[test]
    fn an_empty_prefix_is_a_typed_error_not_a_full_scan() {
        let dictionary = build(&["alpha", "beta"]);
        assert_eq!(dictionary.prefix_range(b""), Err(DictError::EmptyTerm));
    }

    #[test]
    fn seek_ceiling_lands_on_the_first_term_at_or_after_the_key() {
        let dictionary = build(&["ant", "bee", "cat", "dog"]);
        assert_eq!(dictionary.seek_ceiling(b"ant"), Some(0));
        assert_eq!(dictionary.seek_ceiling(b"apple"), Some(1));
        assert_eq!(dictionary.seek_ceiling(b"bee"), Some(1));
        assert_eq!(dictionary.seek_ceiling(b"cz"), Some(3));
        assert_eq!(dictionary.seek_ceiling(b"zebra"), None);
        assert_eq!(dictionary.seek_ceiling(b""), Some(0));
    }

    #[test]
    fn out_of_order_and_duplicate_terms_are_refused() {
        let mut dictionary = TermDictionary::default();
        dictionary
            .push(b"beta", TermInfo::default())
            .expect("first");
        assert_eq!(
            dictionary.push(b"alpha", TermInfo::default()),
            Err(DictError::NotSorted {
                term: String::from("alpha")
            })
        );
        assert_eq!(
            dictionary.push(b"beta", TermInfo::default()),
            Err(DictError::Duplicate {
                term: String::from("beta")
            })
        );
    }

    #[test]
    fn an_empty_dictionary_answers_every_query_negatively() {
        let dictionary = TermDictionary::default();
        assert!(dictionary.is_empty());
        assert_eq!(dictionary.get(b"anything"), None);
        assert_eq!(dictionary.seek_exact(b"anything"), None);
        assert_eq!(dictionary.seek_ceiling(b"anything"), None);
        assert_eq!(dictionary.prefix_range(b"a").expect("prefix"), 0..0);
    }

    #[test]
    fn building_from_a_sorted_map_matches_incremental_pushes() {
        let mut map = BTreeMap::new();
        for (index, term) in ["ant", "bee", "cat"].into_iter().enumerate() {
            map.insert(
                term.as_bytes().to_vec(),
                TermInfo {
                    document_frequency: u32::try_from(index + 1).expect("small"),
                    postings_offset: index as u64 * 16,
                    postings_length: 16,
                },
            );
        }
        let from_map = TermDictionary::from_map(&map);
        let incremental = build(&["ant", "bee", "cat"]);
        assert_eq!(from_map.encoded_bytes(), incremental.encoded_bytes());
        assert_eq!(from_map.len(), 3);
    }

    #[test]
    fn varints_round_trip_at_the_boundaries() {
        for value in [0_usize, 1, 127, 128, 300, 16_383, 16_384, 1_000_000] {
            let mut buffer = Vec::new();
            push_varint(&mut buffer, value);
            let (decoded, consumed) = read_varint(&buffer).expect("decodes");
            assert_eq!(decoded, value);
            assert_eq!(consumed, buffer.len());
        }
        assert_eq!(read_varint(&[0x80]), None, "an unterminated varint");
    }

    #[test]
    fn lookups_work_across_many_blocks() {
        // Enough terms to exercise the block index rather than one block.
        let sample = sorted_sample();
        assert!(sample.len() > TERMS_PER_BLOCK * 4);
        let refs: Vec<&str> = sample.iter().map(String::as_str).collect();
        let dictionary = build(&refs);
        assert_eq!(
            dictionary.seek_exact(sample.last().expect("non-empty").as_bytes()),
            Some(sample.len() - 1)
        );
        assert_eq!(dictionary.seek_exact(sample[0].as_bytes()), Some(0));
    }
}
