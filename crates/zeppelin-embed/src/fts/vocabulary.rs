//! Sorted expansion dictionary owned by one immutable lexical assembly.

use super::index::FieldId;

struct Entry {
    term: std::ops::Range<usize>,
    fields: std::ops::Range<usize>,
}

/// Terms and field memberships are deduplicated independently of live rows.
/// The existing segment dictionaries are Vec-owned, so their bytes are copied
/// once here; subsequent queries share this complete immutable dictionary.
pub(crate) struct Vocabulary {
    entries: Vec<Entry>,
    terms: Vec<u8>,
    fields: Vec<FieldId>,
    // Offsets into the same term bytes, ordered by (byte length, lexical order).
    by_length: Vec<std::ops::Range<usize>>,
}

impl Vocabulary {
    pub(crate) const fn empty() -> Self {
        Self {
            entries: Vec::new(),
            terms: Vec::new(),
            fields: Vec::new(),
            by_length: Vec::new(),
        }
    }

    /// Reserve temporary sort references and retained storage before allocating.
    /// The caller owns both reservations, including every failed build.
    pub(crate) fn build<'a, E>(
        terms: impl Iterator<Item = (&'a [u8], FieldId)> + Clone,
        mut reserve: impl FnMut(Option<usize>, Option<usize>) -> Result<(), E>,
    ) -> Result<Self, E> {
        let count = terms.clone().count();
        let temporary = count.checked_mul(std::mem::size_of::<(&[u8], FieldId)>());
        reserve(temporary, Some(0))?;
        let mut sorted = Vec::with_capacity(count);
        sorted.extend(terms);
        sorted.sort_unstable();
        sorted.dedup();
        let mut previous = None;
        let mut term_count = 0_usize;
        let mut term_bytes = Some(0_usize);
        for (term, _) in &sorted {
            if previous != Some(*term) {
                term_count += 1;
                term_bytes = term_bytes.and_then(|bytes| bytes.checked_add(term.len()));
                previous = Some(*term);
            }
        }
        let retained = term_count
            .checked_mul(std::mem::size_of::<Entry>())
            .and_then(|bytes| {
                bytes.checked_add(
                    term_count.checked_mul(std::mem::size_of::<std::ops::Range<usize>>())?,
                )
            })
            .and_then(|bytes| bytes.checked_add(term_bytes?))
            .and_then(|bytes| {
                bytes.checked_add(sorted.len().checked_mul(std::mem::size_of::<FieldId>())?)
            });
        reserve(temporary, retained)?;
        let mut entries = Vec::with_capacity(term_count);
        // The reservation callback rejects overflow before construction.
        // Contiguous buffers avoid per-term heap allocations.
        let mut terms = Vec::with_capacity(term_bytes.unwrap_or(0));
        let mut fields = Vec::with_capacity(sorted.len());
        let mut remaining = sorted.as_slice();
        #[cfg(any(test, feature = "test-support"))]
        let mut group_checks = 0;
        while let Some((term, _)) = remaining.first() {
            let end = remaining
                .iter()
                .take_while(|(candidate, _)| {
                    #[cfg(any(test, feature = "test-support"))]
                    {
                        group_checks += 1;
                    }
                    candidate == term
                })
                .count();
            let (group, rest) = remaining.split_at(end);
            let term_start = terms.len();
            let field_start = fields.len();
            terms.extend_from_slice(term);
            fields.extend(group.iter().map(|(_, field)| *field));
            entries.push(Entry {
                term: term_start..terms.len(),
                fields: field_start..fields.len(),
            });
            remaining = rest;
        }
        #[cfg(any(test, feature = "test-support"))]
        {
            super::preparation_observer::vocabulary_build(terms.len());
            super::preparation_observer::record_vocabulary_group_checks(group_checks);
        }
        let mut by_length = Vec::with_capacity(term_count);
        by_length.extend(entries.iter().map(|entry| entry.term.clone()));
        by_length.sort_unstable_by_key(|range| (range.len(), range.start));
        Ok(Self {
            entries,
            terms,
            fields,
            by_length,
        })
    }

    // Ranges are private and constructed only from the corresponding buffers'
    // lengths above. They cannot be supplied by callers or persisted bytes.
    fn term(&self, entry: &Entry) -> &[u8] {
        self.terms
            .split_at(entry.term.start)
            .1
            .split_at(entry.term.len())
            .0
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = (&[u8], &[FieldId])> {
        self.entries.iter().map(|entry| {
            (
                self.term(entry),
                self.fields
                    .split_at(entry.fields.start)
                    .1
                    .split_at(entry.fields.len())
                    .0,
            )
        })
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &[u8]> {
        self.entries().map(|(term, _)| term)
    }

    /// IDs come only from an index built from this exact immutable vocabulary.
    pub(crate) fn select<'a>(
        &'a self,
        ids: impl Iterator<Item = usize> + 'a,
    ) -> impl Iterator<Item = &'a [u8]> {
        ids.flat_map(|id| self.entries.split_at(id).1.split_at(1).0.iter())
            .map(|entry| self.term(entry))
    }

    /// Only byte lengths that can be within the requested edit budget.
    /// Returned terms are length-ordered; the caller restores lexical result order.
    pub(crate) fn fuzzy_candidates(
        &self,
        length: usize,
        distance: u32,
    ) -> impl Iterator<Item = &[u8]> {
        let minimum = length.saturating_sub(distance as usize);
        let maximum = length.saturating_add(distance as usize);
        let start = self
            .by_length
            .partition_point(|range| range.len() < minimum);
        let remaining = self.by_length.split_at(start).1;
        let count = remaining.partition_point(|range| range.len() <= maximum);
        remaining
            .split_at(count)
            .0
            .iter()
            .map(|range| self.terms.split_at(range.start).1.split_at(range.len()).0)
    }

    pub(crate) fn prefix<'a>(&'a self, prefix: &'a [u8]) -> impl Iterator<Item = &'a [u8]> {
        let start = self.entries.partition_point(|entry| {
            #[cfg(any(test, feature = "test-support"))]
            super::preparation_observer::vocabulary_seek();
            self.term(entry) < prefix
        });
        self.entries
            .iter()
            .skip(start)
            .take_while(move |entry| {
                #[cfg(any(test, feature = "test-support"))]
                super::preparation_observer::vocabulary_visit();
                self.term(entry).starts_with(prefix)
            })
            .map(|entry| self.term(entry))
    }
}

#[cfg(test)]
impl FromIterator<Vec<u8>> for Vocabulary {
    fn from_iter<T: IntoIterator<Item = Vec<u8>>>(iter: T) -> Self {
        let terms = iter.into_iter().collect::<Vec<_>>();
        match Self::build(
            terms
                .iter()
                .map(|term| (term.as_slice(), super::index::DEFAULT_FIELD)),
            |_, _| Ok::<_, std::convert::Infallible>(()),
        ) {
            Ok(value) => value,
            Err(never) => match never {},
        }
    }
}
