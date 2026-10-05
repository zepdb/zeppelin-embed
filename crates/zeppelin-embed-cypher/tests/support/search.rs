//! Local Zeppelin search-extension fixture; not an original TCK fixture.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::result_large_err
)]
use zeppelin_embed::epoch::{ComputeUnits, EmbeddingRuntime, EmbeddingTower, Normalization};
use zeppelin_embed::lifecycle::{CancelToken, QueryControl};
use zeppelin_embed::property_graph::query::completed::{CompletedGraphResult, GraphQueryOptions};
use zeppelin_embed::property_graph::query::plan::ParameterBinding;
use zeppelin_embed::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use zeppelin_embed::property_graph::{
    ApplicationKey, CanonicalContents, CanonicalEmbedding, EntityKind, GraphName, GraphProperty,
    GraphRevision, GraphStore, PropertyData, PropertyValue,
};
use zeppelin_embed_cypher::{CompileLimits, StatementError, execute};
pub struct SearchFixture {
    pub root: std::path::PathBuf,
    pub store: Option<GraphStore>,
    pub tower: EmbeddingTower,
}
pub fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}
impl SearchFixture {
    pub fn create() -> Self {
        let root = crate::support::unique_temp_dir("ze58-search");
        let tower = EmbeddingTower {
            model_id: "ze58".into(),
            model_version: "1".into(),
            weights_digest: vec![58],
            dims: 2,
            normalization: Normalization::None,
            prompt_prefix: String::new(),
            max_tokens: 32,
            runtime: EmbeddingRuntime::CpuReference,
            compute_units: ComputeUnits::Cpu,
            os_build: None,
        };
        let store = GraphStore::create(
            &root,
            zeppelin_embed::lifecycle::OpenOptions::new()
                .with_max_resident_bytes(256 * 1024 * 1024),
            Some(tower.clone()),
        )
        .unwrap();
        for (key, text, vector) in [
            ("a", Some("amber birch"), Some([1.0, 1.0])),
            ("b", Some("amber"), Some([5.0, 5.0])),
            ("c", Some("birch"), None),
            ("d", None, Some([0.0, 0.0])),
        ] {
            let mut labels = [GraphName::new("Chunk").unwrap()];
            let mut properties = [GraphProperty::new(
                GraphName::new("key").unwrap(),
                PropertyValue::new(PropertyData::String(key)).unwrap(),
            )];
            let contents = CanonicalContents::node(
                &mut labels,
                &mut properties,
                text,
                vector
                    .as_ref()
                    .map(|v| CanonicalEmbedding::new(&tower, v).unwrap()),
            )
            .unwrap();
            store
                .apply_batch(
                    &[StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "ze58", key).unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&contents)),
                    }],
                    &control(),
                )
                .unwrap();
        }
        let fixture = Self {
            root,
            store: Some(store),
            tower,
        };
        fixture.run("CREATE (alice:Person {key: 'Alice'}), (project:Project {key: 'X'}), (m:Meeting {key: 'M'}), (alice)-[:ATTENDED]->(m), (alice)-[:ATTENDED]->(m), (m)-[:ABOUT]->(project)");
        fixture.run("MATCH (a:Chunk {key: 'a'}), (b:Chunk {key: 'b'}), (m:Meeting) CREATE (a)-[:FROM_MEETING]->(m), (a)-[:FROM_MEETING]->(m), (b)-[:FROM_MEETING]->(m)");
        fixture
    }
    pub fn apply(&self, writes: &[StructuredWrite<'_, '_>]) {
        self.store().apply_batch(writes, &control()).unwrap();
    }
    pub fn store(&self) -> &GraphStore {
        self.store.as_ref().unwrap()
    }
    pub fn run(&self, text: &str) -> CompletedGraphResult {
        self.try_run(text, &[])
            .unwrap_or_else(|e| panic!("{text}: {e}"))
    }
    pub fn try_run(
        &self,
        text: &str,
        parameters: &[ParameterBinding<'_>],
    ) -> Result<CompletedGraphResult, StatementError> {
        execute(
            self.store().statement_store(),
            &control(),
            &GraphQueryOptions::default(),
            text,
            parameters,
            CompileLimits::default(),
        )
    }
    pub fn reopen(&mut self) {
        self.store.take().unwrap().close().unwrap();
        self.store = Some(
            GraphStore::open(
                &self.root,
                zeppelin_embed::lifecycle::OpenOptions::new()
                    .with_max_resident_bytes(256 * 1024 * 1024),
                Some(self.tower.clone()),
            )
            .unwrap(),
        );
    }
}
impl Drop for SearchFixture {
    fn drop(&mut self) {
        if let Some(store) = self.store.take() {
            let _ = store.close();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
