//! Small ownership boundary around the selected compressed-bitmap crate.

use roaring::RoaringBitmap;

/// A compressed set of segment-local document identifiers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DocBitmap {
    inner: RoaringBitmap,
}

impl Eq for DocBitmap {}

impl DocBitmap {
    /// Creates an empty bitmap.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates the document universe `0..row_count`.
    #[must_use]
    pub fn full(row_count: u32) -> Self {
        Self {
            inner: (0..row_count).collect(),
        }
    }

    /// Creates a bitmap from document identifiers.
    #[must_use]
    pub fn from_ids(ids: impl IntoIterator<Item = u32>) -> Self {
        Self {
            inner: ids.into_iter().collect(),
        }
    }

    /// Inserts one document identifier and reports whether it was new.
    pub fn insert(&mut self, document: u32) -> bool {
        self.inner.insert(document)
    }

    /// Removes one document identifier and reports whether it was present.
    pub fn remove(&mut self, document: u32) -> bool {
        self.inner.remove(document)
    }

    /// Returns whether the document identifier is present.
    #[must_use]
    pub fn contains(&self, document: u32) -> bool {
        self.inner.contains(document)
    }

    /// Returns the exact bitmap cardinality.
    #[must_use]
    pub fn cardinality(&self) -> u64 {
        self.inner.len()
    }

    /// Returns whether the bitmap contains no identifiers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Iterates identifiers in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.inner.iter()
    }

    /// Mutates this bitmap to its union with `other`.
    pub fn union_with(&mut self, other: &Self) {
        self.inner |= &other.inner;
    }

    /// Mutates this bitmap to its intersection with `other`.
    pub fn intersect_with(&mut self, other: &Self) {
        self.inner &= &other.inner;
    }

    /// Removes every identifier present in `other`.
    pub fn subtract(&mut self, other: &Self) {
        self.inner -= &other.inner;
    }

    /// Returns whether every identifier is also present in `other`.
    #[must_use]
    pub fn is_subset(&self, other: &Self) -> bool {
        self.inner.is_subset(&other.inner)
    }
}
