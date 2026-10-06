//! Checked immutable graph containers. These codecs do not admit a GraphStore,
//! publish roots, replay a WAL, or authorize cleanup.

pub mod allocation;
pub mod artifact;
pub mod tree;

/// Bounded immutable adjacency codecs and range merging.
pub mod adjacency;
pub(crate) mod consolidation;

/// Bounded, role-checked logical streams over immutable physical chunks.
pub mod payload;

/// Retained logical windows and bounded field decoding over payload extents.
pub mod stream;

/// Lossless logical records and their required semantic validation hooks.
pub mod records;

mod view;
#[cfg(feature = "graph-cypher")]
#[allow(
    unused_imports,
    reason = "crate-private ZE-45 interface is consumed by ZE-50 after this dependency lands"
)]
pub(crate) use view::{
    CursorState, DirectionSelection, ExpandCursor, GraphPreparation, GraphReadView, LabelSelection,
    NativeArtifactWindow, NativeCatalog, NativePreparationCatalog, NativePreparationSource,
    NativeQuerySource, NativeReadCapability, NativeReadonlyMapping, NodeCursor, NodeView,
    PreparedGraphArtifacts, PreparedGraphFailure, RelView, RelationshipTypeSelection,
    TextPayloadReader,
};

/// Combined private storage capacity inside the authentic writer/store owner.
pub mod memory;

/// Packed private artifacts and explicit owned abort inventories.
pub mod prepared;

/// Persisted immutable allocation descriptors and reclamation state.
pub mod inventory;

/// Bounded protected-root, completed-mark, and reclaim-state proofs.
pub(crate) mod reclaim;

/// Native directory changes from one authentic normalized writer batch.
pub mod participant;

/// Sparse text/vector retrieval participants over native graph records.
#[cfg(feature = "graph-cypher")]
pub(crate) mod search;

/// Current durable recovery inventory bound. Native sources must be able to
/// address that inventory; 64 mapping slots imposed a smaller accidental store
/// limit. Descriptors are charged to the existing query/storage owner; payloads
/// remain immutable read-only mappings rather than copied graph rows.
pub(crate) const MAX_NATIVE_ARTIFACTS: usize = 8_192;

/// Locate a retained immutable mapping or an empty slot without allocating.
pub(crate) fn mapping_slot<T>(
    slots: &[std::cell::OnceCell<T>],
    artifact: artifact::ArtifactId,
    identity: impl Fn(&T) -> artifact::ArtifactId,
    mut step: impl FnMut() -> Result<(), tree::directory::TreeError>,
) -> Result<Option<&std::cell::OnceCell<T>>, tree::directory::TreeError> {
    let start = (xxhash_rust::xxh3::xxh3_64(&artifact.get().to_le_bytes()) as usize)
        .checked_rem(slots.len())
        .ok_or(tree::directory::TreeError::Memory)?;
    for index in (start..slots.len()).chain(0..start) {
        step()?;
        let slot = slots.get(index).ok_or(tree::directory::TreeError::Memory)?;
        if let Some(value) = slot.get() {
            if identity(value) == artifact {
                return Ok(Some(slot));
            }
        } else {
            return Ok(Some(slot));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod mapping_index_tests {
    #[test]
    #[allow(clippy::unwrap_used, reason = "test fixture failures are assertions")]
    fn ze257_mapping_lookup_work_does_not_scan_the_retained_inventory() {
        use super::{artifact::ArtifactId, mapping_slot};
        use std::cell::OnceCell;
        let slots: Vec<_> = (0..8192).map(|_| OnceCell::new()).collect();
        let mut probes = 0_u64;
        for index in 1..=4096 {
            let id = ArtifactId::new(index).unwrap();
            mapping_slot(
                &slots,
                id,
                |value| *value,
                || {
                    probes += 1;
                    Ok(())
                },
            )
            .unwrap()
            .unwrap()
            .set(id)
            .unwrap();
        }
        for index in (1..=4096).rev() {
            let id = ArtifactId::new(index).unwrap();
            let found = mapping_slot(
                &slots,
                id,
                |value| *value,
                || {
                    probes += 1;
                    Ok(())
                },
            )
            .unwrap()
            .unwrap()
            .get()
            .copied();
            assert_eq!(found, Some(id));
        }
        eprintln!("ZE257 mapping probes={probes} for 4096 inserts and reverse lookups");
        assert!(probes < 4096 * 8, "mapping table probes: {probes}");
    }
}

// Private fixture controls; descriptor and auxiliary source capacities stay intact.
#[cfg(all(any(test, feature = "test-seams"), feature = "graph-cypher"))]
pub(crate) mod mapping_slot_capture {
    #![allow(clippy::expect_used, clippy::panic)]
    use std::cell::RefCell;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum Kind {
        Preparation,
        Recovery,
    }
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct Report {
        pub kind: Kind,
        pub generation: u64,
        pub capacity: usize,
        pub filled: usize,
        pub post_exhaustion_resolves: usize,
        pub opens: usize,
        pub scoped_opens: usize,
    }
    pub(crate) struct Observation(std::cell::Cell<Report>);
    impl Observation {
        pub(crate) fn new(kind: Kind, generation: u64, capacity: usize) -> Self {
            Self(std::cell::Cell::new(Report {
                kind,
                generation,
                capacity,
                filled: 0,
                post_exhaustion_resolves: 0,
                opens: 0,
                scoped_opens: 0,
            }))
        }
        pub(crate) fn report(&self) -> Report {
            self.0.get()
        }
        pub(crate) fn filled(&self, filled: usize) {
            self.0.set(Report {
                filled,
                ..self.report()
            });
        }
        pub(crate) fn open(&self, scoped: bool) {
            let report = self.report();
            self.0.set(Report {
                opens: report.opens + 1,
                scoped_opens: report.scoped_opens + usize::from(scoped),
                ..report
            });
        }
        pub(crate) fn resolve(&self, exhausted: bool) {
            if exhausted {
                self.0.set(Report {
                    post_exhaustion_resolves: self.report().post_exhaustion_resolves + 1,
                    ..self.report()
                });
            }
        }
    }
    impl Drop for Observation {
        fn drop(&mut self) {
            publish(self.report());
        }
    }
    thread_local! {
        static CAPTURE: RefCell<Option<Vec<Report>>> = const { RefCell::new(None) };
        static DEFAULT_CAPACITY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    #[cfg(test)]
    #[derive(Debug)]
    pub(crate) struct VerifyReport {
        pub path: &'static str,
        pub kind: super::tree::TreeKind,
        pub entries: usize,
        pub calls: usize,
        pub work: u64,
        pub nanos: u128,
    }
    #[cfg(test)]
    thread_local! {
        static VERIFY: RefCell<Vec<VerifyReport>> = const { RefCell::new(Vec::new()) };
        static SPLITS: RefCell<Vec<(super::tree::TreeKind, usize)>> = const { RefCell::new(Vec::new()) };
    }
    #[cfg(test)]
    pub(crate) fn begin_verify(work: u64) -> Option<(std::time::Instant, u64)> {
        CAPTURE
            .with(|capture| capture.borrow().is_some())
            .then(|| (std::time::Instant::now(), work))
    }
    #[cfg(test)]
    pub(crate) fn end_verify(
        start: Option<(std::time::Instant, u64)>,
        path: &'static str,
        kind: super::tree::TreeKind,
        entries: usize,
        calls: usize,
        work: u64,
    ) {
        if let Some((started, before)) = start {
            let nanos = started.elapsed().as_nanos();
            VERIFY.with(|reports| {
                reports.borrow_mut().push(VerifyReport {
                    path,
                    kind,
                    entries,
                    calls,
                    work: work - before,
                    nanos,
                })
            });
        }
    }
    #[cfg(test)]
    pub(crate) fn leaf_split(kind: super::tree::TreeKind, pages: usize) {
        if pages > 1 && CAPTURE.with(|capture| capture.borrow().is_some()) {
            SPLITS.with(|reports| reports.borrow_mut().push((kind, pages)));
        }
    }
    pub(crate) struct Capture;
    impl Capture {
        pub(crate) fn start() -> Self {
            CAPTURE.with(|capture| {
                assert!(capture.borrow_mut().replace(Vec::new()).is_none());
            });
            Self
        }
        pub(crate) fn take(&self) -> Vec<Report> {
            CAPTURE.with(|capture| {
                std::mem::take(capture.borrow_mut().as_mut().expect("active capture"))
            })
        }
        #[cfg(test)]
        pub(crate) fn take_verify(&self) -> Vec<VerifyReport> {
            VERIFY.with(|reports| std::mem::take(&mut *reports.borrow_mut()))
        }
        #[cfg(test)]
        pub(crate) fn take_splits(&self) -> Vec<(super::tree::TreeKind, usize)> {
            SPLITS.with(|reports| std::mem::take(&mut *reports.borrow_mut()))
        }
        pub(crate) fn restore_default_capacity(&self) {
            DEFAULT_CAPACITY.with(|default| default.set(true));
        }
    }
    impl Drop for Capture {
        fn drop(&mut self) {
            #[cfg(test)]
            VERIFY.with(|reports| reports.borrow_mut().clear());
            #[cfg(test)]
            SPLITS.with(|reports| reports.borrow_mut().clear());
            DEFAULT_CAPACITY.with(|default| default.set(false));
            CAPTURE.with(|capture| {
                *capture.borrow_mut() = None;
            });
        }
    }
    pub(crate) fn capacity(original: usize) -> usize {
        CAPTURE.with(|capture| {
            if original > 4
                && capture.borrow().is_some()
                && !DEFAULT_CAPACITY.with(std::cell::Cell::get)
            {
                64
            } else {
                original
            }
        })
    }
    pub(crate) fn publish(report: Report) {
        CAPTURE.with(|capture| {
            if let Some(reports) = capture.borrow_mut().as_mut() {
                reports.push(report);
            }
        });
    }
}

#[cfg(all(test, feature = "graph-cypher"))]
pub(crate) mod preparation_work_capture;
