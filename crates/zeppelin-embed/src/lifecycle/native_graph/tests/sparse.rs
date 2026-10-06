#![cfg(test)]

use super::*;
use crate::property_graph::catalog::SymbolKind;
use crate::property_graph::staging::StagedBatch;
use crate::property_graph::{
    CanonicalEmbedding, CanonicalFingerprint, EntityShape, GraphDeleteMode, OperationProvenance,
    RelId,
};

struct OverrideArtifactsSource<'a, S> {
    base: &'a S,
    artifacts: Vec<(ArtifactId, Vec<u8>)>,
}

struct MissingReferenceSource<'a, S> {
    base: &'a S,
    missing: PhysicalRef,
}

impl<S: crate::property_graph::storage::tree::directory::BlockSource>
    crate::property_graph::storage::tree::directory::BlockSource for MissingReferenceSource<'_, S>
{
    fn resolve<'a>(
        &'a self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<crate::property_graph::storage::artifact::FramedBlock<'a>, TreeError> {
        if reference == self.missing {
            return Err(TreeError::Missing);
        }
        self.base.resolve(reference, resources)
    }
}

impl<S: crate::property_graph::storage::tree::directory::BlockSource>
    crate::property_graph::storage::tree::directory::BlockSource
    for OverrideArtifactsSource<'_, S>
{
    fn resolve<'a>(
        &'a self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<crate::property_graph::storage::artifact::FramedBlock<'a>, TreeError> {
        let Some((_, bytes)) = self
            .artifacts
            .iter()
            .find(|(artifact, _)| *artifact == reference.artifact)
        else {
            return self.base.resolve(reference, resources);
        };
        resources.step(1)?;
        let frame = artifact::decode(ContainerKind::Object, None, bytes)?;
        Ok(frame.framed_block(reference)?)
    }
}

fn rewrite_block(
    bytes: &[u8],
    target: BlockKind,
    mut change: impl FnMut(&mut [u8]),
) -> Option<Vec<u8>> {
    rewrite_matching_block(bytes, target, |payload| {
        change(payload);
        true
    })
}

fn rewrite_matching_block(
    bytes: &[u8],
    target: BlockKind,
    mut change: impl FnMut(&mut [u8]) -> bool,
) -> Option<Vec<u8>> {
    rewrite_selected_block(bytes, |reference, payload| {
        reference.kind == target && change(payload)
    })
}

fn rewrite_selected_block(
    bytes: &[u8],
    mut change: impl FnMut(PhysicalRef, &mut [u8]) -> bool,
) -> Option<Vec<u8>> {
    let frame = artifact::decode(ContainerKind::Object, None, bytes).unwrap();
    let identity = frame.identity();
    let mut payloads = Vec::<(BlockKind, Vec<u8>)>::new();
    let mut index = 0_usize;
    let mut changed = false;
    while let Ok(reference) = frame.reference(index) {
        let mut payload = frame.framed_block(reference).unwrap().payload().to_vec();
        if !changed && change(reference, &mut payload) {
            changed = true;
        }
        payloads.push((reference.kind, payload));
        index += 1;
    }
    if !changed {
        return None;
    }
    let blocks = payloads
        .iter()
        .map(|(kind, payload)| Block {
            kind: *kind,
            payload,
        })
        .collect::<Vec<_>>();
    let mut output = vec![0_u8; artifact::encoded_len(ContainerKind::Object, &blocks).unwrap()];
    artifact::encode_into(ContainerKind::Object, identity, &blocks, &mut output).unwrap();
    Some(output)
}

fn rewrite_sparse_membership(
    bytes: &[u8],
    target: PhysicalRef,
    mut change: impl FnMut(&mut Vec<u8>, &mut Vec<u8>),
) -> Option<Vec<u8>> {
    rewrite_selected_block(bytes, |reference, payload| {
        if reference != target {
            return false;
        }
        let Ok(page) = crate::property_graph::storage::tree::decode_page(
            crate::property_graph::storage::tree::TreeKind::SparseMembership,
            payload,
        ) else {
            return false;
        };
        if page.header().level != 0 {
            return false;
        }
        let mut owned = Vec::<(Vec<u8>, Vec<u8>)>::new();
        let mut index = 0_usize;
        while let Ok(cell) = page.cell(index) {
            let crate::property_graph::storage::tree::Cell::Leaf {
                key: crate::property_graph::storage::tree::Key::Inline(key),
                value,
            } = cell
            else {
                return false;
            };
            owned.push((key.to_vec(), value.to_vec()));
            index += 1;
        }
        let Some((key, value)) = owned.first_mut() else {
            return false;
        };
        change(key, value);
        let cells = owned
            .iter()
            .map(
                |(key, value)| crate::property_graph::storage::tree::Cell::Leaf {
                    key: crate::property_graph::storage::tree::Key::Inline(key),
                    value,
                },
            )
            .collect::<Vec<_>>();
        let mut encoded = vec![0_u8; crate::property_graph::storage::tree::PAGE_BYTES];
        crate::property_graph::storage::tree::encode_page(page.header(), &cells, &mut encoded)
            .unwrap();
        payload.copy_from_slice(&encoded);
        true
    })
}

fn rewrite_sparse_source_value(
    bytes: &[u8],
    target: PhysicalRef,
    mut change: impl FnMut(&mut Vec<u8>, &mut Vec<u8>),
) -> Option<Vec<u8>> {
    rewrite_selected_block(bytes, |reference, payload| {
        if reference != target {
            return false;
        }
        let Ok(page) = crate::property_graph::storage::tree::decode_page(
            crate::property_graph::storage::tree::TreeKind::SparseSources,
            payload,
        ) else {
            return false;
        };
        if page.header().level != 0 {
            return false;
        }
        let mut owned = Vec::<(Vec<u8>, Vec<u8>)>::new();
        let mut index = 0_usize;
        while let Ok(cell) = page.cell(index) {
            let crate::property_graph::storage::tree::Cell::Leaf {
                key: crate::property_graph::storage::tree::Key::Inline(key),
                value,
            } = cell
            else {
                return false;
            };
            owned.push((key.to_vec(), value.to_vec()));
            index += 1;
        }
        let Some((key, value)) = owned.first_mut() else {
            return false;
        };
        change(key, value);
        let cells = owned
            .iter()
            .map(
                |(key, value)| crate::property_graph::storage::tree::Cell::Leaf {
                    key: crate::property_graph::storage::tree::Key::Inline(key),
                    value,
                },
            )
            .collect::<Vec<_>>();
        let mut encoded = vec![0_u8; crate::property_graph::storage::tree::PAGE_BYTES];
        crate::property_graph::storage::tree::encode_page(page.header(), &cells, &mut encoded)
            .unwrap();
        payload.copy_from_slice(&encoded);
        true
    })
}

fn rewrite_directory_without_key(
    bytes: &[u8],
    target: PhysicalRef,
    kind: crate::property_graph::storage::tree::TreeKind,
    removed_key: &[u8],
) -> Option<Vec<u8>> {
    rewrite_selected_block(bytes, |reference, payload| {
        if reference != target {
            return false;
        }
        let Ok(page) = crate::property_graph::storage::tree::decode_page(kind, payload) else {
            return false;
        };
        if page.header().level != 0 {
            return false;
        }
        let mut owned = Vec::<(Vec<u8>, Vec<u8>)>::new();
        let mut removed = false;
        let mut index = 0_usize;
        while let Ok(cell) = page.cell(index) {
            let crate::property_graph::storage::tree::Cell::Leaf {
                key: crate::property_graph::storage::tree::Key::Inline(key),
                value,
            } = cell
            else {
                return false;
            };
            if key == removed_key {
                removed = true;
            } else {
                owned.push((key.to_vec(), value.to_vec()));
            }
            index += 1;
        }
        if !removed {
            return false;
        }
        let cells = owned
            .iter()
            .map(
                |(key, value)| crate::property_graph::storage::tree::Cell::Leaf {
                    key: crate::property_graph::storage::tree::Key::Inline(key),
                    value,
                },
            )
            .collect::<Vec<_>>();
        let mut encoded = vec![0_u8; crate::property_graph::storage::tree::PAGE_BYTES];
        crate::property_graph::storage::tree::encode_page(page.header(), &cells, &mut encoded)
            .unwrap();
        payload.copy_from_slice(&encoded);
        true
    })
}

fn roots_for_rewritten_artifact(
    mut roots: crate::property_graph::storage::search::SparseRoots,
    artifact_id: ArtifactId,
    bytes: &[u8],
) -> crate::property_graph::storage::search::SparseRoots {
    let checksum =
        u64::from_le_bytes(*bytes.get(bytes.len() - 8..).unwrap().first_chunk().unwrap());
    for required in [&mut roots.text, &mut roots.vector].into_iter().flatten() {
        if required.object.artifact == artifact_id {
            required.object.bytes = u32::try_from(bytes.len()).unwrap();
            required.object.checksum = checksum;
        }
    }
    roots
}

fn rewrite_installed_block(
    directory: &std::path::Path,
    artifacts: &mut Vec<(ArtifactId, Vec<u8>)>,
    target: PhysicalRef,
    mut change: impl FnMut(&mut [u8]),
) {
    let position = artifacts
        .iter()
        .position(|(artifact, _)| *artifact == target.artifact);
    let base = match position {
        Some(index) => artifacts.get(index).unwrap().1.clone(),
        None => std::fs::read(artifact_path(directory, target.artifact)).unwrap(),
    };
    let bytes = rewrite_selected_block(&base, |reference, payload| {
        if reference != target {
            return false;
        }
        change(payload);
        true
    })
    .unwrap();
    match position {
        Some(index) => artifacts.get_mut(index).unwrap().1 = bytes,
        None => artifacts.push((target.artifact, bytes)),
    }
}

fn rewrite_installed_membership(
    directory: &std::path::Path,
    artifacts: &mut Vec<(ArtifactId, Vec<u8>)>,
    target: PhysicalRef,
    mut change: impl FnMut(&mut Vec<u8>, &mut Vec<u8>),
) {
    let position = artifacts
        .iter()
        .position(|(artifact, _)| *artifact == target.artifact);
    let base = match position {
        Some(index) => artifacts.get(index).unwrap().1.clone(),
        None => std::fs::read(artifact_path(directory, target.artifact)).unwrap(),
    };
    let bytes = rewrite_sparse_membership(&base, target, |key, value| change(key, value)).unwrap();
    match position {
        Some(index) => artifacts.get_mut(index).unwrap().1 = bytes,
        None => artifacts.push((target.artifact, bytes)),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "fixture spells out independent bounds and resource owners"
)]
fn materialize_sparse_generation<'source, 'a, 'b, S, F>(
    artifacts: crate::property_graph::storage::PreparedGraphArtifacts<'source, 'a, 'b, S, F>,
    admitted: &NativeGraphBundle,
    directory: &std::path::Path,
    root_artifact: u128,
    catalog: crate::property_graph::wal::RequiredRef,
    node_high_water: u128,
    relationship_high_water: u128,
    symbol_high_waters: crate::property_graph::catalog::SymbolHighWaters,
    lexical: crate::fts::tokenizer::TokenizerEpoch,
    document: Option<crate::epoch::EmbeddingTower>,
) -> NativeGraphBundleInput
where
    S: crate::property_graph::storage::tree::directory::BlockSource,
    F: FnMut() -> Result<ArtifactIdentity, TreeError>,
{
    let roots = artifacts.candidate().roots();
    let sequence = artifacts.candidate().sequence();
    let generation = artifacts.candidate().target_generation();
    let store = roots.store();
    let sparse = artifacts.sparse_roots();
    let descriptors = artifacts
        .inventory()
        .iter()
        .map(|change| change.object)
        .collect::<Vec<_>>();
    for index in 0..artifacts.objects().len() {
        let object = artifacts.objects().artifact(index).unwrap();
        std::fs::write(
            artifact_path(directory, object.identity().artifact),
            object.bytes(),
        )
        .unwrap();
    }
    let mut wal_roots = WalGraphRoots::default();
    for (slot, reference) in roots.references().into_iter().enumerate() {
        let Some(block) = reference else {
            continue;
        };
        let object = descriptors
            .iter()
            .find(|descriptor| descriptor.artifact == block.artifact)
            .copied()
            .map(|object| RequiredRef { object, block })
            .or_else(|| admitted.wal_roots().slots[slot].filter(|required| required.block == block))
            .unwrap();
        wal_roots.slots[slot] = Some(object);
    }
    let max_serial = descriptors
        .iter()
        .map(|descriptor| descriptor.serial)
        .max()
        .unwrap_or(0)
        .max(catalog.object.serial)
        .max(admitted.high_waters().creation_serial);
    let root_identity = ArtifactIdentity {
        store,
        artifact: ArtifactId::new(root_artifact).unwrap(),
        generation,
        creation_serial: max_serial + 1,
    };
    let root_envelope = write_framed_file(
        directory,
        ContainerKind::RootEnvelope,
        root_identity,
        &[Block {
            kind: BlockKind::CheckpointPayload,
            payload: b"ze61-sparse-generation",
        }],
    );
    NativeGraphBundleInput {
        base: BaseIdentity {
            store,
            generation,
            fold: Default::default(),
            roots: Some(root_identity.artifact),
        },
        root_envelope,
        roots,
        wal_roots,
        sequence,
        catalog,
        vector: sparse.vector,
        text: sparse.text,
        reclaim: None,
        high_waters: HighWaters {
            node: node_high_water,
            relationship: relationship_high_water,
            symbols: [
                symbol_high_waters.label,
                symbol_high_waters.relationship_type,
                symbol_high_waters.property,
                symbol_high_waters.namespace,
            ],
            creation_serial: root_identity.creation_serial,
        },
        prepared_inventories: Vec::new(),
        lexical,
        document,
    }
}

#[derive(Clone)]
struct SparseFixtureEntry<'a> {
    provenance: OperationProvenance<'a>,
    shape: Option<EntityShape<'a>>,
    canonical: Option<Vec<u8>>,
    membership: crate::property_graph::staging::Membership,
}

impl crate::property_graph::staging::CanonicalSource for SparseFixtureEntry<'_> {
    fn read_at(&self, offset: u64, output: &mut [u8]) -> std::io::Result<usize> {
        let bytes = self
            .canonical
            .as_ref()
            .ok_or(std::io::ErrorKind::InvalidData)?;
        let bytes = bytes
            .get(offset as usize..)
            .ok_or(std::io::ErrorKind::UnexpectedEof)?;
        let count = bytes.len().min(output.len());
        output
            .get_mut(..count)
            .ok_or(std::io::ErrorKind::InvalidInput)?
            .copy_from_slice(bytes.get(..count).ok_or(std::io::ErrorKind::InvalidInput)?);
        Ok(count)
    }
}

impl SparseFixtureEntry<'_> {
    fn live(&self, view: BaseIdentity) -> Option<BaseEntity<'_>> {
        let bytes = self.canonical.as_ref()?;
        Some(BaseEntity {
            view,
            provenance: self.provenance,
            shape: self.shape?,
            fingerprint: CanonicalFingerprint::new(
                bytes.len() as u64,
                xxhash_rust::xxh3::xxh3_64(bytes),
            )
            .unwrap(),
            source: self,
            membership: self.membership,
        })
    }
}

struct SparseFixture<'a> {
    identity: BaseIdentity,
    high: StageHighWaters,
    entries: Vec<SparseFixtureEntry<'a>>,
    symbols: Vec<SymbolEntry<'a>>,
    document: Option<crate::epoch::EmbeddingTower>,
}

impl<'a> SparseFixture<'a> {
    fn empty(
        identity: BaseIdentity,
        high: StageHighWaters,
        document: Option<crate::epoch::EmbeddingTower>,
    ) -> Self {
        Self {
            identity,
            high,
            entries: Vec::new(),
            symbols: Vec::new(),
            document,
        }
    }

    fn after<'b>(
        &'b self,
        batch: &'b StagedBatch<'b>,
        identity: BaseIdentity,
    ) -> SparseFixture<'b> {
        let mut entries = self.entries.clone();
        for delta in batch.deltas() {
            let entry = SparseFixtureEntry {
                provenance: delta.provenance(),
                shape: delta.shape(),
                canonical: delta.canonical().map(<[u8]>::to_vec),
                membership: delta.membership().1,
            };
            if let Some(old) = entries.iter_mut().find(|old| {
                old.provenance.fields().incarnation == entry.provenance.fields().incarnation
            }) {
                *old = entry;
            } else {
                entries.push(entry);
            }
        }
        let mut symbols = self.symbols.clone();
        symbols.extend_from_slice(batch.symbols());
        SparseFixture {
            identity,
            high: batch.high_waters(),
            entries,
            symbols,
            document: self.document.clone(),
        }
    }
}

impl AdmittedBase for SparseFixture<'_> {
    fn identity(&self) -> BaseIdentity {
        self.identity
    }
    fn high_waters(&self) -> StageHighWaters {
        self.high
    }
    fn interpretation(&self) -> GraphInterpretation<'_> {
        GraphInterpretation::new(
            TokenizerEpoch::of(&TokenizerConfig::text_default()),
            self.document.as_ref(),
        )
        .unwrap()
    }
    fn key(
        &self,
        key: ApplicationKey<'_>,
        _: &mut WriteControl<'_>,
    ) -> Result<BaseKeyState<'_>, StageError> {
        Ok(
            match self
                .entries
                .iter()
                .rev()
                .find(|entry| entry.provenance.fields().key == Some(key))
            {
                Some(entry) => match entry.live(self.identity) {
                    Some(live) => BaseKeyState::Live(live),
                    None => BaseKeyState::Deleted(self.identity, entry.provenance),
                },
                None => BaseKeyState::NeverUsed,
            },
        )
    }
    fn entity(
        &self,
        id: EntityId,
        _: &mut WriteControl<'_>,
    ) -> Result<Option<BaseEntity<'_>>, StageError> {
        Ok(self
            .entries
            .iter()
            .find(|entry| entry.provenance.fields().incarnation == id)
            .and_then(|entry| entry.live(self.identity)))
    }
    fn has_live_incident(
        &self,
        _: NodeId,
        _: &[RelId],
        _: &mut WriteControl<'_>,
    ) -> Result<bool, StageError> {
        Ok(false)
    }
    fn property(
        &self,
        _: EntityId,
        _: GraphName<'_>,
        _: &mut WriteControl<'_>,
    ) -> Result<Option<PropertyValue<'_>>, StageError> {
        Ok(None)
    }
    fn stored_text(&self, _: NodeId, _: &mut WriteControl<'_>) -> Result<Option<&str>, StageError> {
        Ok(None)
    }
    fn symbol(
        &self,
        kind: SymbolKind,
        name: GraphName<'_>,
        _: &mut WriteControl<'_>,
    ) -> Result<Option<Symbol>, StageError> {
        Ok(self
            .symbols
            .iter()
            .find(|entry| entry.symbol.kind() == kind && entry.name == name)
            .map(|entry| entry.symbol))
    }
}

#[test]
fn ze61_sparse_populations_match_model() {
    use crate::property_graph::storage::search::{Modality, SparseView};
    use crate::property_graph::storage::{
        GraphPreparation, NativePreparationCatalog, NativePreparationSource,
    };

    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let identity = StoreInstanceId::new(61_001).unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let storage = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let document = crate::epoch::EmbeddingTower {
        model_id: "ze61-document".into(),
        model_version: "1".into(),
        weights_digest: vec![0x61, 0xa5],
        dims: 2,
        normalization: crate::epoch::Normalization::None,
        prompt_prefix: "doc: ".into(),
        max_tokens: 32,
        runtime: crate::epoch::EmbeddingRuntime::CpuReference,
        compute_units: crate::epoch::ComputeUnits::Cpu,
        os_build: None,
    };
    let zero = [0.0_f32, -0.0_f32];
    let bits = [f32::from_bits(0x3f80_0001), f32::from_bits(0x8000_0000)];
    let zero_vector = crate::property_graph::CanonicalEmbedding::new(&document, &zero).unwrap();
    let bit_vector = crate::property_graph::CanonicalEmbedding::new(&document, &bits).unwrap();
    let mut labels = [];
    let mut properties = [];
    let graph_only = CanonicalContents::node(&mut labels, &mut properties, None, None).unwrap();
    let mut labels = [];
    let mut properties = [];
    let empty = CanonicalContents::node(&mut labels, &mut properties, Some(""), None).unwrap();
    let mut labels = [];
    let mut properties = [];
    let analyzed_empty =
        CanonicalContents::node(&mut labels, &mut properties, Some("the and"), None).unwrap();
    let mut labels = [];
    let mut properties = [];
    let text = CanonicalContents::node(&mut labels, &mut properties, Some("bronze zeppelin"), None)
        .unwrap();
    let mut labels = [];
    let mut properties = [];
    let vector =
        CanonicalContents::node(&mut labels, &mut properties, None, Some(zero_vector)).unwrap();
    let mut labels = [];
    let mut properties = [];
    let both = CanonicalContents::node(
        &mut labels,
        &mut properties,
        Some("silver zeppelin"),
        Some(bit_vector),
    )
    .unwrap();
    let collision_vector =
        crate::property_graph::CanonicalEmbedding::new(&document, &bits).unwrap();
    let collision_both = CanonicalContents::node(
        &mut [],
        &mut [],
        Some("collision zeppelin"),
        Some(collision_vector),
    )
    .unwrap();
    let collision_low = NodeId::new(1).unwrap();
    let collision_key = ApplicationKey::new(EntityKind::Node, "app", "collision-low").unwrap();
    let bootstrap_graph = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let initial_identity = BaseIdentity {
        store: identity,
        generation: GraphGeneration::new(0),
        fold: Default::default(),
        roots: Some(ArtifactId::new(61_001).unwrap()),
    };
    let initial_high = StageHighWaters::default();
    let initial = EmptyProducerBase {
        identity: initial_identity,
        high_waters: initial_high,
        document: Some(document.clone()),
    };
    let bootstrap = [StructuredWrite {
        key: collision_key,
        revision: GraphRevision::new(1).unwrap(),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&bootstrap_graph)),
    }];
    let staged0 = stage_structured(&initial, &bootstrap, &writer, &mut |_| Ok(())).unwrap();
    let mut input0 = bundle(identity, 0, 61_001);
    input0.catalog = write_complete_sparse_catalog(
        directory.path(),
        ArtifactIdentity {
            store: identity,
            artifact: ArtifactId::new(61_002).unwrap(),
            generation: GraphGeneration::new(0),
            creation_serial: 10,
        },
        initial_high,
        &[],
        Some(&document),
    );
    input0.root_envelope = write_framed_file(
        directory.path(),
        ContainerKind::RootEnvelope,
        ArtifactIdentity {
            store: identity,
            artifact: ArtifactId::new(61_001).unwrap(),
            generation: GraphGeneration::new(0),
            creation_serial: 9,
        },
        &[Block {
            kind: BlockKind::CheckpointPayload,
            payload: b"ze61-sparse-base",
        }],
    );
    input0.document = Some(document.clone());
    input0.high_waters.creation_serial = 10;
    store.install_native_graph_for_test(input0).unwrap();
    let lease0 = store.admit_native_read().unwrap();
    let source0 = NativePreparationSource::new(&lease0, &storage, 32).unwrap();
    let mut resources0 = source0.resources(u64::MAX).unwrap();
    let mut artifact0 = 61_010_u128;
    let mut serial0 = 11_u64;
    let prepared0 = GraphPreparation::new(
        &source0,
        GraphGeneration::new(1),
        || {
            let output = ArtifactIdentity {
                store: identity,
                artifact: ArtifactId::new(artifact0)?,
                generation: GraphGeneration::new(1),
                creation_serial: serial0,
            };
            artifact0 += 1;
            serial0 += 1;
            Ok(output)
        },
        PackLimits::default(),
        &store.tokenizer,
        &mut resources0,
    )
    .unwrap()
    .prepare(&staged0, &mut resources0)
    .unwrap_or_else(|failure| panic!("collision bootstrap failed: {}", failure.error()));
    let target_identity0 = BaseIdentity {
        store: identity,
        generation: GraphGeneration::new(1),
        fold: Default::default(),
        roots: Some(ArtifactId::new(61_099).unwrap()),
    };
    let fixture0 = SparseFixture::empty(initial_identity, initial_high, Some(document.clone()));
    let mut admitted = fixture0.after(&staged0, target_identity0);
    admitted.high.node = 1_u128 << 64;
    let catalog0 = write_complete_sparse_catalog(
        directory.path(),
        ArtifactIdentity {
            store: identity,
            artifact: ArtifactId::new(61_098).unwrap(),
            generation: GraphGeneration::new(1),
            creation_serial: 10_000,
        },
        admitted.high,
        &admitted.symbols,
        Some(&document),
    );
    let input1 = materialize_sparse_generation(
        prepared0,
        lease0.bundle(),
        directory.path(),
        61_099,
        catalog0,
        admitted.high.node,
        admitted.high.relationship,
        admitted.high.symbols,
        lease0.bundle().lexical(),
        Some(document.clone()),
    );
    drop(resources0);
    drop(source0);
    drop(lease0);
    store.install_native_graph_for_test(input1).unwrap();

    let contents = [graph_only, empty, analyzed_empty, text, vector, both];
    let keys = ["graph", "empty", "stops", "text", "vector", "both"];
    let mut requests = Vec::with_capacity(8);
    requests.push(StructuredWrite {
        key: collision_key,
        revision: GraphRevision::new(2).unwrap(),
        operation: StructuredOperation::Put(EntityId::Node(collision_low)),
        image: Some(WriteImage::Node(&collision_both)),
    });
    requests.push(StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "collision-high").unwrap(),
        revision: GraphRevision::new(1).unwrap(),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&collision_both)),
    });
    requests.extend((0..contents.len()).map(|index| StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", keys[index]).unwrap(),
        revision: GraphRevision::new(1).unwrap(),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&contents[index])),
    }));
    let staged = stage_structured(&admitted, &requests, &writer, &mut |_| Ok(())).unwrap();
    let lease = store.admit_native_read().unwrap();
    let source = NativePreparationSource::new(&lease, &storage, 64).unwrap();
    let mut resources = source.resources(u64::MAX).unwrap();
    let generation = GraphGeneration::new(lease.bundle().base().generation.get() + 1);
    let mut next_artifact = 61_100_u128;
    let mut next_serial = lease.bundle().high_waters().creation_serial + 1;
    let preparation = GraphPreparation::new(
        &source,
        generation,
        || {
            let identity = ArtifactIdentity {
                store: identity,
                artifact: ArtifactId::new(next_artifact)?,
                generation,
                creation_serial: next_serial,
            };
            next_artifact += 1;
            next_serial += 1;
            Ok(identity)
        },
        PackLimits::default(),
        &store.tokenizer,
        &mut resources,
    )
    .unwrap();
    let artifacts = preparation
        .prepare(&staged, &mut resources)
        .unwrap_or_else(|failure| {
            panic!(
                "native preparation failed before sparse handoff: {}",
                failure.error()
            )
        });

    // Role 6 subtype 1 is a sparse root and stays at version 1. Role 6 subtype 2
    // is a sparse source, which ZE-158 writes in the 200-byte V2 layout, so the
    // two populations no longer share one version and must be counted apart.
    let mut sparse_roots = 0_usize;
    let mut sparse_sources = 0_usize;
    for index in 0..artifacts.objects().len() {
        let object = artifacts.objects().artifact(index).unwrap();
        let frame = artifact::decode(
            ContainerKind::Object,
            Some((object.identity().store, object.identity().artifact)),
            object.bytes(),
        )
        .unwrap();
        let mut block = 0_usize;
        while let Ok(reference) = frame.reference(block) {
            if reference.kind == BlockKind::CommitParticipant {
                let payload = frame.framed_block(reference).unwrap().payload();
                if payload.get(..9) == Some(&[b'Z', b'G', b'C', b'P', 6, 0, 1, 0, 1]) {
                    sparse_roots += 1;
                }
                if payload.get(..9) == Some(&[b'Z', b'G', b'C', b'P', 6, 0, 2, 0, 2]) {
                    sparse_sources += 1;
                    assert_eq!(payload.len(), 200, "sparse source is not the V2 width");
                    // A V2 vector source carries its native index; a text source
                    // never does. Offset 144 is that presence byte.
                    let presence = payload.get(144).copied();
                    match payload.get(9).copied() {
                        Some(1) => assert_eq!(
                            presence,
                            Some(0),
                            "sparse text source claims a native vector index"
                        ),
                        Some(2) => assert_eq!(
                            presence,
                            Some(1),
                            "sparse vector source omits its native vector index"
                        ),
                        other => panic!("unexpected sparse modality {other:?}"),
                    }
                }
            }
            block += 1;
        }
    }
    assert_eq!(
        sparse_roots, 2,
        "finished real preparation omitted the two sparse roots"
    );
    assert_eq!(
        sparse_sources, 2,
        "finished real preparation omitted the two sparse V2 sources"
    );

    let roots = artifacts.sparse_roots();
    let catalog = NativePreparationCatalog::open(&source, &mut resources).unwrap();
    let sparse = SparseView::open(
        artifacts.objects(),
        roots,
        artifacts.candidate().roots(),
        lease.bundle().catalog(),
        &catalog,
        lease.bundle().document(),
        lease.bundle().lexical(),
        &storage,
        &mut resources,
    )
    .unwrap();
    assert_eq!(sparse.text_count(), 4);
    assert_eq!(sparse.vector_count(), 4);
    assert_eq!(sparse.text_length(), 8);

    let nodes = staged
        .receipts()
        .iter()
        .map(|receipt| match receipt.entity {
            EntityId::Node(node) => node,
            EntityId::Relationship(_) => panic!("node fixture returned a relationship"),
        })
        .collect::<Vec<_>>();
    assert_eq!(nodes.len(), 8);
    assert_eq!(nodes[0], collision_low);
    assert_eq!(nodes[1], NodeId::new((1_u128 << 64) + 1).unwrap());
    assert_eq!(nodes[0].get() as u64, nodes[1].get() as u64);
    for (row, node) in nodes.iter().take(2).copied().enumerate() {
        let text = sparse
            .lookup(Modality::Text, node, &mut resources)
            .unwrap()
            .unwrap();
        let vector = sparse
            .lookup(Modality::Vector, node, &mut resources)
            .unwrap()
            .unwrap();
        assert_eq!(
            (text.node, text.revision, text.row),
            (node, if row == 0 { 2 } else { 1 }, row as u32)
        );
        assert_eq!(
            (vector.node, vector.revision, vector.row),
            (node, if row == 0 { 2 } else { 1 }, row as u32)
        );
    }
    for node in nodes.iter().skip(2).take(3) {
        assert!(
            sparse
                .lookup(Modality::Text, *node, &mut resources)
                .unwrap()
                .is_none()
        );
        assert!(
            sparse
                .lookup(Modality::Vector, *node, &mut resources)
                .unwrap()
                .is_none()
        );
    }
    let text_member = sparse
        .lookup(Modality::Text, nodes[5], &mut resources)
        .unwrap()
        .unwrap();
    assert_eq!(
        (text_member.node, text_member.revision, text_member.row),
        (nodes[5], 1, 2)
    );
    assert_eq!(text_member.analyzed_length, 2);
    assert!(text_member.vector.is_none());
    assert!(
        sparse
            .lookup(Modality::Vector, nodes[5], &mut resources)
            .unwrap()
            .is_none()
    );

    let vector_member = sparse
        .lookup(Modality::Vector, nodes[6], &mut resources)
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            vector_member.node,
            vector_member.revision,
            vector_member.row
        ),
        (nodes[6], 1, 2)
    );
    let vector_payload = vector_member.vector.unwrap();
    assert_eq!(
        vector_payload
            .coordinate(0, &mut resources)
            .unwrap()
            .to_bits(),
        0
    );
    assert_eq!(
        vector_payload
            .coordinate(1, &mut resources)
            .unwrap()
            .to_bits(),
        0x8000_0000
    );

    let both_text = sparse
        .lookup(Modality::Text, nodes[7], &mut resources)
        .unwrap()
        .unwrap();
    let both_vector = sparse
        .lookup(Modality::Vector, nodes[7], &mut resources)
        .unwrap()
        .unwrap();
    assert_eq!(
        (both_text.node, both_text.revision, both_text.row),
        (nodes[7], 1, 3)
    );
    assert_eq!(both_text.analyzed_length, 2);
    assert_eq!(
        (both_vector.node, both_vector.revision, both_vector.row),
        (nodes[7], 1, 3)
    );
    let both_payload = both_vector.vector.unwrap();
    assert_eq!(
        both_payload
            .coordinate(0, &mut resources)
            .unwrap()
            .to_bits(),
        0x3f80_0001
    );
    assert_eq!(
        both_payload
            .coordinate(1, &mut resources)
            .unwrap()
            .to_bits(),
        0x8000_0000
    );

    let expected_text = [
        (nodes[0], 2_u64),
        (nodes[1], 1_u64),
        (nodes[5], 1_u64),
        (nodes[7], 1_u64),
    ]
    .into_iter()
    .collect::<std::collections::BTreeSet<_>>();
    let expected_vector = [
        (nodes[0], 2_u64),
        (nodes[1], 1_u64),
        (nodes[6], 1_u64),
        (nodes[7], 1_u64),
    ]
    .into_iter()
    .collect::<std::collections::BTreeSet<_>>();
    let observed = |modality, resources: &mut TreeResources<'_>| {
        nodes
            .iter()
            .filter_map(|node| {
                sparse
                    .lookup(modality, *node, resources)
                    .unwrap()
                    .map(|member| (member.node, member.revision))
            })
            .collect::<std::collections::BTreeSet<_>>()
    };
    let actual_text = observed(Modality::Text, &mut resources);
    let actual_vector = observed(Modality::Vector, &mut resources);
    assert_eq!(actual_text, expected_text);
    assert_eq!(actual_vector, expected_vector);
    let mut injected_dense_text = actual_text.clone();
    injected_dense_text.insert((nodes[4], 1));
    assert_ne!(injected_dense_text, expected_text);
    let mut injected_fake_vector = actual_vector.clone();
    injected_fake_vector.insert((nodes[2], 1));
    assert_ne!(injected_fake_vector, expected_vector);
    assert_eq!(actual_text, expected_text);
    assert_eq!(actual_vector, expected_vector);
}

fn physical_order(reference: &PhysicalRef) -> (u128, u64, u32, u16, u16) {
    (
        reference.artifact.get(),
        reference.offset,
        reference.length,
        reference.kind as u16,
        reference.version,
    )
}

fn expected_payload_closure<S: crate::property_graph::storage::tree::directory::BlockSource>(
    source: &S,
    payload: crate::property_graph::storage::payload::PayloadRef,
    resources: &mut TreeResources<'_>,
    output: &mut Vec<PhysicalRef>,
) {
    let reference = payload.reference();
    output.push(reference);
    let block = crate::property_graph::storage::tree::directory::BlockSource::resolve(
        source, reference, resources,
    )
    .unwrap();
    if reference.kind == BlockKind::ExtentList {
        let count =
            u32::from_le_bytes(block.payload().get(16..20).unwrap().try_into().unwrap()) as usize;
        for index in 0..count {
            let start = 32 + index * 32;
            output.push(
                artifact::decode_reference(block.payload().get(start..start + 32).unwrap())
                    .unwrap(),
            );
        }
    }
}

fn expected_directory_entries<S: crate::property_graph::storage::tree::directory::BlockSource>(
    source: &S,
    root: PhysicalRef,
    kind: crate::property_graph::storage::tree::TreeKind,
    resources: &mut TreeResources<'_>,
    output: &mut Vec<PhysicalRef>,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut pending = vec![root];
    let mut entries = Vec::new();
    while let Some(reference) = pending.pop() {
        output.push(reference);
        let block = crate::property_graph::storage::tree::directory::BlockSource::resolve(
            source, reference, resources,
        )
        .unwrap();
        let page =
            crate::property_graph::storage::tree::decode_page(kind, block.payload()).unwrap();
        let count =
            u32::from_le_bytes(block.payload().get(12..16).unwrap().try_into().unwrap()) as usize;
        for index in 0..count {
            match page.cell(index).unwrap() {
                crate::property_graph::storage::tree::Cell::Leaf {
                    key: crate::property_graph::storage::tree::Key::Inline(key),
                    value,
                } => entries.push((key.to_vec(), value.to_vec())),
                crate::property_graph::storage::tree::Cell::Branch { child, .. } => {
                    pending.push(child);
                }
                _ => panic!("sparse fixture used an overflow leaf key"),
            }
        }
    }
    entries
}

fn expected_installed_trace_closure<
    S: crate::property_graph::storage::tree::directory::BlockSource,
>(
    source: &S,
    roots: crate::property_graph::storage::search::SparseRoots,
    store: StoreInstanceId,
    resources: &mut TreeResources<'_>,
) -> Vec<PhysicalRef> {
    let mut output = Vec::new();
    for required in [roots.text, roots.vector].into_iter().flatten() {
        output.push(required.block);
        let root = crate::property_graph::storage::tree::directory::BlockSource::resolve(
            source,
            required.block,
            resources,
        )
        .unwrap();
        let payload = root.payload();
        output.push(artifact::decode_reference(payload.get(128..160).unwrap()).unwrap());
        let members = artifact::decode_reference(payload.get(168..200).unwrap()).unwrap();
        let sources = artifact::decode_reference(payload.get(208..240).unwrap()).unwrap();
        let _ = expected_directory_entries(
            source,
            members,
            crate::property_graph::storage::tree::TreeKind::SparseMembership,
            resources,
            &mut output,
        );
        let source_entries = expected_directory_entries(
            source,
            sources,
            crate::property_graph::storage::tree::TreeKind::SparseSources,
            resources,
            &mut output,
        );
        for (key, value) in source_entries {
            let source_reference = artifact::decode_reference(&key).unwrap();
            let mask = crate::property_graph::storage::payload::PayloadRef::decode(
                value.get(..48).unwrap(),
            )
            .unwrap();
            expected_payload_closure(source, mask, resources, &mut output);
            output.push(source_reference);
            let source_block =
                crate::property_graph::storage::tree::directory::BlockSource::resolve(
                    source,
                    source_reference,
                    resources,
                )
                .unwrap();
            let manifest = source_block.payload();
            let generation = GraphGeneration::new(u64::from_le_bytes(
                manifest.get(16..24).unwrap().try_into().unwrap(),
            ));
            let rows = u32::from_le_bytes(manifest.get(32..36).unwrap().try_into().unwrap());
            let row_table = crate::property_graph::storage::payload::PayloadRef::decode(
                manifest.get(40..88).unwrap(),
            )
            .unwrap();
            expected_payload_closure(source, row_table, resources, &mut output);
            if manifest.get(88).copied() == Some(1) {
                let lexical = crate::property_graph::storage::payload::PayloadRef::decode(
                    manifest.get(96..144).unwrap(),
                )
                .unwrap();
                expected_payload_closure(source, lexical, resources, &mut output);
            }
            // ZE-158 gives every V2 vector source a native index. Offset 144 is
            // the presence byte and is absent from the 144-byte V1 manifest. The
            // index header carries its own interpretation catalog RequiredRef at
            // bytes 64..160, whose block reference lives at 128..160, so both the
            // index block and that catalog participant are trace descendants.
            if manifest.get(144).copied() == Some(1) {
                let vector_index = crate::property_graph::storage::payload::PayloadRef::decode(
                    manifest.get(152..200).unwrap(),
                )
                .unwrap();
                expected_payload_closure(source, vector_index, resources, &mut output);
                let index_payload = crate::property_graph::storage::stream::PayloadSlice::new(
                    source,
                    store,
                    generation,
                    vector_index,
                );
                let mut header = [0_u8; 160];
                assert_eq!(
                    index_payload.read_at(0, &mut header, resources).unwrap(),
                    header.len()
                );
                output.push(artifact::decode_reference(header.get(128..160).unwrap()).unwrap());
            }
            let rows_payload = crate::property_graph::storage::stream::PayloadSlice::new(
                source, store, generation, row_table,
            );
            for row in 0..rows {
                let mut row_bytes = [0_u8; 80];
                assert_eq!(
                    rows_payload
                        .read_at(u64::from(row) * 80, &mut row_bytes, resources)
                        .unwrap(),
                    row_bytes.len(),
                );
                let record = crate::property_graph::storage::payload::PayloadRef::decode(
                    row_bytes.get(24..72).unwrap(),
                )
                .unwrap();
                expected_payload_closure(source, record, resources, &mut output);
                let record_payload = crate::property_graph::storage::stream::PayloadSlice::new(
                    source, store, generation, record,
                );
                let mut record_bytes = vec![0_u8; usize::try_from(record.len()).unwrap()];
                assert_eq!(
                    record_payload
                        .read_at(0, &mut record_bytes, resources)
                        .unwrap(),
                    record_bytes.len(),
                );
                let references = record_bytes.get(record_bytes.len() - 96..).unwrap();
                let canonical = crate::property_graph::storage::payload::PayloadRef::decode(
                    references.get(..48).unwrap(),
                )
                .unwrap();
                let provenance = crate::property_graph::storage::payload::PayloadRef::decode(
                    references.get(48..).unwrap(),
                )
                .unwrap();
                expected_payload_closure(source, canonical, resources, &mut output);
                expected_payload_closure(source, provenance, resources, &mut output);
            }
        }
    }
    output.sort_by_key(physical_order);
    output
}

fn collect_installed_trace<'s, 'lease, 'm>(
    source: &'s crate::property_graph::storage::NativePreparationSource<'lease, 'm>,
    catalog: &'s crate::property_graph::storage::NativePreparationCatalog<'s, 'lease, 'm>,
    resources: &mut TreeResources<'m>,
    capacity: usize,
) -> Vec<PhysicalRef> {
    let mut cursor = crate::property_graph::storage::search::SearchTraceCursor::for_preparation(
        source, catalog, resources,
    )
    .unwrap();
    let mut output = Vec::new();
    loop {
        let mut batch = vec![None; capacity];
        let result = cursor.trace(&mut batch, resources).unwrap();
        assert!(result.count <= capacity);
        output.extend(batch.into_iter().take(result.count).map(Option::unwrap));
        if result.complete {
            break;
        }
    }
    output.sort_by_key(physical_order);
    output
}

fn run_sparse_lifecycle_acceptance(
    check_corruption: bool,
    check_checkpoint: bool,
    check_required_bytes: bool,
    check_trace: bool,
    check_resources: bool,
) {
    use crate::property_graph::storage::search::{Modality, SparseRoots, SparseView};
    use crate::property_graph::storage::{
        GraphPreparation, NativePreparationCatalog, NativePreparationSource,
    };

    let directory = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(
        Store::open(
            directory.path(),
            OpenOptions::new()
                .with_max_resident_bytes(256 * 1024 * 1024)
                .with_reader_drain_timeout(std::time::Duration::ZERO),
        )
        .unwrap(),
    );
    let identity = StoreInstanceId::new(61_200).unwrap();
    let document = crate::epoch::EmbeddingTower {
        model_id: "ze61-cow-document".into(),
        model_version: "1".into(),
        weights_digest: vec![0x61, 0xc0],
        dims: 2,
        normalization: crate::epoch::Normalization::None,
        prompt_prefix: "doc: ".into(),
        max_tokens: 32,
        runtime: crate::epoch::EmbeddingRuntime::CpuReference,
        compute_units: crate::epoch::ComputeUnits::Cpu,
        os_build: None,
    };
    let base_identity = BaseIdentity {
        store: identity,
        generation: GraphGeneration::new(0),
        fold: Default::default(),
        roots: Some(ArtifactId::new(61_201).unwrap()),
    };
    let namespace = SymbolEntry {
        symbol: Symbol::Namespace(NamespaceId::new(1).unwrap()),
        name: GraphName::new("app").unwrap(),
    };
    let base_high = StageHighWaters {
        node: 1_u128 << 64,
        relationship: 0,
        symbols: SymbolHighWaters {
            namespace: 1,
            ..SymbolHighWaters::default()
        },
    };
    let mut fixture0 = SparseFixture::empty(base_identity, base_high, Some(document.clone()));
    fixture0.symbols.push(namespace);
    let catalog0 = write_complete_sparse_catalog(
        directory.path(),
        ArtifactIdentity {
            store: identity,
            artifact: ArtifactId::new(61_202).unwrap(),
            generation: GraphGeneration::new(0),
            creation_serial: 2,
        },
        base_high,
        &fixture0.symbols,
        Some(&document),
    );
    let root0 = write_framed_file(
        directory.path(),
        ContainerKind::RootEnvelope,
        ArtifactIdentity {
            store: identity,
            artifact: ArtifactId::new(61_201).unwrap(),
            generation: GraphGeneration::new(0),
            creation_serial: 1,
        },
        &[Block {
            kind: BlockKind::CheckpointPayload,
            payload: b"ze61-cow-base",
        }],
    );
    let mut input0 = bundle(identity, 0, 61_201);
    input0.base = base_identity;
    input0.root_envelope = root0;
    input0.catalog = catalog0;
    input0.high_waters = HighWaters {
        node: 1_u128 << 64,
        relationship: 0,
        symbols: [0, 0, 0, 1],
        creation_serial: 2,
    };
    input0.document = Some(document.clone());
    store.install_native_graph_for_test(input0).unwrap();

    if check_resources {
        struct ObserveInitialSparse;
        impl NativeReadConsumer<u64> for ObserveInitialSparse {
            fn consume<'s, 'lease, 'm, 'g>(
                &mut self,
                view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
                runtime: &mut crate::property_graph::query::runtime::RuntimeContext<'lease, 'm, 'g>,
            ) -> Result<u64, TreeError> {
                let sparse = view.sparse_view(runtime)?;
                let mut resources = TreeResources::for_query(runtime)?;
                let mut text = sparse.sources(Modality::Text, &mut resources)?;
                let mut vector = sparse.sources(Modality::Vector, &mut resources)?;
                if sparse.text_count() != 0
                    || sparse.vector_count() != 0
                    || text.next(&mut resources)?.is_some()
                    || vector.next(&mut resources)?.is_some()
                {
                    return Err(TreeError::Invalid("nonempty initial sparse query state"));
                }
                Ok(sparse.sequence())
            }
        }
        assert_eq!(
            store
                .with_native_read(
                    &QueryControl::Cancel(CancelToken::new()),
                    crate::property_graph::query::runtime::RuntimeLimits::default(),
                    2 * 1024 * 1024,
                    8,
                    ObserveInitialSparse,
                )
                .unwrap(),
            0
        );
    }

    let old_vector = [f32::from_bits(0x3f00_0001), -0.0];
    let old_embedding_b = CanonicalEmbedding::new(&document, &old_vector).unwrap();
    let old_a = CanonicalContents::node(&mut [], &mut [], Some("old zeppelin"), None).unwrap();
    let old_b = CanonicalContents::node(
        &mut [],
        &mut [],
        Some("old zeppelin"),
        Some(old_embedding_b),
    )
    .unwrap();
    let extent_text = "extent ".repeat(9_500);
    assert!(extent_text.len() > 64 * 1024);
    let old_c = CanonicalContents::node(&mut [], &mut [], Some(&extent_text), None).unwrap();
    let create = [
        StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", "a").unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&old_a)),
        },
        StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", "b").unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&old_b)),
        },
        StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", "unrelated-extent").unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&old_c)),
        },
    ];
    let shared = GraphResources::from_store(&store).unwrap();
    let writer1 = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let staged1 = stage_structured(&fixture0, &create, &writer1, &mut |_| Ok(())).unwrap();
    let node_a = match staged1.receipts()[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("node create returned relationship"),
    };
    let node_b = match staged1.receipts()[1].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("node create returned relationship"),
    };
    let node_c = match staged1.receipts()[2].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("node create returned relationship"),
    };
    let lease0 = store.admit_native_read().unwrap();
    let control1 = QueryControl::Cancel(CancelToken::new());
    let memory1 = StorageMemory::new(&writer1, &control1, 32 * 1024 * 1024).unwrap();
    let source0 = NativePreparationSource::new(&lease0, &memory1, 64).unwrap();
    let mut resources1 = source0.resources(u64::MAX).unwrap();
    let mut artifact1 = 61_300_u128;
    let mut serial1 = 3_u64;
    let prepared1 = GraphPreparation::new(
        &source0,
        GraphGeneration::new(1),
        || {
            let output = ArtifactIdentity {
                store: identity,
                artifact: ArtifactId::new(artifact1)?,
                generation: GraphGeneration::new(1),
                creation_serial: serial1,
            };
            artifact1 += 1;
            serial1 += 1;
            Ok(output)
        },
        PackLimits::default(),
        &store.tokenizer,
        &mut resources1,
    )
    .unwrap()
    .prepare(&staged1, &mut resources1)
    .unwrap_or_else(|failure| panic!("generation one failed: {}", failure.error()));
    let target_identity1 = BaseIdentity {
        store: identity,
        generation: GraphGeneration::new(1),
        fold: Default::default(),
        roots: Some(ArtifactId::new(61_399).unwrap()),
    };
    let fixture1 = fixture0.after(&staged1, target_identity1);
    let catalog1 = write_complete_sparse_catalog(
        directory.path(),
        ArtifactIdentity {
            store: identity,
            artifact: ArtifactId::new(61_398).unwrap(),
            generation: GraphGeneration::new(1),
            creation_serial: 10_000,
        },
        fixture1.high,
        &fixture1.symbols,
        Some(&document),
    );
    let input1 = materialize_sparse_generation(
        prepared1,
        lease0.bundle(),
        directory.path(),
        61_399,
        catalog1,
        fixture1.high.node,
        fixture1.high.relationship,
        fixture1.high.symbols,
        lease0.bundle().lexical(),
        Some(document.clone()),
    );
    drop(resources1);
    drop(source0);
    drop(lease0);
    store.install_native_graph_for_test(input1).unwrap();

    let replacement_vector = [f32::from_bits(0x3f80_0001), f32::from_bits(0x8000_0000)];
    let replacement_embedding_a = CanonicalEmbedding::new(&document, &replacement_vector).unwrap();
    let replacement_embedding_b = CanonicalEmbedding::new(&document, &replacement_vector).unwrap();
    let replacement_a =
        CanonicalContents::node(&mut [], &mut [], None, Some(replacement_embedding_a)).unwrap();
    let replacement_b = CanonicalContents::node(
        &mut [],
        &mut [],
        Some("new silver"),
        Some(replacement_embedding_b),
    )
    .unwrap();
    let replace = [
        StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", "a").unwrap(),
            revision: GraphRevision::new(2).unwrap(),
            operation: StructuredOperation::Put(EntityId::Node(node_a)),
            image: Some(WriteImage::Node(&replacement_a)),
        },
        StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", "b").unwrap(),
            revision: GraphRevision::new(2).unwrap(),
            operation: StructuredOperation::Put(EntityId::Node(node_b)),
            image: Some(WriteImage::Node(&replacement_b)),
        },
    ];
    let writer2 = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let staged2 = stage_structured(&fixture1, &replace, &writer2, &mut |_| Ok(())).unwrap();
    let lease1 = store.admit_native_read().unwrap();
    let control2 = QueryControl::Cancel(CancelToken::new());
    let memory2 = StorageMemory::new(&writer2, &control2, 32 * 1024 * 1024).unwrap();

    {
        let prepare_cancel = CancelToken::new();
        let failure_control = QueryControl::Cancel(prepare_cancel.clone());
        let failure_memory =
            StorageMemory::new(&writer2, &failure_control, 32 * 1024 * 1024).unwrap();
        let failure_baseline = failure_memory.reserved_bytes();
        let failed_source = NativePreparationSource::new(&lease1, &failure_memory, 64).unwrap();
        let mut failed_resources = failed_source.resources(u64::MAX).unwrap();
        let mut calls = 0_u64;
        let cancel_in_prepare = prepare_cancel.clone();
        let failed = match GraphPreparation::new(
            &failed_source,
            GraphGeneration::new(2),
            || {
                calls += 1;
                if calls == 2 {
                    cancel_in_prepare.cancel();
                }
                Ok(ArtifactIdentity {
                    store: identity,
                    artifact: ArtifactId::new(61_450 + u128::from(calls))?,
                    generation: GraphGeneration::new(2),
                    creation_serial: lease1.bundle().high_waters().creation_serial + calls,
                })
            },
            PackLimits {
                artifact_bytes: crate::property_graph::storage::artifact::MAX_ARTIFACT_BYTES,
                blocks: 1,
                ..PackLimits::default()
            },
            &store.tokenizer,
            &mut failed_resources,
        )
        .unwrap()
        .prepare(&staged2, &mut failed_resources)
        {
            Ok(_) => panic!("mid-preparation cancellation was ignored"),
            Err(failure) => failure,
        };
        assert!(matches!(
            failed.error(),
            TreeError::Control(crate::lifecycle::QueryError::Cancelled { partial: false })
        ));
        let (error, objects, retained) = failed.into_parts();
        assert!(matches!(
            error,
            TreeError::Control(crate::lifecycle::QueryError::Cancelled { partial: false })
        ));
        assert!(!objects.is_empty());
        assert_eq!(objects.abort_inventory().count(), objects.len());
        assert_eq!(retained.bundle().base(), lease1.bundle().base());
        assert_eq!(retained.bundle().text(), lease1.bundle().text());
        assert_eq!(retained.bundle().vector(), lease1.bundle().vector());
        drop(retained);
        drop(objects);
        drop(failed_resources);
        drop(failed_source);
        assert_eq!(failure_memory.reserved_bytes(), failure_baseline);
    }

    let source1 = NativePreparationSource::new(&lease1, &memory2, 128).unwrap();
    let mut resources2 = source1.resources(u64::MAX).unwrap();
    let mut artifact2 = 61_500_u128;
    let mut serial2 = lease1.bundle().high_waters().creation_serial + 1;
    let prepared2 = GraphPreparation::new(
        &source1,
        GraphGeneration::new(2),
        || {
            let output = ArtifactIdentity {
                store: identity,
                artifact: ArtifactId::new(artifact2)?,
                generation: GraphGeneration::new(2),
                creation_serial: serial2,
            };
            artifact2 += 1;
            serial2 += 1;
            Ok(output)
        },
        PackLimits::default(),
        &store.tokenizer,
        &mut resources2,
    )
    .unwrap()
    .prepare(&staged2, &mut resources2)
    .unwrap_or_else(|failure| panic!("generation two failed: {}", failure.error()));
    assert_eq!(prepared2.membership_changes().len(), 2);
    assert_eq!(
        prepared2.membership_changes()[0].membership,
        Some(crate::property_graph::wal::Membership {
            text_before: true,
            text_after: false,
            vector_before: false,
            vector_after: true,
        })
    );
    assert_eq!(
        prepared2.membership_changes()[1].membership,
        Some(crate::property_graph::wal::Membership {
            text_before: true,
            text_after: true,
            vector_before: true,
            vector_after: true,
        })
    );
    let catalog_view1 = NativePreparationCatalog::open(&source1, &mut resources2).unwrap();
    let old_view = SparseView::open(
        &source1,
        SparseRoots {
            text: lease1.bundle().text(),
            vector: lease1.bundle().vector(),
        },
        lease1.bundle().roots(),
        lease1.bundle().catalog(),
        &catalog_view1,
        lease1.bundle().document(),
        lease1.bundle().lexical(),
        &memory2,
        &mut resources2,
    )
    .unwrap();
    assert_eq!((old_view.text_count(), old_view.vector_count()), (3, 1));
    let new_view = SparseView::open(
        prepared2.objects(),
        prepared2.sparse_roots(),
        prepared2.candidate().roots(),
        lease1.bundle().catalog(),
        &catalog_view1,
        lease1.bundle().document(),
        lease1.bundle().lexical(),
        &memory2,
        &mut resources2,
    )
    .unwrap();
    assert_eq!((new_view.text_count(), new_view.vector_count()), (2, 2));
    assert!(
        new_view
            .lookup(Modality::Text, node_a, &mut resources2)
            .unwrap()
            .is_none()
    );
    assert!(
        new_view
            .lookup(Modality::Vector, node_a, &mut resources2)
            .unwrap()
            .is_some()
    );
    let current_b = new_view
        .lookup(Modality::Vector, node_b, &mut resources2)
        .unwrap()
        .unwrap()
        .vector
        .unwrap();
    assert_eq!(
        current_b.coordinate(0, &mut resources2).unwrap().to_bits(),
        0x3f80_0001
    );
    assert_eq!(
        current_b.coordinate(1, &mut resources2).unwrap().to_bits(),
        0x8000_0000
    );
    assert!(
        old_view
            .lookup(Modality::Text, node_a, &mut resources2)
            .unwrap()
            .is_some()
    );
    assert!(
        new_view
            .lookup(Modality::Text, node_c, &mut resources2)
            .unwrap()
            .is_some()
    );

    if check_checkpoint {
        use crate::property_graph::storage::search::{
            SparseCheckpoint, validate_checkpoint, validate_replay_transition,
        };
        let checkpoint = SparseCheckpoint {
            cutoff: lease1.bundle().sequence(),
            roots: SparseRoots {
                text: lease1.bundle().text(),
                vector: lease1.bundle().vector(),
            },
        };
        assert_eq!(old_view.checkpoint(), 0);
        assert_eq!(new_view.checkpoint(), 0);
        assert_eq!(old_view.text_length(), 9_504);
        assert_eq!(new_view.text_length(), 9_502);
        assert_eq!(
            validate_checkpoint(
                prepared2.objects(),
                checkpoint,
                lease1.bundle().roots(),
                lease1.bundle().catalog(),
                &catalog_view1,
                lease1.bundle().document(),
                lease1.bundle().lexical(),
                &memory2,
                &mut resources2,
            )
            .unwrap(),
            (3, 1)
        );
        assert_eq!(
            validate_replay_transition(
                prepared2.objects(),
                checkpoint,
                lease1.bundle().roots(),
                prepared2.sparse_roots(),
                prepared2.candidate().roots(),
                lease1.bundle().catalog(),
                &catalog_view1,
                lease1.bundle().document(),
                lease1.bundle().lexical(),
                &staged2,
                prepared2.membership_changes(),
                &memory2,
                &mut resources2,
            )
            .unwrap(),
            (2, 2)
        );
        let wrong_cutoff = SparseCheckpoint {
            cutoff: checkpoint.cutoff + 1,
            ..checkpoint
        };
        assert!(matches!(
            validate_checkpoint(
                prepared2.objects(),
                wrong_cutoff,
                lease1.bundle().roots(),
                lease1.bundle().catalog(),
                &catalog_view1,
                lease1.bundle().document(),
                lease1.bundle().lexical(),
                &memory2,
                &mut resources2,
            ),
            Err(TreeError::Invalid("sparse checkpoint cutoff"))
        ));
        let mut wrong_order = prepared2.membership_changes().to_vec();
        wrong_order.swap(0, 1);
        assert!(matches!(
            validate_replay_transition(
                prepared2.objects(),
                checkpoint,
                lease1.bundle().roots(),
                prepared2.sparse_roots(),
                prepared2.candidate().roots(),
                lease1.bundle().catalog(),
                &catalog_view1,
                lease1.bundle().document(),
                lease1.bundle().lexical(),
                &staged2,
                &wrong_order,
                &memory2,
                &mut resources2,
            ),
            Err(TreeError::Invalid("sparse replay delta order"))
        ));
        assert!(matches!(
            validate_replay_transition(
                prepared2.objects(),
                checkpoint,
                lease1.bundle().roots(),
                prepared2.sparse_roots(),
                prepared2.candidate().roots(),
                lease1.bundle().catalog(),
                &catalog_view1,
                lease1.bundle().document(),
                lease1.bundle().lexical(),
                &staged2,
                &prepared2.membership_changes()[..1],
                &memory2,
                &mut resources2,
            ),
            Err(TreeError::Invalid("sparse replay change cardinality"))
        ));
        let mut invented = prepared2.membership_changes().to_vec();
        invented.push(invented[1]);
        assert!(matches!(
            validate_replay_transition(
                prepared2.objects(),
                checkpoint,
                lease1.bundle().roots(),
                prepared2.sparse_roots(),
                prepared2.candidate().roots(),
                lease1.bundle().catalog(),
                &catalog_view1,
                lease1.bundle().document(),
                lease1.bundle().lexical(),
                &staged2,
                &invented,
                &memory2,
                &mut resources2,
            ),
            Err(TreeError::Invalid("sparse replay change cardinality"))
        ));
        let mut mismatched = prepared2.membership_changes().to_vec();
        mismatched[0].membership.as_mut().unwrap().text_after = true;
        assert!(matches!(
            validate_replay_transition(
                prepared2.objects(),
                checkpoint,
                lease1.bundle().roots(),
                prepared2.sparse_roots(),
                prepared2.candidate().roots(),
                lease1.bundle().catalog(),
                &catalog_view1,
                lease1.bundle().document(),
                lease1.bundle().lexical(),
                &staged2,
                &mismatched,
                &memory2,
                &mut resources2,
            ),
            Err(TreeError::Invalid("sparse replay analyzed membership"))
        ));
        let alternative_embedding =
            CanonicalEmbedding::new(&document, &replacement_vector).unwrap();
        let alternative_b = CanonicalContents::node(
            &mut [],
            &mut [],
            Some("different silver"),
            Some(alternative_embedding),
        )
        .unwrap();
        let alternative_writes = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "a").unwrap(),
                revision: GraphRevision::new(2).unwrap(),
                operation: StructuredOperation::Put(EntityId::Node(node_a)),
                image: Some(WriteImage::Node(&replacement_a)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "b").unwrap(),
                revision: GraphRevision::new(2).unwrap(),
                operation: StructuredOperation::Put(EntityId::Node(node_b)),
                image: Some(WriteImage::Node(&alternative_b)),
            },
        ];
        let alternative_writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
        let alternative = stage_structured(
            &fixture1,
            &alternative_writes,
            &alternative_writer,
            &mut |_| Ok(()),
        )
        .unwrap();
        assert!(matches!(
            validate_replay_transition(
                prepared2.objects(),
                checkpoint,
                lease1.bundle().roots(),
                prepared2.sparse_roots(),
                prepared2.candidate().roots(),
                lease1.bundle().catalog(),
                &catalog_view1,
                lease1.bundle().document(),
                lease1.bundle().lexical(),
                &alternative,
                prepared2.membership_changes(),
                &memory2,
                &mut resources2,
            ),
            Err(TreeError::Invalid("sparse replay canonical origin"))
        ));
    }

    if check_corruption || check_required_bytes {
        let text_root = prepared2.sparse_roots().text.unwrap();
        let root_block = crate::property_graph::storage::tree::directory::BlockSource::resolve(
            prepared2.objects(),
            text_root.block,
            &mut resources2,
        )
        .unwrap();
        assert_eq!(root_block.payload().get(160).copied(), Some(1));
        let text_members =
            artifact::decode_reference(root_block.payload().get(168..200).unwrap()).unwrap();
        let member_block = crate::property_graph::storage::tree::directory::BlockSource::resolve(
            prepared2.objects(),
            text_members,
            &mut resources2,
        )
        .unwrap();
        let member_page = crate::property_graph::storage::tree::decode_page(
            crate::property_graph::storage::tree::TreeKind::SparseMembership,
            member_block.payload(),
        )
        .unwrap();
        let crate::property_graph::storage::tree::Cell::Leaf { value, .. } =
            member_page.cell(0).unwrap()
        else {
            panic!("text membership root is not a leaf");
        };
        let source_reference = artifact::decode_reference(value.get(8..40).unwrap()).unwrap();
        let text_sources =
            artifact::decode_reference(root_block.payload().get(208..240).unwrap()).unwrap();
        let sources_block = crate::property_graph::storage::tree::directory::BlockSource::resolve(
            prepared2.objects(),
            text_sources,
            &mut resources2,
        )
        .unwrap();
        let sources_page = crate::property_graph::storage::tree::decode_page(
            crate::property_graph::storage::tree::TreeKind::SparseSources,
            sources_block.payload(),
        )
        .unwrap();
        let crate::property_graph::storage::tree::Cell::Leaf {
            value: source_value,
            ..
        } = sources_page.cell(0).unwrap()
        else {
            panic!("text source root is not a leaf");
        };
        let text_mask = crate::property_graph::storage::payload::PayloadRef::decode(
            source_value.get(0..48).unwrap(),
        )
        .unwrap()
        .reference();
        #[derive(Clone, Copy, Debug)]
        enum Corruption {
            FullIdentity,
            Revision,
            Ordinal,
            Source,
            Modality,
            CanonicalReference,
            FutureGeneration,
            WrongInterpretation,
        }
        let cases = [
            (
                Corruption::FullIdentity,
                "sparse membership/row correlation",
            ),
            (Corruption::Revision, "sparse membership/row correlation"),
            (Corruption::Ordinal, "sparse source cutoff or row geometry"),
            (Corruption::Source, "sparse member source is absent"),
            (Corruption::Modality, "sparse source geometry or modality"),
            (
                Corruption::CanonicalReference,
                "sparse/native record mismatch",
            ),
            (
                Corruption::FutureGeneration,
                "sparse source cutoff or row geometry",
            ),
            (
                Corruption::WrongInterpretation,
                "sparse root interpretation mismatch",
            ),
        ];
        for (case, expected) in cases {
            let mut corrupt = None;
            for index in 0..prepared2.objects().len() {
                let object = prepared2.objects().artifact(index).unwrap();
                let bytes = match case {
                    Corruption::FullIdentity => {
                        rewrite_block(object.bytes(), BlockKind::RetrievalRows, |payload| {
                            let truncated = node_b.get() as u64 as u128;
                            payload
                                .get_mut(0..16)
                                .unwrap()
                                .copy_from_slice(&truncated.to_le_bytes());
                        })
                    }
                    Corruption::Revision => {
                        rewrite_block(object.bytes(), BlockKind::RetrievalRows, |payload| {
                            payload
                                .get_mut(16..24)
                                .unwrap()
                                .copy_from_slice(&1_u64.to_le_bytes());
                        })
                    }
                    Corruption::CanonicalReference => {
                        rewrite_block(object.bytes(), BlockKind::RetrievalRows, |payload| {
                            payload
                                .get_mut(40..56)
                                .unwrap()
                                .copy_from_slice(&61_999_u128.to_le_bytes());
                        })
                    }
                    Corruption::Ordinal => {
                        rewrite_sparse_membership(object.bytes(), text_members, |_, value| {
                            value
                                .get_mut(40..44)
                                .unwrap()
                                .copy_from_slice(&1_u32.to_le_bytes());
                        })
                    }
                    Corruption::Source => {
                        rewrite_sparse_membership(object.bytes(), text_members, |_, value| {
                            value
                                .get_mut(8..24)
                                .unwrap()
                                .copy_from_slice(&61_999_u128.to_le_bytes());
                        })
                    }
                    Corruption::Modality => rewrite_matching_block(
                        object.bytes(),
                        BlockKind::CommitParticipant,
                        |payload| {
                            if payload.get(8).copied() != Some(2)
                                || payload.get(9).copied() != Some(Modality::Text as u8)
                            {
                                return false;
                            }
                            *payload.get_mut(9).unwrap() = Modality::Vector as u8;
                            true
                        },
                    ),
                    Corruption::FutureGeneration => rewrite_matching_block(
                        object.bytes(),
                        BlockKind::CommitParticipant,
                        |payload| {
                            if payload.get(8).copied() != Some(2)
                                || payload.get(9).copied() != Some(Modality::Text as u8)
                            {
                                return false;
                            }
                            payload
                                .get_mut(16..24)
                                .unwrap()
                                .copy_from_slice(&3_u64.to_le_bytes());
                            true
                        },
                    ),
                    Corruption::WrongInterpretation => rewrite_matching_block(
                        object.bytes(),
                        BlockKind::CommitParticipant,
                        |payload| {
                            if payload.get(8).copied() != Some(1)
                                || payload.get(9).copied() != Some(Modality::Text as u8)
                            {
                                return false;
                            }
                            let wrong = lease1.bundle().lexical().value() + 1;
                            payload
                                .get_mut(56..64)
                                .unwrap()
                                .copy_from_slice(&wrong.to_le_bytes());
                            true
                        },
                    ),
                };
                if let Some(bytes) = bytes {
                    corrupt = Some((object.identity().artifact, bytes));
                    break;
                }
            }
            let (artifact_id, bytes) = corrupt.unwrap_or_else(|| panic!("missing {case:?} target"));
            let corrupt_roots =
                roots_for_rewritten_artifact(prepared2.sparse_roots(), artifact_id, &bytes);
            let corrupt_source = OverrideArtifactsSource {
                base: prepared2.objects(),
                artifacts: vec![(artifact_id, bytes)],
            };
            let observed = match SparseView::open(
                &corrupt_source,
                corrupt_roots,
                prepared2.candidate().roots(),
                lease1.bundle().catalog(),
                &catalog_view1,
                lease1.bundle().document(),
                lease1.bundle().lexical(),
                &memory2,
                &mut resources2,
            ) {
                Ok(view) => match view.lookup(Modality::Text, node_b, &mut resources2) {
                    Ok(_) => panic!("{case:?} corruption was accepted"),
                    Err(error) => error,
                },
                Err(error) => error,
            };
            match observed {
                TreeError::Invalid(detail) => assert_eq!(detail, expected, "{case:?}"),
                other => panic!("{case:?} returned {other}"),
            }
        }

        let native_nodes = prepared2
            .candidate()
            .roots()
            .directory(crate::property_graph::storage::tree::TreeKind::Nodes)
            .unwrap()
            .reference()
            .unwrap();
        let native_object = (0..prepared2.objects().len())
            .map(|index| prepared2.objects().artifact(index).unwrap())
            .find(|object| object.identity().artifact == native_nodes.artifact)
            .unwrap();
        let native_bytes = rewrite_directory_without_key(
            native_object.bytes(),
            native_nodes,
            crate::property_graph::storage::tree::TreeKind::Nodes,
            &node_b.get().to_le_bytes(),
        )
        .unwrap();
        let missing_roots = roots_for_rewritten_artifact(
            prepared2.sparse_roots(),
            native_nodes.artifact,
            &native_bytes,
        );
        let missing_source = OverrideArtifactsSource {
            base: prepared2.objects(),
            artifacts: vec![(native_nodes.artifact, native_bytes)],
        };
        let missing_view = SparseView::open(
            &missing_source,
            missing_roots,
            prepared2.candidate().roots(),
            lease1.bundle().catalog(),
            &catalog_view1,
            lease1.bundle().document(),
            lease1.bundle().lexical(),
            &memory2,
            &mut resources2,
        )
        .unwrap();
        assert!(matches!(
            missing_view.lookup(Modality::Text, node_b, &mut resources2),
            Err(TreeError::Invalid("sparse native node is absent"))
        ));

        if check_required_bytes {
            #[derive(Clone, Copy, Debug)]
            enum RequiredCorruption {
                Role,
                Version,
                LegacyVersion,
                Modality,
                Reserved,
                ShortRows,
                ExtraRows,
                MissingLexical,
                MaskTail,
                SourceCount,
                RootTotal,
            }
            // ZE-158 split the source manifest's header check: the role and
            // subtype are checked first, then the (version, width) pair, so the
            // two corruptions no longer share one combined message.
            let required_cases = [
                (
                    RequiredCorruption::Role,
                    Some("sparse source role or subtype"),
                ),
                (
                    RequiredCorruption::Version,
                    Some("sparse source version or width"),
                ),
                (
                    RequiredCorruption::LegacyVersion,
                    Some("sparse source version or width"),
                ),
                (RequiredCorruption::Modality, Some("sparse modality")),
                (
                    RequiredCorruption::Reserved,
                    Some("sparse source reserved or width"),
                ),
                (
                    RequiredCorruption::ShortRows,
                    Some("sparse source geometry or modality"),
                ),
                (
                    RequiredCorruption::ExtraRows,
                    Some("sparse source geometry or modality"),
                ),
                (RequiredCorruption::MissingLexical, None),
                (RequiredCorruption::MaskTail, Some("sparse mask tail bits")),
                (
                    RequiredCorruption::SourceCount,
                    Some("sparse source aggregate mismatch"),
                ),
                (
                    RequiredCorruption::RootTotal,
                    Some("sparse root aggregate mismatch"),
                ),
            ];
            for (case, expected) in required_cases {
                let mut corrupt = None;
                for index in 0..prepared2.objects().len() {
                    let object = prepared2.objects().artifact(index).unwrap();
                    let bytes = match case {
                        RequiredCorruption::Role
                        | RequiredCorruption::Version
                        | RequiredCorruption::LegacyVersion
                        | RequiredCorruption::Modality
                        | RequiredCorruption::Reserved
                        | RequiredCorruption::ShortRows
                        | RequiredCorruption::ExtraRows
                        | RequiredCorruption::MissingLexical => {
                            rewrite_selected_block(object.bytes(), |reference, payload| {
                                if reference != source_reference {
                                    return false;
                                }
                                match case {
                                    RequiredCorruption::Role => payload
                                        .get_mut(4..6)
                                        .unwrap()
                                        .copy_from_slice(&7_u16.to_le_bytes()),
                                    // 2 is the live V2 version, so it would be a
                                    // no-op here. 3 is unknown at any width and
                                    // 1 is legal only at the 144-byte V1 width.
                                    RequiredCorruption::Version => payload
                                        .get_mut(6..8)
                                        .unwrap()
                                        .copy_from_slice(&3_u16.to_le_bytes()),
                                    RequiredCorruption::LegacyVersion => payload
                                        .get_mut(6..8)
                                        .unwrap()
                                        .copy_from_slice(&1_u16.to_le_bytes()),
                                    RequiredCorruption::Modality => {
                                        *payload.get_mut(9).unwrap() = 9
                                    }
                                    RequiredCorruption::Reserved => {
                                        *payload.get_mut(10).unwrap() = 1
                                    }
                                    RequiredCorruption::ShortRows => payload
                                        .get_mut(48..56)
                                        .unwrap()
                                        .copy_from_slice(&79_u64.to_le_bytes()),
                                    RequiredCorruption::ExtraRows => payload
                                        .get_mut(48..56)
                                        .unwrap()
                                        .copy_from_slice(&81_u64.to_le_bytes()),
                                    RequiredCorruption::MissingLexical => payload
                                        .get_mut(112..128)
                                        .unwrap()
                                        .copy_from_slice(&61_999_u128.to_le_bytes()),
                                    _ => unreachable!(),
                                }
                                true
                            })
                        }
                        RequiredCorruption::MaskTail => {
                            rewrite_selected_block(object.bytes(), |reference, payload| {
                                if reference != text_mask {
                                    return false;
                                }
                                *payload.get_mut(0).unwrap() |= 0x80;
                                true
                            })
                        }
                        RequiredCorruption::SourceCount => {
                            rewrite_sparse_source_value(object.bytes(), text_sources, |_, value| {
                                value
                                    .get_mut(48..56)
                                    .unwrap()
                                    .copy_from_slice(&2_u64.to_le_bytes());
                            })
                        }
                        RequiredCorruption::RootTotal => {
                            rewrite_selected_block(object.bytes(), |reference, payload| {
                                if reference != text_root.block {
                                    return false;
                                }
                                payload
                                    .get_mut(240..248)
                                    .unwrap()
                                    .copy_from_slice(&3_u64.to_le_bytes());
                                true
                            })
                        }
                    };
                    if let Some(bytes) = bytes {
                        corrupt = Some((object.identity().artifact, bytes));
                        break;
                    }
                }
                let (artifact_id, bytes) =
                    corrupt.unwrap_or_else(|| panic!("missing {case:?} target"));
                let roots =
                    roots_for_rewritten_artifact(prepared2.sparse_roots(), artifact_id, &bytes);
                let source = OverrideArtifactsSource {
                    base: prepared2.objects(),
                    artifacts: vec![(artifact_id, bytes)],
                };
                let observed = match SparseView::open(
                    &source,
                    roots,
                    prepared2.candidate().roots(),
                    lease1.bundle().catalog(),
                    &catalog_view1,
                    lease1.bundle().document(),
                    lease1.bundle().lexical(),
                    &memory2,
                    &mut resources2,
                ) {
                    Ok(view) => view
                        .validate_all(Modality::Text, &mut resources2)
                        .map(|_| ()),
                    Err(error) => Err(error),
                };
                match (observed, expected) {
                    (Err(TreeError::Invalid(detail)), Some(expected)) => {
                        assert_eq!(detail, expected, "{case:?}");
                    }
                    (Err(_), None) => {}
                    (Err(other), Some(expected)) => {
                        panic!("{case:?} returned {other}, expected {expected}");
                    }
                    (Ok(()), _) => panic!("{case:?} corruption was accepted"),
                }
            }
            {
                let family = FormatFamily::NativeGraphObject.id() + 1;
                let mut roots = prepared2.sparse_roots();
                roots.text.as_mut().unwrap().object.family = family;
                assert!(matches!(
                    SparseView::open(
                        prepared2.objects(),
                        roots,
                        prepared2.candidate().roots(),
                        lease1.bundle().catalog(),
                        &catalog_view1,
                        lease1.bundle().document(),
                        lease1.bundle().lexical(),
                        &memory2,
                        &mut resources2,
                    ),
                    Err(TreeError::Invalid("sparse required root mismatch"))
                ));
            }
            let mut roots = prepared2.sparse_roots();
            roots.text.as_mut().unwrap().object.version = 2;
            assert!(matches!(
                SparseView::open(
                    prepared2.objects(),
                    roots,
                    prepared2.candidate().roots(),
                    lease1.bundle().catalog(),
                    &catalog_view1,
                    lease1.bundle().document(),
                    lease1.bundle().lexical(),
                    &memory2,
                    &mut resources2,
                ),
                Err(TreeError::Invalid("sparse required root mismatch"))
            ));
            let mut wrong_document = document.clone();
            wrong_document.model_version.push_str("-wrong");
            assert!(matches!(
                SparseView::open(
                    prepared2.objects(),
                    prepared2.sparse_roots(),
                    prepared2.candidate().roots(),
                    lease1.bundle().catalog(),
                    &catalog_view1,
                    Some(&wrong_document),
                    lease1.bundle().lexical(),
                    &memory2,
                    &mut resources2,
                ),
                Err(TreeError::Invalid(
                    "historical sparse catalog interpretation"
                ))
            ));
        }
    }

    let target_identity2 = BaseIdentity {
        store: identity,
        generation: GraphGeneration::new(2),
        fold: Default::default(),
        roots: Some(ArtifactId::new(61_599).unwrap()),
    };
    let fixture2 = fixture1.after(&staged2, target_identity2);
    let replay_request = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "b").unwrap(),
        revision: GraphRevision::new(2).unwrap(),
        operation: StructuredOperation::Put(EntityId::Node(node_b)),
        image: Some(WriteImage::Node(&replacement_b)),
    }];
    let replayed = stage_structured(&fixture2, &replay_request, &writer2, &mut |_| Ok(())).unwrap();
    assert_eq!(
        replayed.disposition(),
        crate::property_graph::BatchDisposition::Replayed
    );
    assert!(replayed.receipts()[0].replayed);
    assert!(
        replayed.deltas().is_empty(),
        "replay created a sparse source delta"
    );
    drop(replayed);
    let no_op = stage_structured(&fixture2, &[], &writer2, &mut |_| Ok(())).unwrap();
    assert_eq!(
        no_op.disposition(),
        crate::property_graph::BatchDisposition::NoOp
    );
    assert!(
        no_op.deltas().is_empty(),
        "no-op created a sparse source delta"
    );
    drop(no_op);
    let catalog2 = write_complete_sparse_catalog(
        directory.path(),
        ArtifactIdentity {
            store: identity,
            artifact: ArtifactId::new(61_598).unwrap(),
            generation: GraphGeneration::new(2),
            creation_serial: 20_000,
        },
        fixture2.high,
        &fixture2.symbols,
        Some(&document),
    );
    drop(new_view);
    drop(old_view);
    drop(catalog_view1);
    let input2 = materialize_sparse_generation(
        prepared2,
        lease1.bundle(),
        directory.path(),
        61_599,
        catalog2,
        fixture2.high.node,
        fixture2.high.relationship,
        fixture2.high.symbols,
        lease1.bundle().lexical(),
        Some(document.clone()),
    );
    drop(resources2);
    drop(source1);
    drop(lease1);
    store.install_native_graph_for_test(input2).unwrap();

    if check_resources {
        let resource_baseline = shared.reserved_bytes().unwrap();
        struct ObserveSparseQuery {
            node: NodeId,
            vector_only: NodeId,
            unrelated: NodeId,
            store: std::sync::Arc<Store>,
        }
        impl NativeReadConsumer<(u64, u64, u64, u64, usize)> for ObserveSparseQuery {
            fn consume<'s, 'lease, 'm, 'g>(
                &mut self,
                view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
                runtime: &mut crate::property_graph::query::runtime::RuntimeContext<'lease, 'm, 'g>,
            ) -> Result<(u64, u64, u64, u64, usize), TreeError> {
                let baseline = runtime.memory().reserved_bytes();
                let sparse = view.sparse_view(runtime)?;
                let opened = runtime.memory().reserved_bytes();
                let mut resources = TreeResources::for_query(runtime)?;
                let text = sparse
                    .lookup(Modality::Text, self.node, &mut resources)?
                    .ok_or(TreeError::Invalid("missing admitted sparse text member"))?;
                let vector = sparse
                    .lookup(Modality::Vector, self.node, &mut resources)?
                    .ok_or(TreeError::Invalid("missing admitted sparse vector member"))?;
                let bits = vector
                    .vector
                    .ok_or(TreeError::Invalid("missing admitted sparse vector payload"))?
                    .coordinate(0, &mut resources)?
                    .to_bits() as u64;
                {
                    let mut text_sources = sparse.sources(Modality::Text, &mut resources)?;
                    let mut found_text = false;
                    let mut found_unrelated = false;
                    let mut dead_rows = 0_u32;
                    while let Some(text_source) = text_sources.next(&mut resources)? {
                        if text_source
                            .lexical(&mut resources)?
                            .is_none_or(|lexical| lexical.row_count() != text_source.row_count())
                        {
                            return Err(TreeError::Invalid(
                                "invalid admitted text source geometry",
                            ));
                        }
                        for row in 0..text_source.row_count() {
                            let live = text_source.is_live(row, &mut resources)?;
                            let source_text = text_source.resolve_row(row, &mut resources)?;
                            if !live {
                                if source_text.is_some() {
                                    return Err(TreeError::Invalid(
                                        "dead text source row resolved",
                                    ));
                                }
                                dead_rows += 1;
                                continue;
                            }
                            let source_text = source_text
                                .ok_or(TreeError::Invalid("missing admitted text source row"))?;
                            if source_text.node == text.node {
                                found_text = source_text.revision == text.revision
                                    && source_text.analyzed_length == text.analyzed_length;
                            } else if source_text.node == self.unrelated {
                                found_unrelated = true;
                            }
                        }
                    }
                    if !found_text || !found_unrelated || dead_rows != 2 {
                        return Err(TreeError::Invalid("text source/direct lookup disagreement"));
                    }
                }
                {
                    let mut vector_sources = sparse.sources(Modality::Vector, &mut resources)?;
                    let vector_source = vector_sources
                        .next(&mut resources)?
                        .ok_or(TreeError::Invalid("missing admitted vector source"))?;
                    if vector_source.row_count() != 2
                        || vector_source.lexical(&mut resources)?.is_some()
                    {
                        return Err(TreeError::Invalid(
                            "invalid admitted vector source geometry",
                        ));
                    }
                    let mut found_direct = false;
                    let mut found_vector_only = false;
                    for row in 0..vector_source.row_count() {
                        if !vector_source.is_live(row, &mut resources)? {
                            return Err(TreeError::Invalid("dead admitted vector source row"));
                        }
                        let source_vector = vector_source
                            .resolve_row(row, &mut resources)?
                            .ok_or(TreeError::Invalid("missing admitted vector source row"))?;
                        let source_bits = source_vector
                            .vector
                            .ok_or(TreeError::Invalid("missing source-local vector payload"))?
                            .coordinate(0, &mut resources)?
                            .to_bits() as u64;
                        if source_vector.node == vector.node {
                            found_direct = source_vector.revision == vector.revision
                                && source_vector.row == vector.row
                                && source_bits == bits;
                        } else if source_vector.node == self.vector_only {
                            found_vector_only = source_bits == bits;
                        }
                    }
                    if !found_direct
                        || !found_vector_only
                        || vector_sources.next(&mut resources)?.is_some()
                    {
                        return Err(TreeError::Invalid(
                            "vector source/direct lookup disagreement",
                        ));
                    }
                }
                {
                    let mut rejected = sparse.sources(Modality::Text, &mut resources)?;
                    let shared = GraphResources::from_store(&self.store)
                        .map_err(|_| TreeError::Invalid("foreign sparse resource setup"))?;
                    let control = QueryControl::Cancel(CancelToken::new());
                    let mut foreign = TreeResources::new(&control, &shared, u64::MAX)?;
                    if !matches!(
                        rejected.next(&mut foreign),
                        Err(TreeError::Invalid("query range scratch owner mismatch"))
                    ) {
                        return Err(TreeError::Invalid("foreign sparse owner was accepted"));
                    }
                }
                let observed = (
                    sparse.text_count(),
                    sparse.vector_count(),
                    u64::from(text.analyzed_length),
                    bits,
                    baseline,
                );
                drop(resources);
                drop(sparse);
                if runtime.memory().reserved_bytes() != baseline || opened < baseline {
                    return Err(TreeError::Invalid(
                        "sparse query reservations were not released",
                    ));
                }
                Ok(observed)
            }
        }

        let observed = store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                crate::property_graph::query::runtime::RuntimeLimits::default(),
                8 * 1024 * 1024,
                128,
                ObserveSparseQuery {
                    node: node_b,
                    vector_only: node_a,
                    unrelated: node_c,
                    store: std::sync::Arc::clone(&store),
                },
            )
            .unwrap();
        assert_eq!(observed.0, 2);
        assert_eq!(observed.1, 2);
        assert_eq!(observed.2, 2);
        assert_eq!(observed.3, 0x3f80_0001);
        assert_eq!(shared.reserved_bytes().unwrap(), resource_baseline);

        {
            let owner_lease = store.admit_native_read().unwrap();
            let owner_memory =
                crate::property_graph::query::resources::QueryMemory::new(&shared, 8 * 1024 * 1024)
                    .unwrap();
            let owner_control = QueryControl::Cancel(CancelToken::new());
            let mut runtime_a = crate::property_graph::query::runtime::RuntimeContext::new(
                &owner_lease,
                &owner_control,
                &owner_memory,
                crate::property_graph::query::runtime::RuntimeLimits::default(),
            )
            .unwrap();
            let capability = NativeReadCapability::admit(&owner_lease, &runtime_a).unwrap();
            let mut initial = TreeResources::for_query(&mut runtime_a).unwrap();
            let owner_source = NativeQuerySource::new(capability, &initial, 128).unwrap();
            let owner_catalog = NativeCatalog::open(&owner_source, &mut initial).unwrap();
            drop(initial);
            let owner_view = GraphReadView::new(&owner_source, &owner_catalog).unwrap();
            let owner_sparse = owner_view.sparse_view(&mut runtime_a).unwrap();
            let mut owner_resources = TreeResources::for_query(&mut runtime_a).unwrap();
            let mut owner_cursor = owner_sparse
                .sources(Modality::Text, &mut owner_resources)
                .unwrap();
            drop(owner_resources);
            let mut runtime_b = crate::property_graph::query::runtime::RuntimeContext::new(
                &owner_lease,
                &owner_control,
                &owner_memory,
                crate::property_graph::query::runtime::RuntimeLimits::default(),
            )
            .unwrap();
            let mut foreign_resources = TreeResources::for_query(&mut runtime_b).unwrap();
            assert!(matches!(
                owner_cursor.next(&mut foreign_resources),
                Err(TreeError::Invalid("query range scratch owner mismatch"))
            ));
            drop(foreign_resources);
            drop(runtime_b);
            drop(owner_cursor);
            drop(owner_sparse);
            drop(owner_view);
            drop(owner_catalog);
            drop(owner_source);
            drop(runtime_a);
            drop(owner_lease);
        }
        assert_eq!(shared.reserved_bytes().unwrap(), resource_baseline);

        struct ControlledSparseQuery {
            node: NodeId,
            cancel: Option<CancelToken>,
            clock: Option<std::sync::Arc<ManualMonotonicClock>>,
        }
        impl NativeReadConsumer<()> for ControlledSparseQuery {
            fn consume<'s, 'lease, 'm, 'g>(
                &mut self,
                view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
                runtime: &mut crate::property_graph::query::runtime::RuntimeContext<'lease, 'm, 'g>,
            ) -> Result<(), TreeError> {
                let sparse = view.sparse_view(runtime)?;
                let mut resources = TreeResources::for_query(runtime)?;
                if sparse
                    .lookup(Modality::Vector, self.node, &mut resources)?
                    .is_none()
                {
                    return Err(TreeError::Invalid("missing sparse control probe member"));
                }
                if let Some(cancel) = &self.cancel {
                    cancel.cancel();
                }
                if let Some(clock) = &self.clock {
                    clock.advance(std::time::Duration::from_secs(2));
                }
                let _ = sparse.lookup(Modality::Text, self.node, &mut resources)?;
                Err(TreeError::Invalid("sparse control did not fire in loop"))
            }
        }

        let cancel = CancelToken::new();
        let cancelled = store.with_native_read(
            &QueryControl::Cancel(cancel.clone()),
            crate::property_graph::query::runtime::RuntimeLimits::default(),
            8 * 1024 * 1024,
            128,
            ControlledSparseQuery {
                node: node_b,
                cancel: Some(cancel),
                clock: None,
            },
        );
        assert!(matches!(
            cancelled,
            Err(NativeGraphError::Read(TreeError::Runtime(
                crate::property_graph::query::runtime::RuntimeError::Value(
                    crate::property_graph::query::QueryError::Cancelled
                )
            )))
        ));
        assert_eq!(shared.reserved_bytes().unwrap(), resource_baseline);

        let clock = std::sync::Arc::new(ManualMonotonicClock::new());
        let deadline =
            Deadline::after_with_test_clock(std::time::Duration::from_secs(1), clock.clone())
                .unwrap();
        let timed_out = store.with_native_read(
            &QueryControl::Deadline(deadline),
            crate::property_graph::query::runtime::RuntimeLimits::default(),
            8 * 1024 * 1024,
            128,
            ControlledSparseQuery {
                node: node_b,
                cancel: None,
                clock: Some(clock),
            },
        );
        assert!(matches!(
            timed_out,
            Err(NativeGraphError::Read(TreeError::Runtime(
                crate::property_graph::query::runtime::RuntimeError::Value(
                    crate::property_graph::query::QueryError::Timeout
                )
            )))
        ));
        assert_eq!(shared.reserved_bytes().unwrap(), resource_baseline);

        let refused = store.with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            crate::property_graph::query::runtime::RuntimeLimits::default(),
            observed.4,
            128,
            ObserveSparseQuery {
                node: node_b,
                vector_only: node_a,
                unrelated: node_c,
                store: std::sync::Arc::clone(&store),
            },
        );
        assert!(matches!(
            refused,
            Err(NativeGraphError::Read(TreeError::Runtime(
                crate::property_graph::query::runtime::RuntimeError::Memory(
                    crate::property_graph::query::resources::MemoryError::Limit
                )
            )))
        ));
        assert_eq!(shared.reserved_bytes().unwrap(), resource_baseline);

        let limits = crate::property_graph::query::runtime::RuntimeLimits::default()
            .with_limit(
                crate::property_graph::query::runtime::WorkKind::LexicalBlocks,
                0,
            )
            .unwrap();
        let limited = store.with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            limits,
            8 * 1024 * 1024,
            128,
            ObserveSparseQuery {
                node: node_b,
                vector_only: node_a,
                unrelated: node_c,
                store: std::sync::Arc::clone(&store),
            },
        );
        assert!(matches!(
            limited,
            Err(NativeGraphError::Read(TreeError::Runtime(
                crate::property_graph::query::runtime::RuntimeError::Limit(
                    crate::property_graph::query::runtime::WorkKind::LexicalBlocks
                )
            )))
        ));
        assert_eq!(shared.reserved_bytes().unwrap(), resource_baseline);
    }

    if check_checkpoint {
        use crate::property_graph::storage::search::prepare_sparse_checkpoint;
        let lease = store.admit_native_read().unwrap();
        let checkpoint_writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
        let checkpoint_control = QueryControl::Cancel(CancelToken::new());
        let checkpoint_memory =
            StorageMemory::new(&checkpoint_writer, &checkpoint_control, 32 * 1024 * 1024).unwrap();
        let checkpoint_source =
            NativePreparationSource::new(&lease, &checkpoint_memory, 128).unwrap();
        let mut checkpoint_resources = checkpoint_source.resources(u64::MAX).unwrap();
        let checkpoint_catalog =
            NativePreparationCatalog::open(&checkpoint_source, &mut checkpoint_resources).unwrap();
        let mut checkpoint_artifact = 61_650_u128;
        let mut checkpoint_serial = lease.bundle().high_waters().creation_serial + 1;
        let mut checkpoint_objects = PreparedObjects::new(
            &checkpoint_source,
            || {
                let identity = ArtifactIdentity {
                    store: identity,
                    artifact: ArtifactId::new(checkpoint_artifact)?,
                    generation: lease.bundle().base().generation,
                    creation_serial: checkpoint_serial,
                };
                checkpoint_artifact += 1;
                checkpoint_serial += 1;
                Ok(identity)
            },
            identity,
            lease.bundle().base().generation,
            PackLimits::default(),
            &checkpoint_memory,
            &mut checkpoint_resources,
        )
        .unwrap();
        let prepared_checkpoint = prepare_sparse_checkpoint(
            &mut checkpoint_objects,
            SparseRoots {
                text: lease.bundle().text(),
                vector: lease.bundle().vector(),
            },
            lease.bundle().roots(),
            lease.bundle().catalog(),
            &checkpoint_catalog,
            lease.bundle().document(),
            lease.bundle().lexical(),
            lease.bundle().sequence(),
            &checkpoint_memory,
            &mut checkpoint_resources,
        )
        .unwrap();
        checkpoint_objects
            .finish(&mut checkpoint_resources)
            .unwrap();
        let mut inventory = Vec::new();
        for index in 0..checkpoint_objects.len() {
            let object = checkpoint_objects.artifact(index).unwrap();
            let bytes = object.bytes();
            let checksum = u64::from_le_bytes(
                *bytes
                    .get(bytes.len() - 8..)
                    .unwrap()
                    .first_chunk::<8>()
                    .unwrap(),
            );
            let object_identity = object.identity();
            inventory.push(crate::property_graph::wal::InventoryChange {
                object: crate::property_graph::wal::ArtifactDescriptor {
                    store: object_identity.store,
                    artifact: object_identity.artifact,
                    generation: object_identity.generation,
                    serial: object_identity.creation_serial,
                    bytes: u32::try_from(bytes.len()).unwrap(),
                    family: FormatFamily::NativeGraphObject.id(),
                    version: 1,
                    checksum,
                },
                state: crate::property_graph::wal::InventoryState::Prepared,
            });
            std::fs::write(
                artifact_path(directory.path(), object_identity.artifact),
                bytes,
            )
            .unwrap();
        }
        let checkpoint_roots = prepared_checkpoint.finalize(&inventory).unwrap();
        let admitted = lease.bundle();
        let mut high_waters = admitted.high_waters();
        high_waters.creation_serial = inventory
            .iter()
            .map(|change| change.object.serial)
            .max()
            .unwrap_or(high_waters.creation_serial)
            .max(high_waters.creation_serial);
        let checkpoint_input = NativeGraphBundleInput {
            base: admitted.base(),
            root_envelope: admitted.root_envelope(),
            roots: admitted.roots(),
            wal_roots: admitted.wal_roots(),
            sequence: admitted.sequence(),
            catalog: admitted.catalog(),
            vector: checkpoint_roots.vector,
            text: checkpoint_roots.text,
            reclaim: admitted.reclaim(),
            high_waters,
            prepared_inventories: admitted.prepared_inventories().to_vec(),
            lexical: admitted.lexical(),
            document: admitted.document().cloned(),
        };
        drop(checkpoint_catalog);
        drop(checkpoint_objects);
        drop(checkpoint_resources);
        drop(checkpoint_source);
        drop(lease);
        store
            .install_native_graph_for_test(checkpoint_input)
            .unwrap();
    }

    let final_embedding = CanonicalEmbedding::new(&document, &replacement_vector).unwrap();
    let final_a = CanonicalContents::node(
        &mut [],
        &mut [],
        Some("final bronze"),
        Some(final_embedding),
    )
    .unwrap();
    let delete = [
        StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", "b").unwrap(),
            revision: GraphRevision::new(3).unwrap(),
            operation: StructuredOperation::Delete(EntityId::Node(node_b), GraphDeleteMode::Detach),
            image: None,
        },
        StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", "unrelated-extent").unwrap(),
            revision: GraphRevision::new(2).unwrap(),
            operation: StructuredOperation::Delete(EntityId::Node(node_c), GraphDeleteMode::Detach),
            image: None,
        },
        StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", "a").unwrap(),
            revision: GraphRevision::new(3).unwrap(),
            operation: StructuredOperation::Put(EntityId::Node(node_a)),
            image: Some(WriteImage::Node(&final_a)),
        },
        StructuredWrite {
            key: ApplicationKey::new(EntityKind::Relationship, "app", "unrelated-rel").unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Relationship {
                source: NodeRef::Existing(node_a),
                target: NodeRef::Existing(node_a),
                relationship_type: GraphName::new("R").unwrap(),
                properties: &[],
            }),
        },
    ];
    let writer3 = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let staged3 = stage_structured(&fixture2, &delete, &writer3, &mut |_| Ok(())).unwrap();
    let lease2 = store.admit_native_read().unwrap();
    let control3 = QueryControl::Cancel(CancelToken::new());
    let memory3 = StorageMemory::new(&writer3, &control3, 32 * 1024 * 1024).unwrap();
    let source2 = NativePreparationSource::new(&lease2, &memory3, 128).unwrap();
    let mut resources3 = source2.resources(u64::MAX).unwrap();
    if check_trace {
        use crate::property_graph::storage::search::SparseRoots;
        let trace_catalog = NativePreparationCatalog::open(&source2, &mut resources3).unwrap();
        let roots = SparseRoots {
            text: lease2.bundle().text(),
            vector: lease2.bundle().vector(),
        };
        let expected = expected_installed_trace_closure(&source2, roots, identity, &mut resources3);
        assert!(
            expected
                .iter()
                .any(|reference| reference.kind == BlockKind::ExtentList),
            "trace closure omitted the unrelated node extent list"
        );
        assert!(
            expected
                .iter()
                .filter(|reference| reference.kind == BlockKind::NodeRecord)
                .count()
                >= 5,
            "trace closure omitted retained dead-row record references"
        );
        for capacity in [1, 2, 256] {
            assert_eq!(
                collect_installed_trace(&source2, &trace_catalog, &mut resources3, capacity,),
                expected,
                "trace closure differs at capacity {capacity}",
            );
        }
        let required_child = expected
            .iter()
            .copied()
            .find(|reference| reference.kind == BlockKind::OperationProvenance)
            .unwrap();
        let missing_source = MissingReferenceSource {
            base: &source2,
            missing: required_child,
        };
        let mut missing = crate::property_graph::storage::search::SearchTraceCursor::for_test(
            &missing_source,
            &trace_catalog,
            roots,
            lease2.bundle().roots(),
            lease2.bundle().catalog(),
            lease2.bundle().document(),
            lease2.bundle().lexical(),
            &lease2,
            &memory3,
            &mut resources3,
        )
        .unwrap();
        let mut emitted = 0_usize;
        loop {
            let mut output = [None];
            match missing.trace(&mut output, &mut resources3) {
                Ok(result) => {
                    assert!(!result.complete, "missing child trace completed");
                    emitted += result.count;
                }
                Err(TreeError::Missing) => break,
                Err(other) => panic!("missing child returned {other}"),
            }
        }
        assert!(emitted > 0);
        assert!(matches!(
            missing.trace(&mut [None], &mut resources3),
            Err(TreeError::Invalid("sparse trace cursor previously failed"))
        ));

        let installed_bytes =
            std::fs::read(artifact_path(directory.path(), required_child.artifact)).unwrap();
        let corrupted = rewrite_selected_block(&installed_bytes, |reference, payload| {
            if reference != required_child {
                return false;
            }
            let byte = payload.first_mut().unwrap();
            *byte ^= 0x80;
            true
        })
        .unwrap();
        let corrupt_roots =
            roots_for_rewritten_artifact(roots, required_child.artifact, &corrupted);
        let corrupt_source = OverrideArtifactsSource {
            base: &source2,
            artifacts: vec![(required_child.artifact, corrupted)],
        };
        let mut corrupt = crate::property_graph::storage::search::SearchTraceCursor::for_test(
            &corrupt_source,
            &trace_catalog,
            corrupt_roots,
            lease2.bundle().roots(),
            lease2.bundle().catalog(),
            lease2.bundle().document(),
            lease2.bundle().lexical(),
            &lease2,
            &memory3,
            &mut resources3,
        )
        .unwrap();
        let mut emitted = 0_usize;
        loop {
            let mut output = [None];
            match corrupt.trace(&mut output, &mut resources3) {
                Ok(result) => {
                    assert!(!result.complete, "corrupt child trace completed");
                    emitted += result.count;
                }
                Err(TreeError::Invalid(_)) | Err(TreeError::Format(_)) => break,
                Err(other) => panic!("corrupt child returned {other}"),
            }
        }
        assert!(emitted > 0);
        drop(trace_catalog);
    }
    let mut artifact3 = 61_600_u128;
    let mut serial3 = lease2.bundle().high_waters().creation_serial + 1;
    let prepared3 = GraphPreparation::new(
        &source2,
        GraphGeneration::new(3),
        || {
            let output = ArtifactIdentity {
                store: identity,
                artifact: ArtifactId::new(artifact3)?,
                generation: GraphGeneration::new(3),
                creation_serial: serial3,
            };
            artifact3 += 1;
            serial3 += 1;
            Ok(output)
        },
        PackLimits::default(),
        &store.tokenizer,
        &mut resources3,
    )
    .unwrap()
    .prepare(&staged3, &mut resources3)
    .unwrap_or_else(|failure| panic!("generation three failed: {}", failure.error()));
    assert_eq!(
        prepared3.membership_changes()[0].membership,
        Some(crate::property_graph::wal::Membership {
            text_before: true,
            text_after: false,
            vector_before: true,
            vector_after: false,
        })
    );
    assert_eq!(
        prepared3.membership_changes()[1].membership,
        Some(crate::property_graph::wal::Membership {
            text_before: true,
            text_after: false,
            vector_before: false,
            vector_after: false,
        })
    );
    assert_eq!(
        prepared3.membership_changes()[2].membership,
        Some(crate::property_graph::wal::Membership {
            text_before: false,
            text_after: true,
            vector_before: true,
            vector_after: true,
        })
    );
    assert_eq!(prepared3.membership_changes()[3].membership, None);
    let catalog_view2 = NativePreparationCatalog::open(&source2, &mut resources3).unwrap();
    let final_view = SparseView::open(
        prepared3.objects(),
        prepared3.sparse_roots(),
        prepared3.candidate().roots(),
        lease2.bundle().catalog(),
        &catalog_view2,
        lease2.bundle().document(),
        lease2.bundle().lexical(),
        &memory3,
        &mut resources3,
    )
    .unwrap();
    assert_eq!((final_view.text_count(), final_view.vector_count()), (1, 1));
    assert!(
        final_view
            .lookup(Modality::Text, node_a, &mut resources3)
            .unwrap()
            .is_some()
    );
    assert!(
        final_view
            .lookup(Modality::Vector, node_a, &mut resources3)
            .unwrap()
            .is_some()
    );
    assert!(
        final_view
            .lookup(Modality::Text, node_b, &mut resources3)
            .unwrap()
            .is_none()
    );
    assert!(
        final_view
            .lookup(Modality::Vector, node_b, &mut resources3)
            .unwrap()
            .is_none()
    );

    if check_checkpoint {
        use crate::property_graph::storage::search::{
            SparseCheckpoint, validate_checkpoint, validate_replay_transition,
        };
        let checkpoint = SparseCheckpoint {
            cutoff: lease2.bundle().sequence(),
            roots: SparseRoots {
                text: lease2.bundle().text(),
                vector: lease2.bundle().vector(),
            },
        };
        assert_eq!(checkpoint.cutoff, 2);
        assert_eq!(final_view.checkpoint(), 2);
        assert_eq!(
            validate_checkpoint(
                prepared3.objects(),
                checkpoint,
                lease2.bundle().roots(),
                lease2.bundle().catalog(),
                &catalog_view2,
                lease2.bundle().document(),
                lease2.bundle().lexical(),
                &memory3,
                &mut resources3,
            )
            .unwrap(),
            (2, 2)
        );
        assert_eq!(
            validate_replay_transition(
                prepared3.objects(),
                checkpoint,
                lease2.bundle().roots(),
                prepared3.sparse_roots(),
                prepared3.candidate().roots(),
                lease2.bundle().catalog(),
                &catalog_view2,
                lease2.bundle().document(),
                lease2.bundle().lexical(),
                &staged3,
                prepared3.membership_changes(),
                &memory3,
                &mut resources3,
            )
            .unwrap(),
            (1, 1)
        );
        let mut invented = prepared3.membership_changes().to_vec();
        invented[0].node = Some(node_a);
        assert!(matches!(
            validate_replay_transition(
                prepared3.objects(),
                checkpoint,
                lease2.bundle().roots(),
                prepared3.sparse_roots(),
                prepared3.candidate().roots(),
                lease2.bundle().catalog(),
                &catalog_view2,
                lease2.bundle().document(),
                lease2.bundle().lexical(),
                &staged3,
                &invented,
                &memory3,
                &mut resources3,
            ),
            Err(TreeError::Invalid("sparse replay node identity"))
        ));
    }

    if check_corruption {
        let old_roots = SparseRoots {
            text: lease2.bundle().text(),
            vector: lease2.bundle().vector(),
        };
        let old_text = old_roots.text.unwrap();
        let old_text_block = crate::property_graph::storage::tree::directory::BlockSource::resolve(
            &source2,
            old_text.block,
            &mut resources3,
        )
        .unwrap();
        let old_members =
            artifact::decode_reference(old_text_block.payload().get(168..200).unwrap()).unwrap();
        let old_member_block =
            crate::property_graph::storage::tree::directory::BlockSource::resolve(
                &source2,
                old_members,
                &mut resources3,
            )
            .unwrap();
        let old_member_page = crate::property_graph::storage::tree::decode_page(
            crate::property_graph::storage::tree::TreeKind::SparseMembership,
            old_member_block.payload(),
        )
        .unwrap();
        let crate::property_graph::storage::tree::Cell::Leaf { value, .. } =
            old_member_page.cell(0).unwrap()
        else {
            panic!("installed sparse membership root is not a leaf");
        };
        let old_source = artifact::decode_reference(value.get(8..40).unwrap()).unwrap();
        let old_source_block =
            crate::property_graph::storage::tree::directory::BlockSource::resolve(
                &source2,
                old_source,
                &mut resources3,
            )
            .unwrap();
        let old_rows = crate::property_graph::storage::payload::PayloadRef::decode(
            old_source_block.payload().get(40..88).unwrap(),
        )
        .unwrap()
        .reference();
        let native_nodes = prepared3
            .candidate()
            .roots()
            .directory(crate::property_graph::storage::tree::TreeKind::Nodes)
            .unwrap();
        let tombstone_entry = crate::property_graph::storage::tree::directory::lookup_entry(
            prepared3.objects(),
            native_nodes,
            &node_b.get().to_le_bytes(),
            &mut resources3,
        )
        .unwrap()
        .unwrap();
        let tombstone =
            crate::property_graph::storage::payload::PayloadRef::decode(tombstone_entry.value())
                .unwrap();

        let mut artifacts = Vec::new();
        rewrite_installed_block(
            directory.path(),
            &mut artifacts,
            old_text.block,
            |payload| {
                payload
                    .get_mut(32..40)
                    .unwrap()
                    .copy_from_slice(&3_u64.to_le_bytes());
            },
        );
        let old_vector = old_roots.vector.unwrap();
        rewrite_installed_block(
            directory.path(),
            &mut artifacts,
            old_vector.block,
            |payload| {
                payload
                    .get_mut(32..40)
                    .unwrap()
                    .copy_from_slice(&3_u64.to_le_bytes());
            },
        );
        rewrite_installed_membership(directory.path(), &mut artifacts, old_members, |_, value| {
            value
                .get_mut(0..8)
                .unwrap()
                .copy_from_slice(&3_u64.to_le_bytes());
        });
        rewrite_installed_block(directory.path(), &mut artifacts, old_rows, |payload| {
            payload
                .get_mut(16..24)
                .unwrap()
                .copy_from_slice(&3_u64.to_le_bytes());
            tombstone
                .encode_into(payload.get_mut(24..72).unwrap())
                .unwrap();
        });
        let mut tombstone_roots = old_roots;
        for (artifact_id, bytes) in &artifacts {
            tombstone_roots = roots_for_rewritten_artifact(tombstone_roots, *artifact_id, bytes);
        }
        let tombstone_source = OverrideArtifactsSource {
            base: prepared3.objects(),
            artifacts,
        };
        let tombstone_view = SparseView::open(
            &tombstone_source,
            tombstone_roots,
            prepared3.candidate().roots(),
            lease2.bundle().catalog(),
            &catalog_view2,
            lease2.bundle().document(),
            lease2.bundle().lexical(),
            &memory3,
            &mut resources3,
        )
        .unwrap();
        assert!(matches!(
            tombstone_view.lookup(Modality::Text, node_b, &mut resources3),
            Err(TreeError::Invalid("sparse row names a tombstone"))
        ));
    }

    if !check_corruption
        && !check_checkpoint
        && !check_required_bytes
        && !check_trace
        && !check_resources
    {
        let target_identity3 = BaseIdentity {
            store: identity,
            generation: GraphGeneration::new(3),
            fold: Default::default(),
            roots: Some(ArtifactId::new(61_699).unwrap()),
        };
        let fixture3 = fixture2.after(&staged3, target_identity3);
        let catalog3 = write_complete_sparse_catalog(
            directory.path(),
            ArtifactIdentity {
                store: identity,
                artifact: ArtifactId::new(61_698).unwrap(),
                generation: GraphGeneration::new(3),
                creation_serial: 30_000,
            },
            fixture3.high,
            &fixture3.symbols,
            Some(&document),
        );
        drop(final_view);
        drop(catalog_view2);
        let input3 = materialize_sparse_generation(
            prepared3,
            lease2.bundle(),
            directory.path(),
            61_699,
            catalog3,
            fixture3.high.node,
            fixture3.high.relationship,
            fixture3.high.symbols,
            lease2.bundle().lexical(),
            Some(document.clone()),
        );
        drop(resources3);
        drop(source2);
        drop(lease2);
        store.install_native_graph_for_test(input3).unwrap();

        let analyzed_empty_a =
            CanonicalContents::node(&mut [], &mut [], Some("the and"), None).unwrap();
        let remove_a = [StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", "a").unwrap(),
            revision: GraphRevision::new(4).unwrap(),
            operation: StructuredOperation::Put(EntityId::Node(node_a)),
            image: Some(WriteImage::Node(&analyzed_empty_a)),
        }];
        let writer4 = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
        let staged4 = stage_structured(&fixture3, &remove_a, &writer4, &mut |_| Ok(())).unwrap();
        let lease3 = store.admit_native_read().unwrap();
        let control4 = QueryControl::Cancel(CancelToken::new());
        let memory4 = StorageMemory::new(&writer4, &control4, 32 * 1024 * 1024).unwrap();
        let source3 = NativePreparationSource::new(&lease3, &memory4, 128).unwrap();
        let mut resources4 = source3.resources(u64::MAX).unwrap();
        let catalog_view3 = NativePreparationCatalog::open(&source3, &mut resources4).unwrap();
        let retained_view = SparseView::open(
            &source3,
            SparseRoots {
                text: lease3.bundle().text(),
                vector: lease3.bundle().vector(),
            },
            lease3.bundle().roots(),
            lease3.bundle().catalog(),
            &catalog_view3,
            lease3.bundle().document(),
            lease3.bundle().lexical(),
            &memory4,
            &mut resources4,
        )
        .unwrap();
        let mut artifact4 = 61_700_u128;
        let mut serial4 = lease3.bundle().high_waters().creation_serial + 1;
        let prepared4 = GraphPreparation::new(
            &source3,
            GraphGeneration::new(4),
            || {
                let output = ArtifactIdentity {
                    store: identity,
                    artifact: ArtifactId::new(artifact4)?,
                    generation: GraphGeneration::new(4),
                    creation_serial: serial4,
                };
                artifact4 += 1;
                serial4 += 1;
                Ok(output)
            },
            PackLimits::default(),
            &store.tokenizer,
            &mut resources4,
        )
        .unwrap()
        .prepare(&staged4, &mut resources4)
        .unwrap_or_else(|failure| panic!("generation four failed: {}", failure.error()));
        assert_eq!(prepared4.membership_changes().len(), 1);
        assert_eq!(
            prepared4.membership_changes()[0].membership,
            Some(crate::property_graph::wal::Membership {
                text_before: true,
                text_after: false,
                vector_before: true,
                vector_after: false,
            })
        );
        let removed_view = SparseView::open(
            prepared4.objects(),
            prepared4.sparse_roots(),
            prepared4.candidate().roots(),
            lease3.bundle().catalog(),
            &catalog_view3,
            lease3.bundle().document(),
            lease3.bundle().lexical(),
            &memory4,
            &mut resources4,
        )
        .unwrap();
        assert_eq!(
            (retained_view.text_count(), retained_view.vector_count()),
            (1, 1)
        );
        assert!(
            retained_view
                .lookup(Modality::Text, node_a, &mut resources4)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            (removed_view.text_count(), removed_view.vector_count()),
            (0, 0)
        );
        assert!(
            removed_view
                .lookup(Modality::Text, node_a, &mut resources4)
                .unwrap()
                .is_none()
        );
        assert!(
            removed_view
                .lookup(Modality::Vector, node_a, &mut resources4)
                .unwrap()
                .is_none()
        );
        drop(removed_view);
        drop(retained_view);
        drop(catalog_view3);
        drop(prepared4);
        drop(resources4);
        drop(source3);
        drop(lease3);
        store.close().unwrap();
        return;
    }

    drop(final_view);
    drop(catalog_view2);
    drop(prepared3);
    drop(resources3);
    drop(source2);
    drop(lease2);

    if check_resources {
        struct CloseSparseQuery {
            node: NodeId,
            store: std::sync::Arc<Store>,
            handle: Option<
                std::sync::mpsc::Sender<
                    std::thread::JoinHandle<Result<(), crate::lifecycle::StoreError>>,
                >,
            >,
        }
        impl NativeReadConsumer<()> for CloseSparseQuery {
            fn consume<'s, 'lease, 'm, 'g>(
                &mut self,
                view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
                runtime: &mut crate::property_graph::query::runtime::RuntimeContext<'lease, 'm, 'g>,
            ) -> Result<(), TreeError> {
                let sparse = view.sparse_view(runtime)?;
                let mut resources = TreeResources::for_query(runtime)?;
                if sparse
                    .lookup(Modality::Vector, self.node, &mut resources)?
                    .is_none()
                {
                    return Err(TreeError::Invalid("missing sparse close probe member"));
                }
                let closing = std::sync::Arc::clone(&self.store);
                let handle = std::thread::spawn(move || closing.close());
                self.handle
                    .take()
                    .ok_or(TreeError::Invalid("missing sparse close handle owner"))?
                    .send(handle)
                    .map_err(|_| TreeError::Invalid("sparse close handle receiver dropped"))?;
                let publication = std::sync::Arc::clone(&self.store.native_graph);
                let mut state = publication
                    .state
                    .lock()
                    .map_err(|_| TreeError::Invalid("sparse close publication poisoned"))?;
                while !state.closing {
                    state = publication
                        .changed
                        .wait(state)
                        .map_err(|_| TreeError::Invalid("sparse close wait poisoned"))?;
                }
                drop(state);
                let _ = sparse.lookup(Modality::Text, self.node, &mut resources)?;
                Err(TreeError::Invalid(
                    "sparse close did not cancel active read",
                ))
            }
        }

        let (handle_tx, handle_rx) = std::sync::mpsc::channel();
        let closed = store.with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            crate::property_graph::query::runtime::RuntimeLimits::default(),
            8 * 1024 * 1024,
            128,
            CloseSparseQuery {
                node: node_b,
                store: std::sync::Arc::clone(&store),
                handle: Some(handle_tx),
            },
        );
        assert!(matches!(
            closed,
            Err(NativeGraphError::Read(TreeError::Runtime(
                crate::property_graph::query::runtime::RuntimeError::Value(
                    crate::property_graph::query::QueryError::ReadCancelled
                )
            )))
        ));
        handle_rx.recv().unwrap().join().unwrap().unwrap();
    } else {
        store.close().unwrap();
    }
}

#[test]
fn ze61_replace_delete_are_atomic_private_participants() {
    run_sparse_lifecycle_acceptance(false, false, false, false, false);
}

#[test]
fn ze61_full_identity_revision_and_row_correlations_are_checked() {
    run_sparse_lifecycle_acceptance(true, false, false, false, false);
}

#[test]
fn ze61_checkpoint_and_replay_match_active_model() {
    run_sparse_lifecycle_acceptance(false, true, false, false, false);
}

#[test]
fn ze61_required_sparse_bytes_fail_loudly() {
    run_sparse_lifecycle_acceptance(false, false, true, false, false);
}

#[test]
fn ze61_search_trace_is_complete_and_bounded() {
    run_sparse_lifecycle_acceptance(false, false, false, true, false);
}

#[test]
fn ze61_sparse_resources_use_the_admitted_owner() {
    run_sparse_lifecycle_acceptance(false, false, false, false, true);
}
