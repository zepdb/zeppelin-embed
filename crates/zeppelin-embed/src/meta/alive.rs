//! Per-segment alive and tombstone sets with exact counters.

use super::bitmap::DocBitmap;

/// A tombstone operation referred outside the segment document-id space.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AliveError {
    document: u32,
    row_count: u32,
}

impl AliveError {
    /// Returns the invalid document identifier.
    #[must_use]
    pub const fn document(self) -> u32 {
        self.document
    }

    /// Returns the segment row count that rejected it.
    #[must_use]
    pub const fn row_count(self) -> u32 {
        self.row_count
    }
}

impl std::fmt::Display for AliveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "document {} is outside segment row count {}",
            self.document, self.row_count
        )
    }
}

impl std::error::Error for AliveError {}

/// Disjoint per-segment alive and tombstone bitmaps.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AliveSet {
    row_count: u32,
    alive: DocBitmap,
    tombstones: DocBitmap,
}

impl AliveSet {
    /// Creates a segment state in which every document is alive.
    #[must_use]
    pub fn new(row_count: u32) -> Self {
        let state = Self {
            row_count,
            alive: DocBitmap::full(row_count),
            tombstones: DocBitmap::new(),
        };
        state.debug_assert_consistent();
        state
    }

    /// Returns the immutable segment row count.
    #[must_use]
    pub const fn row_count(&self) -> u32 {
        self.row_count
    }

    /// Marks one live document as tombstoned.
    ///
    /// Returns `true` only for the first tombstone of that identifier.
    pub fn tombstone(&mut self, document: u32) -> Result<bool, AliveError> {
        if document >= self.row_count {
            return Err(AliveError {
                document,
                row_count: self.row_count,
            });
        }
        let changed = self.alive.remove(document);
        if changed {
            self.tombstones.insert(document);
        }
        self.debug_assert_consistent();
        Ok(changed)
    }

    /// Returns whether a document is currently alive.
    #[must_use]
    pub fn is_alive(&self, document: u32) -> bool {
        self.alive.contains(document)
    }

    /// Returns the live-document bitmap.
    #[must_use]
    pub fn alive_bitmap(&self) -> &DocBitmap {
        &self.alive
    }

    /// Returns the tombstone bitmap.
    #[must_use]
    pub fn tombstone_bitmap(&self) -> &DocBitmap {
        &self.tombstones
    }

    /// Returns the exact live-document count.
    #[must_use]
    pub fn live_count(&self) -> u64 {
        self.alive.cardinality()
    }

    /// Returns the exact tombstone count.
    #[must_use]
    pub fn tombstone_count(&self) -> u64 {
        self.tombstones.cardinality()
    }

    /// Iterates live document identifiers in ascending order.
    pub fn iter_alive(&self) -> impl Iterator<Item = u32> + '_ {
        self.alive.iter()
    }

    /// Audits the disjoint-union counter invariant in debug builds.
    pub fn debug_assert_consistent(&self) {
        debug_assert_eq!(
            self.alive.cardinality() + self.tombstones.cardinality(),
            u64::from(self.row_count)
        );
        debug_assert!(self.alive.iter().all(|id| !self.tombstones.contains(id)));
        debug_assert!(self.tombstones.iter().all(|id| id < self.row_count));
    }
}

#[allow(clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tombstone_then_alive_iteration_skips() {
        let mut alive = AliveSet::new(4);
        alive.tombstone(1).expect("row exists");
        assert_eq!(alive.iter_alive().collect::<Vec<_>>(), vec![0, 2, 3]);
    }

    #[test]
    fn alive_counters_remain_consistent() {
        let mut alive = AliveSet::new(4);
        alive.tombstone(1).expect("row exists");
        alive.tombstone(1).expect("idempotent tombstone");
        alive.debug_assert_consistent();
        assert_eq!(alive.tombstone_count(), 1);
        assert_eq!(alive.live_count(), 3);
        assert_eq!(
            alive.tombstone_count(),
            alive.tombstone_bitmap().cardinality()
        );
        assert_eq!(alive.live_count(), alive.alive_bitmap().cardinality());
    }
}
