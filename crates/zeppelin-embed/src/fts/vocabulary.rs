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

    #[cfg(test)]
    pub(crate) fn build<'a, E>(
        terms: impl Iterator<Item = (&'a [u8], FieldId)> + Clone,
        reserve: impl FnMut(Option<usize>, Option<usize>) -> Result<(), E>,
    ) -> Result<Self, E> {
        let mut work = super::control::WorkCheck::new(|| Ok(()));
        Self::build_controlled(terms, reserve, &mut work)
    }

    /// Reserve temporary references and retained storage before allocating.
    /// Canceled construction never publishes the partial dictionary.
    pub(crate) fn build_controlled<'a, E>(
        terms: impl Iterator<Item = (&'a [u8], FieldId)> + Clone,
        mut reserve: impl FnMut(Option<usize>, Option<usize>) -> Result<(), E>,
        work: &mut super::control::WorkCheck<impl FnMut() -> Result<(), E>>,
    ) -> Result<Self, E> {
        work.check_now()?;
        let mut count = 0_usize;
        for _ in terms.clone() {
            work.step()?;
            count += 1;
        }
        let temporary = count.checked_mul(std::mem::size_of::<(&[u8], FieldId)>());
        reserve(temporary, Some(0))?;
        let mut sorted = Vec::with_capacity(count);
        for term in terms {
            work.step()?;
            sorted.push(term);
        }
        super::control::sort_by(&mut sorted, Ord::cmp, work)?;
        let mut unique = 0_usize;
        for read in 0..sorted.len() {
            work.step()?;
            if unique == 0 || sorted.get(read) != sorted.get(unique - 1) {
                sorted.swap(unique, read);
                unique += 1;
            }
        }
        sorted.truncate(unique);
        let mut previous = None;
        let mut term_count = 0_usize;
        let mut term_bytes = Some(0_usize);
        for (term, _) in &sorted {
            work.step()?;
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
            work.step()?;
            let mut end = 0;
            for (candidate, _) in remaining {
                work.step()?;
                #[cfg(any(test, feature = "test-support"))]
                {
                    group_checks += 1;
                }
                if candidate != term {
                    break;
                }
                end += 1;
            }
            let (group, rest) = remaining.split_at(end);
            let term_start = terms.len();
            let field_start = fields.len();
            for chunk in term.chunks(4_096) {
                work.step()?;
                terms.extend_from_slice(chunk);
            }
            for (_, field) in group {
                work.step()?;
                fields.push(*field);
            }
            entries.push(Entry {
                term: term_start..terms.len(),
                fields: field_start..fields.len(),
            });
            remaining = rest;
        }
        #[cfg(any(test, feature = "test-support"))]
        {
            super::preparation_observer::record_vocabulary_group_checks(group_checks);
        }
        let mut by_length = Vec::with_capacity(term_count);
        for entry in &entries {
            work.step()?;
            by_length.push(entry.term.clone());
        }
        super::control::sort_by(
            &mut by_length,
            |left, right| (left.len(), left.start).cmp(&(right.len(), right.start)),
            work,
        )?;
        work.check_now()?;
        #[cfg(any(test, feature = "test-support"))]
        super::preparation_observer::vocabulary_build(terms.len());
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

    pub(crate) fn exact(&self, term: &[u8]) -> Option<&[u8]> {
        let index = self.entries.partition_point(|entry| {
            #[cfg(any(test, feature = "test-support"))]
            super::preparation_observer::vocabulary_seek();
            self.term(entry) < term
        });
        let candidate = self.entries.get(index).map(|entry| self.term(entry))?;
        (candidate == term).then_some(candidate)
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
#[allow(clippy::expect_used, clippy::items_after_test_module)]
mod cancellation_tests {
    use super::*;

    #[test]
    fn astra_18_vocabulary_build_cancels_during_input_walks() {
        let terms = (0..4_096)
            .map(|i| format!("word{i:04}").into_bytes())
            .collect::<Vec<_>>();
        for phase in [0, 1] {
            let visits = std::cell::Cell::new(0);
            let reservations = std::cell::Cell::new(0);
            let input = terms
                .iter()
                .map(|term| (term.as_slice(), FieldId(0)))
                .inspect(|_| visits.set(visits.get() + 1));
            let mut work = super::super::control::WorkCheck::new(|| {
                if reservations.get() >= phase && visits.get() >= phase * 4_096 + 64 {
                    Err("cancelled")
                } else {
                    Ok(())
                }
            });
            let result = Vocabulary::build_controlled(
                input,
                |_, _| {
                    reservations.set(reservations.get() + 1);
                    Ok(())
                },
                &mut work,
            );
            println!(
                "phase={phase}, visits={}, reservations={}",
                visits.get(),
                reservations.get()
            );
            assert!(matches!(result, Err("cancelled")));
            assert!(
                visits.get() <= phase * 4_096 + 128,
                "input traversal did not stop"
            );
        }
        let mut work = super::super::control::WorkCheck::new(|| Ok::<(), ()>(()));
        let clean = Vocabulary::build_controlled(
            terms.iter().map(|term| (term.as_slice(), FieldId(0))),
            |_, _| Ok(()),
            &mut work,
        )
        .expect("clean build");
        assert_eq!(
            clean.iter().collect::<Vec<_>>(),
            terms.iter().map(Vec::as_slice).collect::<Vec<_>>()
        );
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
