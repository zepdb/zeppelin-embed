//! Immutable reverse index for one vocabulary and one phonetic encoder version.

use super::{phonetic, vocabulary::Vocabulary};

#[derive(Eq, Ord, PartialEq, PartialOrd)]
struct Entry {
    code: [u8; phonetic::MAX_CODE_LENGTH],
    term: usize,
}

pub(crate) struct PhoneticIndex {
    entries: Vec<Entry>,
}

fn key(code: &str) -> [u8; phonetic::MAX_CODE_LENGTH] {
    let mut result = [0; phonetic::MAX_CODE_LENGTH];
    for (out, byte) in result.iter_mut().zip(code.bytes()) {
        *out = byte;
    }
    result
}

impl PhoneticIndex {
    /// Reserve the retained term IDs and the encoder's bounded working storage
    /// before allocation. The caller retains the owned charge with this index.
    pub(crate) fn build<E>(
        vocabulary: &Vocabulary,
        mut reserve: impl FnMut(Option<usize>, Option<usize>) -> Result<(), E>,
    ) -> Result<Self, E> {
        let count = vocabulary.iter().count();
        let maximum = vocabulary.iter().map(<[u8]>::len).max().unwrap_or(0);
        let scratch = maximum
            .max(8)
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(phonetic::MAX_CODE_LENGTH));
        reserve(count.checked_mul(std::mem::size_of::<Entry>()), scratch)?;
        let mut entries = Vec::with_capacity(count);
        for (term, bytes) in vocabulary.iter().enumerate() {
            if let Ok(word) = std::str::from_utf8(bytes) {
                let code = phonetic::encode(word);
                if !code.is_empty() {
                    entries.push(Entry {
                        code: key(&code),
                        term,
                    });
                }
            }
        }
        entries.sort_unstable();
        #[cfg(any(test, feature = "test-support"))]
        super::preparation_observer::phonetic_index_build();
        Ok(Self { entries })
    }

    /// IDs are ordered by the source vocabulary's byte order inside each bucket.
    pub(crate) fn terms(&self, code: &str) -> impl Iterator<Item = usize> + '_ {
        let code = key(code);
        let start = self.entries.partition_point(|entry| entry.code < code);
        let remaining = self.entries.split_at(start).1;
        let count = remaining.partition_point(|entry| entry.code == code);
        #[cfg(any(test, feature = "test-support"))]
        super::preparation_observer::phonetic_bucket(count);
        remaining.split_at(count).0.iter().map(|entry| entry.term)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::fts::{
        index::FieldId,
        query::{self, LexicalMatchKind, LexicalQuery, LexicalQueryError},
    };

    fn index(vocabulary: &Vocabulary) -> PhoneticIndex {
        PhoneticIndex::build(vocabulary, |_, _| Ok::<_, std::convert::Infallible>(()))
            .expect("index")
    }
    fn expand(
        query: &LexicalQuery,
        vocabulary: &Vocabulary,
        index: &PhoneticIndex,
    ) -> Result<Vec<query::LexicalExpansion>, LexicalQueryError> {
        query::expand_with_phonetic(query, vocabulary, |code| {
            Ok(query::phonetic_expansions(vocabulary, index, code))
        })
    }

    #[test]
    fn astra_14_phonetic_cached_expansions_match_full_scan() {
        let words = [
            "night", "knight", "knit", "smith", "smyth", "robert", "rupert", "123", "中文",
        ];
        let vocabulary = words
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect::<Vocabulary>();
        let cached = index(&vocabulary);
        for (word, expected) in [
            ("night", vec!["knight", "knit", "night"]),
            ("smith", vec!["smith", "smyth"]),
            ("robert", vec!["robert", "rupert"]),
        ] {
            let actual = expand(
                &LexicalQuery::phonetic(word.as_bytes().to_vec(), FieldId(7)),
                &vocabulary,
                &cached,
            )
            .expect("literal");
            assert_eq!(
                actual.iter().map(|e| e.term.as_slice()).collect::<Vec<_>>(),
                expected.iter().map(|s| s.as_bytes()).collect::<Vec<_>>()
            );
            assert!(
                actual
                    .iter()
                    .all(|e| e.boost_thousandths == 250 && e.kind == LexicalMatchKind::Phonetic)
            );
        }
        use rand::RngCore;
        let mut rng =
            crate::test_support::seeded_rng("astra_14_phonetic_cached_expansions_match_full_scan");
        let mut words = words
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect::<Vec<_>>();
        for _ in 0..4096 {
            let len = 1 + rng.next_u32() % 24;
            words.push(
                (0..len)
                    .map(|_| b'a' + (rng.next_u32() % 26) as u8)
                    .collect(),
            );
        }
        words.extend([vec![0xff], Vec::new(), "éabc".as_bytes().to_vec()]);
        let vocabulary = words.into_iter().collect::<Vocabulary>();
        let cached = index(&vocabulary);
        for word in vocabulary
            .iter()
            .step_by(31)
            .filter_map(|term| std::str::from_utf8(term).ok())
        {
            let code = phonetic::encode(word);
            if code.is_empty() {
                continue;
            }
            let expected = vocabulary
                .iter()
                .filter(|term| {
                    std::str::from_utf8(term).is_ok_and(|word| phonetic::encode(word) == code)
                })
                .collect::<Vec<_>>();
            let actual = expand(
                &LexicalQuery::phonetic(word.as_bytes().to_vec(), FieldId(99)),
                &vocabulary,
                &cached,
            )
            .expect("differential");
            assert_eq!(
                actual.iter().map(|e| e.term.as_slice()).collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[test]
    fn astra_14_phonetic_empty_invalid_and_multifield_cases_preserve_errors() {
        let terms = [
            (b"night".as_slice(), FieldId(7)),
            (b"night", FieldId(8)),
            (b"knight", FieldId(8)),
            (&[0xff], FieldId(7)),
            (b"123", FieldId(7)),
            (b"", FieldId(8)),
        ];
        let vocabulary = Vocabulary::build(terms.iter().copied(), |_, _| {
            Ok::<_, std::convert::Infallible>(())
        })
        .expect("vocabulary");
        let cached = index(&vocabulary);
        for field in [FieldId(7), FieldId(8), FieldId(99)] {
            let hits = expand(
                &LexicalQuery::phonetic(b"night".to_vec(), field),
                &vocabulary,
                &cached,
            )
            .expect("global dictionary");
            assert_eq!(
                hits.iter().map(|e| e.term.as_slice()).collect::<Vec<_>>(),
                vec![b"knight".as_slice(), b"night"]
            );
        }
        for (bytes, error) in [
            (vec![0xff], LexicalQueryError::InvalidPhoneticUtf8),
            (Vec::new(), LexicalQueryError::EmptyPhoneticCode),
            (b"123".to_vec(), LexicalQueryError::EmptyPhoneticCode),
        ] {
            let mut lookup_called = false;
            let actual = query::expand_with_phonetic(
                &LexicalQuery::phonetic(bytes, FieldId(7)),
                &vocabulary,
                |_| {
                    lookup_called = true;
                    Ok(Vec::new())
                },
            );
            assert_eq!(actual, Err(error));
            assert!(
                !lookup_called,
                "invalid input must not build a reverse index"
            );
        }
    }
}
