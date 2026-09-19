#![no_main]
//! Framed raw/mutated inner bodies reach native semantic decoders. This bounded
//! tooling source is not production cache/admission or allocation evidence.
use libfuzzer_sys::fuzz_target;
use std::sync::OnceLock;
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::property_graph::storage::{
    artifact::*,
    inventory::verify_inventory_entry,
    memory::StorageMemory,
    payload::{PayloadRef, prepare_payload},
    records::*,
    stream::PayloadSlice,
    tree::{TreeKind, directory::*},
};
use zeppelin_embed::property_graph::{catalog::*, resources::GraphResources, staging::*, *};
fn runtime() -> &'static (Store, GraphResources) {
    static RUNTIME: OnceLock<(Store, GraphResources)> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        let path = std::env::temp_dir().join(format!("ze43-record-fuzz-{}", std::process::id()));
        let store = Store::open(
            &path,
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .unwrap();
        let resources = GraphResources::from_store(&store).unwrap();
        (store, resources)
    })
}
struct Blocks(Vec<(PhysicalRef, Vec<u8>)>);
impl BlockSource for Blocks {
    fn resolve<'a>(
        &'a self,
        reference: PhysicalRef,
        r: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        r.step(1)?;
        let (_, bytes) = self
            .0
            .iter()
            .find(|(stored, _)| *stored == reference)
            .ok_or(TreeError::Missing)?;
        let frame = decode(
            ContainerKind::Object,
            Some((store(), reference.artifact)),
            bytes,
        )?;
        Ok(frame.framed_block(reference)?)
    }
}
impl BlockSink for Blocks {
    fn append(
        &mut self,
        kind: BlockKind,
        generation: GraphGeneration,
        bytes: &[u8],
        r: &mut TreeResources<'_>,
    ) -> Result<PhysicalRef, TreeError> {
        r.step(bytes.len() as u64)?;
        let identity = ArtifactIdentity {
            store: store(),
            artifact: ArtifactId::new(self.0.len() as u128 + 1)?,
            generation,
            creation_serial: self.0.len() as u64 + 1,
        };
        let blocks = [Block {
            kind,
            payload: bytes,
        }];
        let mut encoded = vec![0; encoded_len(ContainerKind::Object, &blocks)?];
        encode_into(ContainerKind::Object, identity, &blocks, &mut encoded)?;
        let reference = decode(
            ContainerKind::Object,
            Some((store(), identity.artifact)),
            &encoded,
        )?
        .reference(0)?;
        self.0.push((reference, encoded));
        Ok(reference)
    }
}
struct Visitor;
impl CanonicalVisitor<Blocks> for Visitor {
    fn label(
        &mut self,
        _: PayloadSlice<'_, Blocks>,
        r: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        r.step(0)
    }
    fn property(
        &mut self,
        _: PayloadSlice<'_, Blocks>,
        _: StoredProperty<'_, Blocks>,
        r: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        r.step(0)
    }
}
struct Catalog;
impl RecordCatalog<Blocks> for Catalog {
    fn resolve(
        &self,
        kind: SymbolKind,
        name: PayloadSlice<'_, Blocks>,
        r: &mut TreeResources<'_>,
    ) -> Result<Symbol, TreeError> {
        let expected = match kind {
            SymbolKind::Namespace => b"N".as_slice(),
            SymbolKind::Label => b"L",
            SymbolKind::Property => b"P",
            SymbolKind::RelationshipType => b"R",
        };
        if !name.compare_bytes(expected, r)?.is_eq() {
            return Err(TreeError::Invalid("fuzz unknown symbol"));
        }
        Symbol::new(kind, 1).map_err(|_| TreeError::Invalid("fuzz symbol"))
    }
}
fn store() -> StoreInstanceId {
    StoreInstanceId::new(1).unwrap()
}
fn generation() -> GraphGeneration {
    GraphGeneration::new(2)
}
fn body(source: &Blocks, reference: PayloadRef, r: &mut TreeResources<'_>) -> Vec<u8> {
    let slice = PayloadSlice::new(source, store(), generation(), reference);
    let mut bytes = vec![0; slice.len() as usize];
    slice.read_at(0, &mut bytes, r).unwrap();
    bytes
}
fn mutated(mut base: Vec<u8>, data: &[u8]) -> Vec<u8> {
    if data[0] & 128 != 0 {
        return data[1..].to_vec();
    }
    for mutation in data[1..].chunks_exact(3) {
        if !base.is_empty() {
            let offset = u16::from_le_bytes([mutation[0], mutation[1]]) as usize % base.len();
            base[offset] ^= mutation[2];
        }
    }
    base
}
fn exercise(data: &[u8]) {
    if data.is_empty() || data.len() > 65_536 {
        return;
    }
    let lane = data[0] & 7;
    let resources = &runtime().1;
    let control = QueryControl::Cancel(CancelToken::new());
    let writer = WriteMemory::new(resources, WriteLimits::default()).unwrap();
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let mut r = TreeResources::for_prepare(&memory, 50_000_000).unwrap();
    let mut objects = Blocks(Vec::new());
    let relationship = lane == 4;
    let entity = if relationship {
        EntityId::Relationship(RelId::new(1u128 << 100).unwrap())
    } else {
        EntityId::Node(NodeId::new(1u128 << 100).unwrap())
    };
    let key = ApplicationKey::new(
        if relationship {
            EntityKind::Relationship
        } else {
            EntityKind::Node
        },
        "N",
        "key\0🦀",
    )
    .unwrap();
    let mut labels = [GraphName::new("L").unwrap()];
    let mut properties = [GraphProperty::new(
        GraphName::new("P").unwrap(),
        PropertyValue::new(PropertyData::I64(-17)).unwrap(),
    )];
    let image = if relationship {
        CanonicalContents::relationship(
            NodeId::new(1).unwrap(),
            NodeId::new(2).unwrap(),
            GraphName::new("R").unwrap(),
            &mut properties,
        )
        .unwrap()
    } else {
        CanonicalContents::node(&mut labels, &mut properties, Some("text\0🦀"), None).unwrap()
    };
    let mut bytes = Vec::new();
    image.write_to(&mut bytes, &mut || Ok(())).unwrap();
    let canonical = prepare_payload(
        &mut objects,
        store(),
        generation(),
        BlockKind::CanonicalImage,
        &bytes,
        &mut r,
    )
    .unwrap();
    let deletion = lane == 3;
    let fields = OperationFields {
        operation: if deletion {
            GraphOperation::StructuredDelete
        } else {
            GraphOperation::StructuredCreate
        },
        key: Some(key),
        requested_revision: GraphRevision::new(1).unwrap(),
        installed_revision: GraphRevision::new(1).unwrap(),
        expected: if deletion {
            ExpectedGraphState::Entity(entity)
        } else {
            ExpectedGraphState::Absent
        },
        incarnation: entity,
        delete_mode: deletion.then_some(GraphDeleteMode::Detach),
        original_generation: generation(),
    };
    bytes.clear();
    OperationProvenance::from_fields(Some(1), fields)
        .unwrap()
        .write_to(&mut bytes, &mut || Ok(()))
        .unwrap();
    let provenance = prepare_payload(
        &mut objects,
        store(),
        generation(),
        BlockKind::OperationProvenance,
        &bytes,
        &mut r,
    )
    .unwrap();
    let accepted = match lane {
        0 | 1 => {
            let (role, reference) = if lane == 0 {
                (BlockKind::CanonicalImage, canonical)
            } else {
                (BlockKind::OperationProvenance, provenance)
            };
            let changed = mutated(body(&objects, reference, &mut r), data);
            let reference =
                prepare_payload(&mut objects, store(), generation(), role, &changed, &mut r)
                    .unwrap();
            let source = PayloadSlice::new(&objects, store(), generation(), reference);
            if lane == 0 {
                verify_canonical(source, None, &mut Visitor, &mut r).is_ok()
            } else {
                verify_provenance(source, &mut r).is_ok()
            }
        }
        2..=4 => {
            let reference = if deletion {
                prepare_node_tombstone(
                    &mut objects,
                    store(),
                    generation(),
                    NodeId::new(1u128 << 100).unwrap(),
                    provenance,
                    &mut r,
                )
                .unwrap()
            } else {
                prepare_record(
                    &mut objects,
                    RecordInput {
                        store: store(),
                        generation: generation(),
                        entity,
                        canonical,
                        provenance,
                    },
                    &Catalog,
                    None,
                    &memory,
                    &mut r,
                )
                .unwrap()
            };
            let changed = mutated(body(&objects, reference, &mut r), data);
            let reference = prepare_payload(
                &mut objects,
                store(),
                generation(),
                if relationship {
                    BlockKind::RelRecord
                } else {
                    BlockKind::NodeRecord
                },
                &changed,
                &mut r,
            )
            .unwrap();
            let source = PayloadSlice::new(&objects, store(), generation(), reference);
            if relationship {
                verify_record(source, entity, &Catalog, None, &mut r).is_ok()
            } else {
                verify_node_state(
                    source,
                    NodeId::new(1u128 << 100).unwrap(),
                    &Catalog,
                    None,
                    &mut r,
                )
                .is_ok()
            }
        }
        5 => {
            let probe =
                FenceKey::new(EntityKind::Node, NamespaceId::new(1).unwrap(), "key\0🦀").unwrap();
            let value = prepare_fence(
                &objects,
                FenceInput {
                    store: store(),
                    generation: generation(),
                    key: probe,
                    provenance,
                    canonical: Some(canonical),
                },
                &Catalog,
                None,
                &mut r,
            )
            .unwrap();
            let changed = mutated(value.to_vec(), data);
            let mut scratch = TreeScratch::for_prepare(&memory).unwrap();
            let root = insert_fence(
                &mut objects,
                DirectoryRoot::empty(store(), TreeKind::KeyFences, generation()),
                probe,
                &changed,
                generation(),
                &mut scratch,
                &mut r,
            );
            match root {
                Ok(root) => {
                    let entry = lookup_fence_entry(&objects, root, probe, &mut r)
                        .unwrap()
                        .unwrap();
                    verify_fence_entry(&objects, root, entry, &Catalog, None, &mut r).is_ok()
                }
                Err(_) => false,
            }
        }
        6 => {
            let mut value = vec![0; 88];
            value[..16].copy_from_slice(&1u128.to_le_bytes());
            value[16..32].copy_from_slice(&1u128.to_le_bytes());
            value[32..40].copy_from_slice(&2u64.to_le_bytes());
            value[40..48].copy_from_slice(&1u64.to_le_bytes());
            value[48..52].copy_from_slice(&104u32.to_le_bytes());
            value[52..54].copy_from_slice(&17u16.to_le_bytes());
            value[54..56].copy_from_slice(&1u16.to_le_bytes());
            value[64] = 1;
            let changed = mutated(value, data);
            let mut scratch = TreeScratch::for_prepare(&memory).unwrap();
            let root = insert(
                &mut objects,
                DirectoryRoot::empty(store(), TreeKind::ObjectInventory, generation()),
                &1u128.to_le_bytes(),
                &changed,
                generation(),
                &mut scratch,
                &mut r,
            );
            match root {
                Ok(root) => {
                    let entry = lookup_entry(&objects, root, &1u128.to_le_bytes(), &mut r)
                        .unwrap()
                        .unwrap();
                    verify_inventory_entry(root, entry, &mut r).is_ok()
                }
                Err(_) => false,
            }
        }
        _ => {
            let _ = PayloadRef::decode(&data[1..]);
            return;
        }
    };
    if data.len() == 1 && data[0] < 8 {
        assert!(
            accepted,
            "valid seeded lane{lane} must reach complete semantic acceptance"
        );
    }
}
fuzz_target!(|data: &[u8]| exercise(data));
