//! ZE-52 slice B: lazy target loading in the native admitted base.
//!
//! `NativeAdmittedBase::new` sizes its record cache from the structured-write
//! list and preloads exactly the entities that list names, so a cache miss is
//! `None`. A query-driven mutation executor discovers its targets from a MATCH
//! clause instead, at query time, and has no such list. These tests pin the
//! second admission shape: `with_lazy_targets` keeps the same preload and adds
//! a bounded fallthrough to the admitted roots, while `new` is unchanged.

#![cfg(test)]
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use super::super::base::{MAX_LAZY_TARGETS, NativeAdmittedBase};
use super::*;
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::property_graph::EntityShape;
use crate::property_graph::storage::NativePreparationSource;

const LISTED_TEXT: &str = "listed node text";
const UNLISTED_TEXT: &str = "unlisted node text\u{0}tail";
const EXTRA_TEXT: &str = "";

/// A node id no commit in this fixture ever allocates.
fn absent_node() -> NodeId {
    NodeId::new(0x5a5a_0000_0000_0000_0000_0000_dead_beef).expect("absent node id")
}

fn fixture_options() -> OpenOptions {
    OpenOptions::new()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024)
}

fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

/// Three committed nodes. Only `listed` is ever named by a structured-write
/// list, so `unlisted` and `extra` can only be reached lazily.
struct LazyFixture {
    _directory: super::tempfile::TempDir,
    store: Store,
    listed: NodeId,
    unlisted: NodeId,
    extra: NodeId,
}

impl LazyFixture {
    fn commit() -> Self {
        let directory = super::tempfile::tempdir().expect("temporary parent");
        let path = directory.path().join("base-lazy");
        let store = Store::create_native_graph(&path, fixture_options(), None)
            .expect("create native store");
        let listed = commit_node(&store, "listed", LISTED_TEXT);
        let unlisted = commit_node(&store, "unlisted", UNLISTED_TEXT);
        let extra = commit_node(&store, "extra", EXTRA_TEXT);
        assert_ne!(listed, unlisted);
        assert_ne!(unlisted, extra);
        Self {
            _directory: directory,
            store,
            listed,
            unlisted,
            extra,
        }
    }

    /// The one structured write `new` is given, naming `listed` only.
    fn listed_request(&self) -> [StructuredWrite<'static, 'static>; 1] {
        [StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", "listed").expect("listed key"),
            revision: GraphRevision::new(2).expect("listed revision"),
            operation: StructuredOperation::Put(EntityId::Node(self.listed)),
            image: None,
        }]
    }
}

/// One committed node carrying the shared property fixture and its own text.
fn commit_node(store: &Store, key: &str, text: &str) -> NodeId {
    let mut labels = [GraphName::new("Document").expect("label")];
    let mut properties = super::publication::property_fixture();
    let image = CanonicalContents::node(&mut labels, &mut properties, Some(text), None)
        .expect("node image");
    let receipts = store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", key).expect("node key"),
                revision: GraphRevision::new(1).expect("node revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &control(),
        )
        .unwrap_or_else(|error| panic!("commit {key}: {error:?}"));
    match receipts[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("commit {key} returned a relationship"),
    }
}

/// Everything one admitted base reports about one node, owned so two bases can
/// be compared after both have been dropped.
#[derive(Debug, Eq, PartialEq)]
struct Observation {
    incarnation: u128,
    revision: u64,
    fingerprint: (u64, u64),
    membership: (bool, bool),
    node_shape: bool,
    text: Option<String>,
    properties: Vec<(String, String)>,
}

fn observe(base: &NativeAdmittedBase<'_, '_, '_, '_>, node: NodeId) -> Observation {
    let entity = base
        .entity(EntityId::Node(node), &mut |_| Ok(()))
        .expect("entity lookup")
        .expect("live entity");
    let fields = entity.provenance.fields();
    let text = base
        .stored_text(node, &mut |_| Ok(()))
        .expect("stored text")
        .map(str::to_owned);
    let properties = super::publication::property_fixture()
        .iter()
        .map(|property| {
            let value = base
                .property(EntityId::Node(node), property.name(), &mut |_| Ok(()))
                .expect("property lookup")
                .expect("present property");
            (
                property.name().as_str().to_owned(),
                format!("{:?}", value.data()),
            )
        })
        .collect();
    Observation {
        incarnation: match fields.incarnation {
            EntityId::Node(id) => id.get(),
            EntityId::Relationship(id) => id.get(),
        },
        revision: fields.installed_revision.get(),
        fingerprint: (entity.fingerprint.bytes(), entity.fingerprint.hash()),
        membership: (entity.membership.text, entity.membership.vector),
        node_shape: matches!(entity.shape, EntityShape::Node),
        text,
        properties,
    }
}

/// Builds one admitted base over the committed store and runs `check` on it.
/// `lazy` selects the constructor: `None` is the structured-write `new`.
fn with_base<T>(
    store: &Store,
    requests: &[StructuredWrite<'_, '_>],
    lazy: Option<usize>,
    check: impl FnOnce(&NativeAdmittedBase<'_, '_, '_, '_>) -> T,
) -> T {
    let lease = store.admit_native_read().expect("native read lease");
    let shared = GraphResources::from_store(store).expect("graph resources");
    let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("write memory");
    let control = control();
    let storage = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).expect("storage memory");
    let source = NativePreparationSource::new(&lease, &storage, 32).expect("preparation source");
    let mut resources = source.resources(64 * 1024 * 1024).expect("tree resources");
    let cell = std::cell::RefCell::new(&mut resources);
    let error = std::cell::Cell::new(None);
    let base = match lazy {
        None => NativeAdmittedBase::new(&lease, &source, &storage, requests, &cell, &error),
        Some(capacity) => NativeAdmittedBase::with_lazy_targets(
            &lease, &source, &storage, requests, &cell, &error, capacity,
        ),
    }
    .expect("admitted base");
    check(&base)
}

/// Constructs a lazy base with `capacity` and reports only whether admission
/// succeeded, so an over-large arena request can be proved to be refused.
fn admit_lazy_capacity(store: &Store, capacity: usize) -> Result<(), NativeGraphError> {
    let lease = store.admit_native_read().expect("native read lease");
    let shared = GraphResources::from_store(store).expect("graph resources");
    let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("write memory");
    let control = control();
    let storage = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).expect("storage memory");
    let source = NativePreparationSource::new(&lease, &storage, 32).expect("preparation source");
    let mut resources = source.resources(64 * 1024 * 1024).expect("tree resources");
    let cell = std::cell::RefCell::new(&mut resources);
    let error = std::cell::Cell::new(None);
    NativeAdmittedBase::with_lazy_targets(&lease, &source, &storage, &[], &cell, &error, capacity)
        .map(|_| ())
}

/// A base built from an empty structured-write list still answers for a node
/// that was published earlier, through the lazy fallthrough only.
#[test]
fn ze52_slice_b_lazy_base_resolves_an_unlisted_target() {
    let fixture = LazyFixture::commit();
    let node = fixture.unlisted;

    // The structured-write admission is the control: with no request naming
    // this node, every reader reports absence.
    with_base(&fixture.store, &[], None, |base| {
        assert!(
            base.entity(EntityId::Node(node), &mut |_| Ok(()))
                .expect("preload entity")
                .is_none()
        );
        assert!(
            base.property(
                EntityId::Node(node),
                GraphName::new("i").expect("property name"),
                &mut |_| Ok(())
            )
            .expect("preload property")
            .is_none()
        );
        assert!(
            base.stored_text(node, &mut |_| Ok(()))
                .expect("preload text")
                .is_none()
        );
    });

    with_base(&fixture.store, &[], Some(4), |base| {
        let entity = base
            .entity(EntityId::Node(node), &mut |_| Ok(()))
            .expect("lazy entity")
            .expect("published node resolves through the lazy fallthrough");
        assert!(matches!(entity.shape, EntityShape::Node));
        assert_eq!(entity.provenance.fields().incarnation, EntityId::Node(node));
        assert_eq!(entity.view, base.identity());
        assert_eq!(
            base.stored_text(node, &mut |_| Ok(())).expect("lazy text"),
            Some(UNLISTED_TEXT)
        );
        let value = base
            .property(
                EntityId::Node(node),
                GraphName::new("i").expect("property name"),
                &mut |_| Ok(()),
            )
            .expect("lazy property")
            .expect("present property");
        assert!(matches!(value.data(), PropertyData::I64(i64::MIN)));
        // Absence inside a resolved entity is still absence.
        assert!(
            base.property(
                EntityId::Node(node),
                GraphName::new("missing").expect("property name"),
                &mut |_| Ok(())
            )
            .expect("lazy missing property")
            .is_none()
        );
    });

    fixture.store.close().expect("close native store");
}

/// The lazy arena is admitted once and never grows: a resolved target is
/// cached, an absent one charges nothing, and one target past the bound is a
/// typed refusal rather than unbounded growth.
#[test]
fn ze52_slice_b_lazy_targets_are_bounded() {
    let fixture = LazyFixture::commit();

    with_base(&fixture.store, &[], Some(1), |base| {
        // An absent target resolves to `None` without consuming the one slot.
        assert!(
            base.entity(EntityId::Node(absent_node()), &mut |_| Ok(()))
                .expect("absent entity")
                .is_none()
        );

        assert!(
            base.entity(EntityId::Node(fixture.unlisted), &mut |_| Ok(()))
                .expect("first lazy entity")
                .is_some()
        );
        // The second read of the same target is a cache hit; a second
        // insertion would already have exhausted the single slot.
        assert!(
            base.entity(EntityId::Node(fixture.unlisted), &mut |_| Ok(()))
                .expect("cached lazy entity")
                .is_some()
        );
        assert_eq!(
            base.stored_text(fixture.unlisted, &mut |_| Ok(()))
                .expect("cached lazy text"),
            Some(UNLISTED_TEXT)
        );

        // A second distinct target exceeds the admitted arena and is refused.
        let refused = base
            .entity(EntityId::Node(fixture.extra), &mut |_| Ok(()))
            .map(|entity| entity.is_some());
        assert!(
            matches!(refused, Err(StageError::NativeStorage(TreeError::Memory))),
            "second distinct lazy target: {refused:?}"
        );

        // The refusal left the arena usable and unchanged.
        assert!(
            base.entity(EntityId::Node(fixture.unlisted), &mut |_| Ok(()))
                .expect("lazy entity after refusal")
                .is_some()
        );
    });

    // A caller cannot ask for an arena larger than the admitted ceiling.
    assert!(admit_lazy_capacity(&fixture.store, MAX_LAZY_TARGETS).is_ok());
    let over = admit_lazy_capacity(&fixture.store, MAX_LAZY_TARGETS + 1);
    assert!(
        matches!(over, Err(NativeGraphError::Invalid(_))),
        "over-large lazy arena: {over:?}"
    );

    fixture.store.close().expect("close native store");
}

/// `new(requests)` keeps its exact structured-write behaviour: it answers for
/// every listed target and reports absence for everything else.
#[test]
fn ze52_slice_b_structured_preload_is_unchanged() {
    let fixture = LazyFixture::commit();
    let request = fixture.listed_request();

    with_base(&fixture.store, &request, None, |base| {
        let observed = observe(base, fixture.listed);
        assert_eq!(observed.incarnation, fixture.listed.get());
        assert_eq!(observed.text.as_deref(), Some(LISTED_TEXT));
        assert_eq!(observed.properties.len(), 9);
        assert!(observed.node_shape);

        assert!(matches!(
            base.key(request[0].key, &mut |_| Ok(()))
                .expect("key state"),
            BaseKeyState::Live(_)
        ));

        // Nothing the request did not name is reachable from this base.
        for unnamed in [fixture.unlisted, fixture.extra] {
            assert!(
                base.entity(EntityId::Node(unnamed), &mut |_| Ok(()))
                    .expect("unnamed entity")
                    .is_none()
            );
            assert!(
                base.property(
                    EntityId::Node(unnamed),
                    GraphName::new("i").expect("property name"),
                    &mut |_| Ok(())
                )
                .expect("unnamed property")
                .is_none()
            );
            assert!(
                base.stored_text(unnamed, &mut |_| Ok(()))
                    .expect("unnamed text")
                    .is_none()
            );
        }
    });

    fixture.store.close().expect("close native store");
}

/// The two loading paths agree: a lazily resolved target reports exactly what
/// the structured-write preload reports for the same entity.
#[test]
fn ze52_slice_b_lazy_and_preload_agree() {
    let fixture = LazyFixture::commit();
    let request = fixture.listed_request();

    let preloaded = with_base(&fixture.store, &request, None, |base| {
        observe(base, fixture.listed)
    });
    let lazily = with_base(&fixture.store, &[], Some(4), |base| {
        observe(base, fixture.listed)
    });
    assert_eq!(preloaded, lazily);

    // The preload still wins when both paths could answer, and the lazy arena
    // is not consulted for a target the request already named: one slot is
    // enough for the unlisted node alone.
    let both = with_base(&fixture.store, &request, Some(1), |base| {
        let listed = observe(base, fixture.listed);
        let unlisted = observe(base, fixture.unlisted);
        (listed, unlisted)
    });
    assert_eq!(both.0, preloaded);
    assert_eq!(both.1.text.as_deref(), Some(UNLISTED_TEXT));
    assert_eq!(both.1.properties, preloaded.properties);

    fixture.store.close().expect("close native store");
}
