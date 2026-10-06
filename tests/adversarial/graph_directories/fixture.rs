//! Independently authored logical trace plus the retained staging-base adapter.
//! Expected canonical bytes below are literal v1 fields, never encoder output.
use super::*;
use zeppelin_embed::fts::tokenizer::{TokenizerConfig, TokenizerEpoch};

pub const NS: &str = "pg8\0";
pub const NODE_HIGH: u128 = 1u128 << 100;
pub const REL_HIGH: u128 = 1u128 << 110;
#[derive(Clone)]
pub struct Request {
    pub kind: model::OperationKind,
    pub name: &'static str,
    pub id: u128,
    pub revision: u64,
    pub labels: &'static [&'static str],
    pub text: Option<String>,
    pub bits: u64,
    pub relationship: bool,
}
impl Request {
    pub fn entity(&self) -> EntityId {
        if self.relationship {
            EntityId::Relationship(RelId::new(self.id).unwrap())
        } else {
            EntityId::Node(NodeId::new(self.id).unwrap())
        }
    }
    pub fn key(&self) -> ApplicationKey<'static> {
        ApplicationKey::new(
            if self.relationship {
                EntityKind::Relationship
            } else {
                EntityKind::Node
            },
            NS,
            self.name,
        )
        .unwrap()
    }
    pub fn operation(&self) -> StructuredOperation {
        match self.kind {
            model::OperationKind::Create => StructuredOperation::Create,
            model::OperationKind::Put => StructuredOperation::Put(self.entity()),
            model::OperationKind::Delete => {
                StructuredOperation::Delete(self.entity(), GraphDeleteMode::Detach)
            }
            model::OperationKind::Recreate => {
                StructuredOperation::Recreate(GraphRevision::new(3).unwrap())
            }
            model::OperationKind::Cypher => unreachable!("structured fixture"),
        }
    }
    pub fn primitive(&self) -> model::Operation {
        use model::{
            DeleteMode, Entity, Expected, Image, Key, Kind, Operation, OperationKind, Shape,
        };
        let kind = if self.relationship {
            Kind::Relationship
        } else {
            Kind::Node
        };
        let entity = Entity { kind, id: self.id };
        Operation {
            kind: self.kind,
            key: Some(Key {
                kind,
                namespace: NS.as_bytes().to_vec(),
                key: self.name.as_bytes().to_vec(),
            }),
            expected: match self.kind {
                OperationKind::Create => Expected::Absent,
                OperationKind::Recreate => Expected::Deletion(3),
                _ => Expected::Entity(entity),
            },
            incarnation: entity,
            revision: self.revision,
            delete_mode: (self.kind == OperationKind::Delete).then_some(DeleteMode::Detach),
            image: (self.kind != OperationKind::Delete).then(|| Image {
                canonical: self.canonical(),
                shape: if self.relationship {
                    Shape::Relationship {
                        source: NODE_HIGH + 1,
                        target: NODE_HIGH + 2,
                        rel_type: 1,
                    }
                } else {
                    Shape::Node {
                        labels: self
                            .labels
                            .iter()
                            .map(|name| match *name {
                                "L1" => 1,
                                "L2" => 2,
                                "L3" => 3,
                                _ => unreachable!(),
                            })
                            .collect(),
                    }
                },
            }),
        }
    }
    fn canonical(&self) -> Vec<u8> {
        fn blob(out: &mut Vec<u8>, bytes: &[u8]) {
            out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        let mut out = b"ZGCI\x01\0".to_vec();
        if self.relationship {
            out.push(2);
            out.extend_from_slice(&(NODE_HIGH + 1).to_le_bytes());
            out.extend_from_slice(&(NODE_HIGH + 2).to_le_bytes());
            blob(&mut out, b"R");
        } else {
            out.push(1);
            out.extend_from_slice(&(self.labels.len() as u64).to_le_bytes());
            for label in self.labels {
                blob(&mut out, label.as_bytes());
            }
        }
        out.extend_from_slice(&1u64.to_le_bytes());
        blob(&mut out, b"bits");
        out.push(4);
        out.extend_from_slice(&self.bits.to_le_bytes());
        out.push(u8::from(self.text.is_some()));
        if let Some(text) = &self.text {
            blob(&mut out, text.as_bytes());
        }
        out.push(0);
        out
    }
}
pub fn trace(seed: u64) -> Vec<Request> {
    use model::OperationKind::*;
    let mut rng = super::super::test_support::seeded_rng("property_graph::directories", seed);
    let bits = 0x7ff8_0000_0000_0000 | (rng.next_u64() & 0x0007_ffff_ffff_ffff);
    let a = Request {
        kind: Create,
        name: "🦀\0a",
        id: NODE_HIGH + 1,
        revision: 1,
        labels: &["L1", "L2"],
        text: Some("old\0text".into()),
        bits,
        relationship: false,
    };
    let b = Request {
        name: "",
        id: NODE_HIGH + 2,
        labels: &["L2"],
        text: None,
        ..a.clone()
    };
    let rel = Request {
        name: "r",
        id: REL_HIGH + 1,
        labels: &[],
        text: None,
        relationship: true,
        ..a.clone()
    };
    let put = Request {
        kind: Put,
        revision: 2,
        labels: &["L2", "L3"],
        text: Some("x🦀\0".repeat(12_000)),
        ..a.clone()
    };
    vec![
        a.clone(),
        b.clone(),
        rel.clone(),
        put,
        Request {
            kind: Delete,
            revision: 3,
            ..a.clone()
        },
        Request {
            kind: Delete,
            revision: 2,
            ..rel
        },
        Request {
            kind: Delete,
            revision: 2,
            ..b
        },
        Request {
            kind: Recreate,
            revision: 4,
            id: NODE_HIGH + 3,
            labels: &["L1"],
            ..a
        },
    ]
}
#[derive(Clone)]
struct Entry {
    provenance: OperationProvenance<'static>,
    shape: Option<EntityShape<'static>>,
    canonical: Option<Vec<u8>>,
    fingerprint: CanonicalFingerprint,
    membership: Membership,
}
impl CanonicalSource for Entry {
    fn read_at(&self, offset: u64, output: &mut [u8]) -> std::io::Result<usize> {
        CanonicalSlice(
            self.canonical
                .as_deref()
                .ok_or(std::io::ErrorKind::InvalidData)?,
        )
        .read_at(offset, output)
    }
}
impl Entry {
    fn live(&self, view: BaseIdentity) -> Option<BaseEntity<'_>> {
        self.canonical.as_ref()?;
        Some(BaseEntity {
            view,
            provenance: self.provenance,
            shape: self.shape?,
            fingerprint: self.fingerprint,
            source: self,
            membership: self.membership,
        })
    }
}
#[derive(Clone)]
pub struct Base {
    identity: BaseIdentity,
    high: HighWaters,
    entries: Vec<Entry>,
    symbols: Vec<(Symbol, &'static str)>,
}
impl Base {
    pub fn empty() -> Self {
        Self {
            identity: BaseIdentity {
                store: StoreInstanceId::new(1).unwrap(),
                generation: GraphGeneration::new(0),
                fold: Default::default(),
                roots: None,
            },
            high: HighWaters {
                node: NODE_HIGH,
                relationship: REL_HIGH,
                ..Default::default()
            },
            entries: Vec::new(),
            symbols: Vec::new(),
        }
    }
    pub fn after(
        &self,
        batch: &StagedBatch<'_>,
        request: &Request,
        fingerprint: CanonicalFingerprint,
    ) -> Self {
        let mut next = self.clone();
        for delta in batch.deltas() {
            let fields = OperationFields {
                key: Some(request.key()),
                ..delta.provenance().fields()
            };
            let entry = Entry {
                provenance: OperationProvenance::from_fields(Some(1), fields).unwrap(),
                shape: delta.shape().map(|shape| match shape {
                    EntityShape::Node => EntityShape::Node,
                    EntityShape::Relationship { source, target, .. } => EntityShape::Relationship {
                        source,
                        target,
                        relationship_type: GraphName::new("R").unwrap(),
                    },
                }),
                canonical: delta.canonical().map(<[u8]>::to_vec),
                fingerprint,
                membership: delta.membership().1,
            };
            if let Some(old) = next
                .entries
                .iter_mut()
                .find(|old| old.provenance.fields().incarnation == fields.incarnation)
            {
                *old = entry;
            } else {
                next.entries.push(entry);
            }
        }
        for symbol in batch.symbols() {
            let name = [NS, "L1", "L2", "L3", "R", "bits"]
                .into_iter()
                .find(|name| *name == symbol.name.as_str())
                .unwrap();
            next.symbols.push((symbol.symbol, name));
        }
        next.high = batch.high_waters();
        if !batch.deltas().is_empty() {
            next.identity.generation = GraphGeneration::new(next.identity.generation.get() + 1);
            next.identity.roots =
                Some(ArtifactId::new(50_000 + next.identity.generation.get() as u128).unwrap());
        }
        next
    }
}
impl<S: BlockSource> RecordCatalog<S> for Base {
    fn resolve(
        &self,
        kind: SymbolKind,
        name: PayloadSlice<'_, S>,
        r: &mut TreeResources<'_>,
    ) -> Result<Symbol, TreeError> {
        for (symbol, text) in &self.symbols {
            r.step(1)?;
            if symbol.kind() == kind && name.compare_bytes(text.as_bytes(), r)?.is_eq() {
                return Ok(*symbol);
            }
        }
        Err(TreeError::Invalid("unknown PG8 fixture symbol"))
    }
}
impl<S: BlockSource> PreparationCatalog<S> for Base {
    fn namespace_id(
        &self,
        name: zeppelin_embed::property_graph::GraphName<'_>,
        r: &mut TreeResources<'_>,
    ) -> Result<zeppelin_embed::property_graph::catalog::NamespaceId, TreeError> {
        for entry in &self.symbols {
            r.step(1)?;
            if entry.1 == name.as_str()
                && let Symbol::Namespace(id) = entry.0
            {
                return Ok(id);
            }
        }
        Err(TreeError::Invalid("unknown fixture namespace"))
    }
    fn base_identity(&self) -> BaseIdentity {
        self.identity
    }
}
impl AdmittedBase for Base {
    fn identity(&self) -> BaseIdentity {
        self.identity
    }
    fn high_waters(&self) -> HighWaters {
        self.high
    }
    fn interpretation(&self) -> GraphInterpretation<'_> {
        GraphInterpretation::new(TokenizerEpoch::of(&TokenizerConfig::text_default()), None)
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
        panic!("PG8 DETACH must not probe incidents")
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
            .find(|(symbol, text)| symbol.kind() == kind && *text == name.as_str())
            .map(|(symbol, _)| *symbol))
    }
}
