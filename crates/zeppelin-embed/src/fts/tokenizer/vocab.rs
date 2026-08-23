//! User vocabulary and synonym stacking — the product feature of task 12.
//!
//! No off-the-shelf tokenizer versions its own behaviour as index metadata
//! (`research/05:202`), and none lets a caller declare that `put_if_match`,
//! `put if match`, and `putifmatch` are the same thing. This module is that
//! declaration, and its digest is folded into the tokenizer epoch, so a
//! vocabulary edit is a visible index-invalidating change rather than silent
//! drift.
//!
//! # Semantics
//!
//! An entry maps one or more *surface forms* — each a sequence of one or
//! more analyzed terms — onto a single canonical term. When a surface form
//! matches a run of tokens, the canonical term is emitted at the run's first
//! position, stacked beside the tokens that produced it. That is Lucene's
//! synonym-filter position convention: a stacked variant occupies the same
//! position, so BM25 term statistics stay coherent and a phrase query
//! crosses the stack for free.
//!
//! Matching is longest-run-first and greedy, so a three-term entry wins over
//! a one-term entry starting at the same position. Ties between entries of
//! equal length are broken by canonical term order, which makes the emission
//! deterministic regardless of insertion order — a property the
//! `analysis_is_deterministic_across_two_runs_of_the_same_input` test pins.

use std::collections::BTreeMap;

/// The longest surface form, in terms, the vocabulary will match.
///
/// Bounding the run length keeps matching linear in the token count; without
/// it a pathological vocabulary could make analysis quadratic.
pub const MAX_SURFACE_TERMS: usize = 8;

/// A user vocabulary rejection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VocabError {
    /// The canonical term was empty.
    EmptyCanonicalTerm,
    /// A surface form contained no terms.
    EmptySurfaceForm {
        /// The canonical term the empty surface form was declared under.
        canonical: String,
    },
    /// A surface form exceeded [`MAX_SURFACE_TERMS`].
    SurfaceFormTooLong {
        /// The canonical term the oversized surface form was declared under.
        canonical: String,
        /// The rejected term count.
        terms: usize,
    },
    /// Two entries claimed the same surface form.
    ConflictingSurfaceForm {
        /// The surface form claimed twice, terms joined by a space.
        surface: String,
        /// The canonical term that already holds it.
        existing: String,
        /// The canonical term that tried to take it.
        attempted: String,
    },
}

impl std::fmt::Display for VocabError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyCanonicalTerm => write!(formatter, "vocabulary canonical term was empty"),
            Self::EmptySurfaceForm { canonical } => {
                write!(formatter, "surface form for {canonical:?} had no terms")
            }
            Self::SurfaceFormTooLong { canonical, terms } => write!(
                formatter,
                "surface form for {canonical:?} has {terms} terms, over the {MAX_SURFACE_TERMS} limit"
            ),
            Self::ConflictingSurfaceForm {
                surface,
                existing,
                attempted,
            } => write!(
                formatter,
                "surface form {surface:?} is already claimed by {existing:?}; {attempted:?} cannot take it"
            ),
        }
    }
}

impl std::error::Error for VocabError {}

/// A user-declared vocabulary of canonical terms and their surface forms.
///
/// Ordered storage is deliberate: the digest and the emission order must not
/// depend on hash-map iteration.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Vocabulary {
    entries: BTreeMap<Vec<String>, String>,
    longest_surface: usize,
}

impl Vocabulary {
    /// Creates an empty vocabulary.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns true when no entry is declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the number of declared surface forms.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns the longest declared surface form, in terms.
    #[must_use]
    pub const fn longest_surface_terms(&self) -> usize {
        self.longest_surface
    }

    /// Declares that every surface form resolves to `canonical`.
    ///
    /// The canonical term is itself registered as a surface form, so an
    /// exact occurrence of it also stacks the canonical term and the
    /// statistics stay consistent.
    ///
    /// # Errors
    ///
    /// Returns [`VocabError`] for an empty canonical term, an empty or
    /// oversized surface form, or a surface form already claimed by a
    /// different canonical term.
    pub fn declare(
        &mut self,
        canonical: &str,
        surface_forms: &[&[&str]],
    ) -> Result<(), VocabError> {
        if canonical.is_empty() {
            return Err(VocabError::EmptyCanonicalTerm);
        }
        let canonical_key: Vec<String> = vec![canonical.to_owned()];
        let mut pending: Vec<Vec<String>> = vec![canonical_key];
        for form in surface_forms {
            if form.is_empty() {
                return Err(VocabError::EmptySurfaceForm {
                    canonical: canonical.to_owned(),
                });
            }
            if form.len() > MAX_SURFACE_TERMS {
                return Err(VocabError::SurfaceFormTooLong {
                    canonical: canonical.to_owned(),
                    terms: form.len(),
                });
            }
            pending.push(form.iter().map(|term| (*term).to_owned()).collect());
        }
        for key in &pending {
            if let Some(existing) = self.entries.get(key)
                && existing != canonical
            {
                return Err(VocabError::ConflictingSurfaceForm {
                    surface: key.join(" "),
                    existing: existing.clone(),
                    attempted: canonical.to_owned(),
                });
            }
        }
        for key in pending {
            self.longest_surface = self.longest_surface.max(key.len());
            self.entries.insert(key, canonical.to_owned());
        }
        Ok(())
    }

    /// Removes a canonical term and every surface form that resolves to it.
    ///
    /// Returns true when anything was removed.
    pub fn remove(&mut self, canonical: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|_, value| value != canonical);
        let removed = self.entries.len() != before;
        if removed {
            self.longest_surface = self.entries.keys().map(Vec::len).max().unwrap_or(0);
        }
        removed
    }

    /// Returns the canonical term for an exact run of terms.
    #[must_use]
    pub fn canonical_for(&self, terms: &[String]) -> Option<&str> {
        self.entries.get(terms).map(String::as_str)
    }

    /// Returns every declared surface form and canonical term, in order.
    pub(crate) fn entries(&self) -> impl Iterator<Item = (&Vec<String>, &String)> {
        self.entries.iter()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn owned(terms: &[&str]) -> Vec<String> {
        terms.iter().map(|term| (*term).to_owned()).collect()
    }

    #[test]
    fn a_declaration_registers_every_surface_form_and_the_canonical_term() {
        let mut vocabulary = Vocabulary::new();
        vocabulary
            .declare(
                "put_if_match",
                &[&["put", "if", "match"][..], &["putifmatch"][..]],
            )
            .expect("valid declaration");
        assert_eq!(
            vocabulary.canonical_for(&owned(&["put", "if", "match"])),
            Some("put_if_match")
        );
        assert_eq!(
            vocabulary.canonical_for(&owned(&["putifmatch"])),
            Some("put_if_match")
        );
        assert_eq!(
            vocabulary.canonical_for(&owned(&["put_if_match"])),
            Some("put_if_match")
        );
        assert_eq!(vocabulary.longest_surface_terms(), 3);
    }

    #[test]
    fn removal_clears_every_surface_form() {
        let mut vocabulary = Vocabulary::new();
        vocabulary
            .declare("put_if_match", &[&["put", "if", "match"][..]])
            .expect("valid declaration");
        assert!(vocabulary.remove("put_if_match"));
        assert!(vocabulary.is_empty());
        assert_eq!(vocabulary.longest_surface_terms(), 0);
        assert!(!vocabulary.remove("put_if_match"));
    }

    #[test]
    fn conflicting_surface_forms_are_refused_rather_than_overwritten() {
        let mut vocabulary = Vocabulary::new();
        vocabulary
            .declare("alpha", &[&["shared", "form"][..]])
            .expect("valid declaration");
        let error = vocabulary
            .declare("beta", &[&["shared", "form"][..]])
            .expect_err("a second claim must be refused");
        assert!(matches!(
            error,
            VocabError::ConflictingSurfaceForm { .. }
        ));
    }

    #[test]
    fn degenerate_declarations_are_typed_errors() {
        let mut vocabulary = Vocabulary::new();
        assert_eq!(
            vocabulary.declare("", &[]),
            Err(VocabError::EmptyCanonicalTerm)
        );
        assert!(matches!(
            vocabulary.declare("alpha", &[&[][..]]),
            Err(VocabError::EmptySurfaceForm { .. })
        ));
        let long = ["t"; MAX_SURFACE_TERMS + 1];
        assert!(matches!(
            vocabulary.declare("alpha", &[&long[..]]),
            Err(VocabError::SurfaceFormTooLong { .. })
        ));
    }

    #[test]
    fn redeclaring_the_same_pair_is_idempotent() {
        let mut vocabulary = Vocabulary::new();
        vocabulary
            .declare("alpha", &[&["a", "b"][..]])
            .expect("valid declaration");
        vocabulary
            .declare("alpha", &[&["a", "b"][..]])
            .expect("redeclaration of the same pair is allowed");
        assert_eq!(vocabulary.len(), 2);
    }
}
