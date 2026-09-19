//! PG8 real staged storage participant, finalized-file reopening and primitive
//! oracle comparison. Fixture base/catalog are not GraphStore admission proof.
use super::coverage::CoverageRegistry;
use rand::RngCore;
use std::cell::Cell;
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::property_graph::storage::{
    artifact::*,
    memory::{StorageBuffer, StorageMemory},
    participant::{DirectoryBase, PreparationCatalog, prepare_directories},
    prepared::{PackLimits, PreparedObjects},
    records::*,
    stream::PayloadSlice,
    tree::{TreeKind, directory::*},
};
use zeppelin_embed::property_graph::{catalog::*, resources::GraphResources, staging::*, *};
use zeppelin_embed_adversarial_oracle::graph_directory as model;
mod fixture;
mod observe;
pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.directories.native-history",
    "property-graph.directories.old-root",
    "property-graph.directories.reopen",
    "property-graph.directories.range",
    "property-graph.directories.detach",
    "property-graph.directories.recreate",
    "property-graph.directories.oracle.can-fire",
    "property-graph.directories.append.fire",
    "property-graph.directories.append.clean",
    "property-graph.directories.read.fire",
    "property-graph.directories.read.clean",
    "property-graph.directories.cancel.fire",
    "property-graph.directories.cancel.clean",
    "property-graph.directories.budget.fire",
    "property-graph.directories.budget.clean",
    "property-graph.directories.private-reuse",
];
struct Files<'a> {
    objects: StorageBuffer<'a, OwnedArtifact<'a>>,
}
impl BlockSource for Files<'_> {
    fn resolve<'a>(
        &'a self,
        reference: PhysicalRef,
        r: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        self.objects
            .as_slice()
            .iter()
            .find(|object| object.identity().artifact == reference.artifact)
            .ok_or(TreeError::Missing)?
            .resolve(reference, r)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Fault {
    None,
    Append(usize),
    Read(usize),
    Cancel(usize),
    Budget,
}
struct Scheduled<'a, S> {
    inner: &'a mut S,
    fault: Fault,
    reads: Cell<usize>,
    writes: usize,
    fired: Cell<bool>,
    cancel: CancelToken,
}
impl<S: BlockSource> BlockSource for Scheduled<'_, S> {
    fn resolve<'a>(
        &'a self,
        reference: PhysicalRef,
        r: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        self.reads.set(self.reads.get() + 1);
        if self.fault == Fault::Read(self.reads.get()) {
            self.fired.set(true);
            return Err(TreeError::Missing);
        }
        self.inner.resolve(reference, r)
    }
}
impl<S: BlockSink> BlockSink for Scheduled<'_, S> {
    fn append(
        &mut self,
        kind: BlockKind,
        generation: GraphGeneration,
        bytes: &[u8],
        r: &mut TreeResources<'_>,
    ) -> Result<PhysicalRef, TreeError> {
        self.writes += 1;
        if self.fault == Fault::Append(self.writes) {
            self.fired.set(true);
            return Err(TreeError::Missing);
        }
        if self.fault == Fault::Cancel(self.writes) {
            self.fired.set(true);
            self.cancel.cancel();
        }
        self.inner.append(kind, generation, bytes, r)
    }
}
struct Run {
    observations: Vec<model::Observation>,
    fired: bool,
    append_calls: usize,
    read_calls: usize,
    reuse_checks: usize,
}
fn run(seed: u64, fault: Fault) -> Result<Run, String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let store = Store::open(
        directory.path().join("store"),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .map_err(|error| error.to_string())?;
    let shared = GraphResources::from_store(&store).map_err(|error| error.to_string())?;
    let shared_before = shared.reserved_bytes().map_err(|error| error.to_string())?;
    let writer =
        WriteMemory::new(&shared, WriteLimits::default()).map_err(|error| error.to_string())?;
    let cancel = CancelToken::new();
    let control = QueryControl::Cancel(cancel.clone());
    let mut output = Run {
        observations: Vec::new(),
        fired: false,
        append_calls: 0,
        read_calls: 0,
        reuse_checks: 0,
    };
    {
        let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024)
            .map_err(|error| error.to_string())?;
        let mut files = Files {
            objects: StorageBuffer::new(&memory, 64).map_err(|error| error.to_string())?,
        };
        let mut base = fixture::Base::empty();
        let mut roots =
            GraphRoots::from_references(base.identity().store, GraphGeneration::new(0), [None; 8])
                .unwrap();
        let mut model = model::Model::new(model::Limits {
            max_image_bytes: 256 * 1024,
            ..Default::default()
        });
        let mut saved = Vec::new();
        for (index, request) in fixture::trace(seed).iter().enumerate() {
            let generation = index as u64 + 1;
            let mut labels: Vec<_> = request
                .labels
                .iter()
                .rev()
                .map(|name| GraphName::new(name).unwrap())
                .collect();
            let mut properties = [GraphProperty::new(
                GraphName::new("bits").unwrap(),
                PropertyValue::new(PropertyData::F64(f64::from_bits(request.bits))).unwrap(),
            )];
            let relationship_properties = properties;
            let image = if request.relationship {
                CanonicalContents::relationship(
                    NodeId::new(fixture::NODE_HIGH + 1).unwrap(),
                    NodeId::new(fixture::NODE_HIGH + 2).unwrap(),
                    GraphName::new("R").unwrap(),
                    &mut properties,
                )
                .unwrap()
            } else {
                CanonicalContents::node(&mut labels, &mut properties, request.text.as_deref(), None)
                    .unwrap()
            };
            let fingerprint = image.fingerprint(&mut || Ok(())).unwrap();
            let write = StructuredWrite {
                key: request.key(),
                revision: GraphRevision::new(request.revision).unwrap(),
                operation: request.operation(),
                image: if request.kind == model::OperationKind::Delete {
                    None
                } else if request.relationship {
                    Some(WriteImage::Relationship {
                        source: NodeRef::Existing(NodeId::new(fixture::NODE_HIGH + 1).unwrap()),
                        target: NodeRef::Existing(NodeId::new(fixture::NODE_HIGH + 2).unwrap()),
                        relationship_type: GraphName::new("R").unwrap(),
                        properties: &relationship_properties,
                    })
                } else {
                    Some(WriteImage::Node(&image))
                },
            };
            let batch = stage_structured(&base, &[write], &writer, &mut |_| Ok(()))
                .map_err(|error| error.to_string())?;
            let active_fault = if generation == 4 { fault } else { Fault::None };
            let mut r = TreeResources::for_prepare(
                &memory,
                if active_fault == Fault::Budget {
                    1
                } else {
                    100_000_000
                },
            )
            .map_err(|error| error.to_string())?;
            let before = memory.reserved_bytes();
            let mut serial = generation * 1000;
            let mut objects = PreparedObjects::new(
                &files,
                || {
                    serial += 1;
                    Ok(ArtifactIdentity {
                        store: base.identity().store,
                        artifact: ArtifactId::new(serial as u128).unwrap(),
                        generation: GraphGeneration::new(generation),
                        creation_serial: serial,
                    })
                },
                base.identity().store,
                GraphGeneration::new(generation),
                PackLimits {
                    artifact_bytes: 512 * 1024,
                    blocks: 256,
                },
                &memory,
                &mut r,
            )
            .map_err(|error| error.to_string())?;
            let mut schedule = Scheduled {
                inner: &mut objects,
                fault: active_fault,
                reads: Cell::new(0),
                writes: 0,
                fired: Cell::new(false),
                cancel: cancel.clone(),
            };
            let candidate = prepare_directories(
                &mut schedule,
                &batch,
                DirectoryBase {
                    identity: base.identity(),
                    roots,
                },
                &base,
                None,
                &memory,
                &mut r,
            );
            let fired = schedule.fired.get()
                || (active_fault == Fault::Budget && matches!(candidate, Err(TreeError::Work)));
            if generation == 4 {
                output.append_calls = schedule.writes;
                output.read_calls = schedule.reads.get();
            }
            drop(schedule);
            match candidate {
                Ok(candidate) => {
                    if active_fault != Fault::None {
                        return Err(format!(
                            "PG8 scheduled fault failed to fire: {active_fault:?}"
                        ));
                    }
                    roots = candidate.roots();
                    let reference = roots
                        .directory(TreeKind::KeyFences)
                        .unwrap()
                        .reference()
                        .unwrap();
                    assert!(
                        objects
                            .abort_inventory()
                            .any(|identity| identity.artifact == reference.artifact),
                        "PG8 private reuse receipt requires current private backing"
                    );
                    let read_start = r.work();
                    let capacity = memory.reserved_bytes();
                    for _ in 0..8 {
                        let block = objects
                            .resolve(reference, &mut r)
                            .map_err(|error| error.to_string())?;
                        assert_eq!(block.reference(), reference);
                    }
                    for changed in [
                        PhysicalRef {
                            kind: BlockKind::NodeRecord,
                            ..reference
                        },
                        PhysicalRef {
                            length: reference.length - 1,
                            ..reference
                        },
                    ] {
                        assert!(
                            objects.resolve(changed, &mut r).is_err(),
                            "PG8 full reference substitution"
                        );
                    }
                    assert_eq!(memory.reserved_bytes(), capacity);
                    assert!(
                        r.work() - read_start <= 8192,
                        "PG8 repeated private reads must reuse complete immutable admission"
                    );
                    output.reuse_checks += 1;

                    objects.finish(&mut r).map_err(|error| error.to_string())?;
                    let mut receipts = Vec::new();
                    for position in 0..objects.len() {
                        let artifact = objects
                            .artifact(position)
                            .map_err(|error| error.to_string())?;
                        let path = directory
                            .path()
                            .join(format!("object-{}", artifact.identity().artifact.get()));
                        std::fs::write(&path, artifact.bytes())
                            .map_err(|error| error.to_string())?;
                        receipts.push((path, artifact.identity(), artifact.bytes().len()));
                    }
                    drop(candidate);
                    drop(objects);
                    assert_eq!(
                        memory.reserved_bytes(),
                        before,
                        "private preparation backing released before physical reopen"
                    );
                    for (path, identity, length) in receipts {
                        let mut file =
                            std::fs::File::open(path).map_err(|error| error.to_string())?;
                        let object = OwnedArtifact::read_from(
                            &mut file,
                            length,
                            (identity.store, identity.artifact),
                            &memory,
                            &mut r,
                        )
                        .map_err(|error| error.to_string())?;
                        files
                            .objects
                            .push(object)
                            .map_err(|error| error.to_string())?;
                    }
                }
                Err(error) => {
                    let typed = match active_fault {
                        Fault::Append(_) | Fault::Read(_) => matches!(error, TreeError::Missing),
                        Fault::Cancel(_) => matches!(error, TreeError::Control(_)),
                        Fault::Budget => matches!(error, TreeError::Work),
                        Fault::None => false,
                    };
                    if !fired || !typed {
                        return Err(format!("PG8 unexpected preparation failure {error:?}"));
                    }
                    for position in 0..objects.len() {
                        assert!(
                            objects.artifact(position).is_err(),
                            "failed prepare cannot expose finalized artifacts"
                        );
                    }
                    let owned = objects.abort_inventory().count();
                    assert_eq!(owned, objects.len());
                    drop(objects);
                    assert_eq!(
                        memory.reserved_bytes(),
                        before,
                        "failed participant capacity released"
                    );
                    output.fired = true;
                    break;
                }
            }
            let next_base = base.after(&batch, request, fingerprint);
            drop(batch);
            base = next_base;
            model
                .apply(generation, request.primitive())
                .map_err(|error| format!("PG8 primitive trace: {error:?}"))?;
            let expected = model.snapshot();
            let observed =
                observe::all(&files, roots, &base, &mut r).map_err(|error| error.to_string())?;
            expected
                .check(&observed)
                .map_err(|error| format!("PG8 actual generation{generation}: {error:?}"))?;
            observe::ranges(&files, roots, &base, &expected, &mut r)
                .map_err(|error| error.to_string())?;
            output.observations.push(observed);
            saved.push((roots, expected));
            for (old_roots, expected) in &saved {
                let old = observe::all(&files, *old_roots, &base, &mut r)
                    .map_err(|error| error.to_string())?;
                expected
                    .check(&old)
                    .map_err(|error| format!("PG8 retained old root: {error:?}"))?;
            }
        }
    }
    if writer.reserved_bytes() != 0
        || shared.reserved_bytes().map_err(|error| error.to_string())? != shared_before
    {
        return Err("PG8 owner capacity leak".into());
    }
    Ok(output)
}
pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    let clean = run(seed, Fault::None)?;
    if clean.reuse_checks != 8 {
        return Err("PG8 private reuse route omitted".into());
    }
    for _ in 0..clean.reuse_checks {
        coverage.hit(REQUIRED_COVERAGE[15]);
    }
    for key in &REQUIRED_COVERAGE[..6] {
        coverage.hit(*key);
    }
    let mut faults = Vec::new();
    for cut in [1, clean.append_calls / 2, clean.append_calls] {
        faults.push((Fault::Append(cut.max(1)), 7, 8));
        faults.push((Fault::Cancel(cut.max(1)), 11, 12));
    }
    for cut in [1, clean.read_calls / 2, clean.read_calls] {
        faults.push((Fault::Read(cut.max(1)), 9, 10));
    }
    faults.push((Fault::Budget, 13, 14));
    for (fault, fire, paired) in faults {
        let broken = run(seed, fault)?;
        if !broken.fired || broken.observations != clean.observations[..3] {
            return Err(format!("PG8 fault/prefix mismatch: {fault:?}"));
        }
        coverage.hit(REQUIRED_COVERAGE[fire]);
        let control = run(seed, Fault::None)?;
        if control.observations != clean.observations {
            return Err("PG8 same-seed clean observations changed".into());
        }
        coverage.hit(REQUIRED_COVERAGE[paired]);
    }
    let mut expected = model::Model::new(model::Limits {
        max_image_bytes: 256 * 1024,
        ..Default::default()
    });
    for (index, request) in fixture::trace(seed).iter().enumerate() {
        expected
            .apply(index as u64 + 1, request.primitive())
            .map_err(|error| format!("PG8 primitive replay: {error:?}"))?;
    }
    let expected = expected.snapshot();
    let last = clean
        .observations
        .last()
        .ok_or("PG8 no clean observation")?;
    let mut broken = last.clone();
    broken.nodes[0].image.canonical[0] ^= 1;
    if expected.check(&broken).is_ok() {
        return Err("PG8 byte comparator failed to fire".into());
    }
    let mut broken = last.clone();
    broken.fences[0].provenance.original_generation += 1;
    if expected.check(&broken).is_ok() {
        return Err("PG8 provenance comparator failed to fire".into());
    }
    let mut broken = last.clone();
    broken.fences.reverse();
    if expected.check(&broken).is_ok() {
        return Err("PG8 order comparator failed to fire".into());
    }
    coverage.hit(REQUIRED_COVERAGE[6]);
    Ok(())
}
