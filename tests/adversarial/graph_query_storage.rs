//! PG19 actual query-resource storage seams with paired seeded controls.
//! The framed source is an immutable component fixture, not public view admission.
use super::coverage::CoverageRegistry;
use rand::RngCore;
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SnapshotLease, Store};
use zeppelin_embed::property_graph::query::resources::{MemoryError, QueryMemory};
use zeppelin_embed::property_graph::query::runtime::{
    RetainedView, RuntimeContext, RuntimeError, RuntimeLimits, WorkKind,
};
use zeppelin_embed::property_graph::query::{QueryError, QueryView};
use zeppelin_embed::property_graph::resources::GraphResources;
use zeppelin_embed::property_graph::storage::adjacency::RangeScratch;
use zeppelin_embed::property_graph::storage::artifact::{
    self, ArtifactId, ArtifactIdentity, Block, BlockKind, ContainerKind, FramedBlock, PhysicalRef,
};
use zeppelin_embed::property_graph::storage::payload::PayloadRef;
use zeppelin_embed::property_graph::storage::stream::PayloadSlice;
use zeppelin_embed::property_graph::storage::tree::TreeKind;
use zeppelin_embed::property_graph::storage::tree::directory::{
    BlockSource, DirectoryCursor, DirectoryRoot, TreeError, TreeResources, lookup_entry,
};
use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};

pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.query-storage.workspace",
    "property-graph.query-storage.counters",
    "property-graph.query-storage.owner-mismatch",
    "property-graph.query-storage.copy-limit.fire",
    "property-graph.query-storage.cancel.fire",
    "property-graph.query-storage.memory.fire",
    "property-graph.query-storage.same-seed-control",
    "property-graph.query-storage.oracle.can-fire",
    "property-graph.query-storage.release",
];

#[derive(Debug, Default)]
pub struct Report {
    pub comparisons: usize,
    pub fault_fires: usize,
    pub clean_controls: usize,
}

struct View {
    token: QueryView,
    lease: SnapshotLease,
}
impl RetainedView for View {
    fn query_view(&self) -> &QueryView {
        &self.token
    }
    fn check_active(&self) -> Result<(), QueryError> {
        self.lease
            .check_active()
            .map_err(|_| QueryError::ReadCancelled)
    }
}

struct Source {
    bytes: Vec<u8>,
    identity: ArtifactIdentity,
    reference: PhysicalRef,
}
impl BlockSource for Source {
    fn resolve<'a>(
        &'a self,
        reference: PhysicalRef,
        _: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        if reference != self.reference {
            return Err(TreeError::Missing);
        }
        let frame = artifact::decode(
            ContainerKind::Object,
            Some((self.identity.store, self.identity.artifact)),
            &self.bytes,
        )?;
        Ok(frame.framed_block(reference)?)
    }
}

fn source(seed: u64) -> Result<(Source, Vec<u8>), String> {
    let mut rng = super::test_support::seeded_rng("property_graph::query_storage", seed);
    let mut payload = vec![0; 32 + (rng.next_u32() as usize % 65)];
    rng.fill_bytes(&mut payload);
    let identity = ArtifactIdentity {
        store: StoreInstanceId::new(19).map_err(|e| e.to_string())?,
        artifact: ArtifactId::new(19).map_err(|e| e.to_string())?,
        generation: GraphGeneration::new(19),
        creation_serial: 19,
    };
    let blocks = [Block {
        kind: BlockKind::StoredText,
        payload: &payload,
    }];
    let mut bytes =
        vec![0; artifact::encoded_len(ContainerKind::Object, &blocks).map_err(|e| e.to_string())?];
    artifact::encode_into(ContainerKind::Object, identity, &blocks, &mut bytes)
        .map_err(|e| e.to_string())?;
    let frame = artifact::decode(
        ContainerKind::Object,
        Some((identity.store, identity.artifact)),
        &bytes,
    )
    .map_err(|e| e.to_string())?;
    let reference = frame.reference(0).map_err(|e| e.to_string())?;
    Ok((
        Source {
            bytes,
            identity,
            reference,
        },
        payload,
    ))
}

fn retained(store: &Store) -> Result<View, String> {
    Ok(View {
        token: QueryView::new(
            StoreInstanceId::new(19).map_err(|e| e.to_string())?,
            GraphGeneration::new(19),
        ),
        lease: store.snapshot().map_err(|e| e.to_string())?,
    })
}

pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<Report, String> {
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(8 * 1024 * 1024),
    )
    .map_err(|e| e.to_string())?;
    let shared = GraphResources::from_store(&store).map_err(|e| e.to_string())?;
    let shared_before = shared.reserved_bytes().map_err(|e| e.to_string())?;
    let view = retained(&store)?;
    let (source, expected) = source(seed)?;
    let payload = PayloadRef::new(
        BlockKind::StoredText,
        expected.len() as u64,
        source.reference,
    )
    .map_err(|e| e.to_string())?;
    let slice = PayloadSlice::new(
        &source,
        source.identity.store,
        source.identity.generation,
        payload,
    );
    let empty = DirectoryRoot::empty(
        source.identity.store,
        TreeKind::Nodes,
        source.identity.generation,
    );
    let mut report = Report::default();

    {
        let memory = QueryMemory::new(&shared, 1024 * 1024).map_err(|e| e.to_string())?;
        let other = QueryMemory::new(&shared, 1024 * 1024).map_err(|e| e.to_string())?;
        let control = QueryControl::Cancel(CancelToken::new());
        let mut context = RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default())
            .map_err(|e| e.to_string())?;
        let before = memory.reserved_bytes();
        let mut resources = TreeResources::for_query(&mut context).map_err(|e| e.to_string())?;
        if resources.reserved_bytes() != 256 * 1024 {
            return Err("PG19 query workspace differs".into());
        }
        coverage.hit(REQUIRED_COVERAGE[0]);
        if !matches!(
            RangeScratch::for_query(&other, &mut resources),
            Err(TreeError::Invalid("query memory owner mismatch"))
        ) {
            return Err("PG19 mixed query memory accepted".into());
        }
        coverage.hit(REQUIRED_COVERAGE[2]);
        let scratch = RangeScratch::for_query(&memory, &mut resources)
            .map_err(|e| format!("PG19 query scratch {e}"))?;
        drop(scratch);
        let key = 1u128.to_le_bytes();
        if lookup_entry(&source, empty, &key, &mut resources)
            .map_err(|e| e.to_string())?
            .is_some()
        {
            return Err("PG19 empty lookup returned an entry".into());
        }
        let mut cursor = DirectoryCursor::seek(&source, empty, None, &mut resources)
            .map_err(|e| e.to_string())?;
        if cursor
            .next_entry(&mut resources)
            .map_err(|e| e.to_string())?
            .is_some()
        {
            return Err("PG19 empty scan returned an entry".into());
        }
        let mut actual = vec![0; expected.len()];
        if slice
            .read_at(0, &mut actual, &mut resources)
            .map_err(|e| e.to_string())?
            != expected.len()
            || actual != expected
        {
            return Err("PG19 seeded payload differs".into());
        }
        let mut mutated = actual.clone();
        if let Some(first) = mutated.first_mut() {
            *first ^= 1;
        }
        if mutated == expected || actual != expected {
            return Err("PG19 comparator can-fire control failed".into());
        }
        report.comparisons += 2;
        coverage.hit(REQUIRED_COVERAGE[7]);
        drop(cursor);
        drop(resources);
        if context.counters().get(WorkKind::Lookups) != 1
            || context.counters().get(WorkKind::Scans) != 1
            || context.counters().get(WorkKind::CopiedBytes) != expected.len() as u64
            || memory.reserved_bytes() != before
        {
            return Err("PG19 exact counters or release differ".into());
        }
        coverage.hit(REQUIRED_COVERAGE[1]);
    }

    {
        let memory = QueryMemory::new(&shared, 1024 * 1024).map_err(|e| e.to_string())?;
        let control = QueryControl::Cancel(CancelToken::new());
        let limits = RuntimeLimits::default()
            .with_limit(WorkKind::CopiedBytes, expected.len() as u64 - 1)
            .map_err(|e| e.to_string())?;
        let mut context =
            RuntimeContext::new(&view, &control, &memory, limits).map_err(|e| e.to_string())?;
        let mut resources = TreeResources::for_query(&mut context).map_err(|e| e.to_string())?;
        let mut output = vec![0xa5; expected.len()];
        if !matches!(
            slice.read_at(0, &mut output, &mut resources),
            Err(TreeError::Runtime(RuntimeError::Limit(
                WorkKind::CopiedBytes
            )))
        ) || output.iter().any(|byte| *byte != 0xa5)
        {
            return Err("PG19 copy limit did not refuse before copy".into());
        }
        drop(resources);
        report.fault_fires += 1;
        coverage.hit(REQUIRED_COVERAGE[3]);
    }
    {
        let memory = QueryMemory::new(&shared, 1024 * 1024).map_err(|e| e.to_string())?;
        let control = QueryControl::Cancel(CancelToken::new());
        let mut context = RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default())
            .map_err(|e| e.to_string())?;
        let mut resources = TreeResources::for_query(&mut context).map_err(|e| e.to_string())?;
        let mut output = vec![0; expected.len()];
        slice
            .read_at(0, &mut output, &mut resources)
            .map_err(|e| e.to_string())?;
        if output != expected {
            return Err("PG19 copy-limit clean control differs".into());
        }
        report.clean_controls += 1;
        coverage.hit(REQUIRED_COVERAGE[6]);
    }

    {
        let memory = QueryMemory::new(&shared, 1024 * 1024).map_err(|e| e.to_string())?;
        let token = CancelToken::new();
        let control = QueryControl::Cancel(token.clone());
        let mut context = RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default())
            .map_err(|e| e.to_string())?;
        let before = memory.reserved_bytes();
        token.cancel();
        if !matches!(
            TreeResources::for_query(&mut context),
            Err(TreeError::Runtime(RuntimeError::Value(
                QueryError::Cancelled
            )))
        ) || memory.reserved_bytes() != before
        {
            return Err("PG19 cancellation did not fire before admission".into());
        }
        report.fault_fires += 1;
        coverage.hit(REQUIRED_COVERAGE[4]);
    }
    {
        let memory = QueryMemory::new(&shared, 1024 * 1024).map_err(|e| e.to_string())?;
        let control = QueryControl::Cancel(CancelToken::new());
        let mut context = RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default())
            .map_err(|e| e.to_string())?;
        drop(TreeResources::for_query(&mut context).map_err(|e| e.to_string())?);
        report.clean_controls += 1;
        coverage.hit(REQUIRED_COVERAGE[6]);
    }

    {
        let memory = QueryMemory::new(&shared, 320 * 1024).map_err(|e| e.to_string())?;
        let control = QueryControl::Cancel(CancelToken::new());
        let mut context = RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default())
            .map_err(|e| e.to_string())?;
        let mut resources = TreeResources::for_query(&mut context).map_err(|e| e.to_string())?;
        let before = memory.reserved_bytes();
        if !matches!(
            RangeScratch::for_query(&memory, &mut resources),
            Err(TreeError::Runtime(RuntimeError::Memory(MemoryError::Limit)))
        ) || memory.reserved_bytes() != before
        {
            return Err("PG19 memory refusal leaked".into());
        }
        report.fault_fires += 1;
        coverage.hit(REQUIRED_COVERAGE[5]);
    }
    {
        let memory = QueryMemory::new(&shared, 1024 * 1024).map_err(|e| e.to_string())?;
        let control = QueryControl::Cancel(CancelToken::new());
        let mut context = RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default())
            .map_err(|e| e.to_string())?;
        let mut resources = TreeResources::for_query(&mut context).map_err(|e| e.to_string())?;
        drop(
            RangeScratch::for_query(&memory, &mut resources)
                .map_err(|e| format!("PG19 memory clean control {e}"))?,
        );
        report.clean_controls += 1;
        coverage.hit(REQUIRED_COVERAGE[6]);
    }

    coverage.hit(REQUIRED_COVERAGE[8]);
    drop(view);
    if shared.reserved_bytes().map_err(|e| e.to_string())? != shared_before {
        return Err("PG19 shared reservations leaked".into());
    }
    store.close().map_err(|e| e.to_string())?;
    Ok(report)
}
