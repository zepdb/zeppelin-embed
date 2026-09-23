//! A fresh native graph store per test, driven only through Cypher text.
#![allow(
    dead_code,
    reason = "shared by several test crates, each using a subset"
)]
use std::path::PathBuf;
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::property_graph::query::completed::{
    CompletedGraphResult, GraphQueryOptions, Outcome,
};
use zeppelin_embed::property_graph::query::plan::ParameterBinding;
use zeppelin_embed_cypher::{CompileLimits, StatementError, execute};

pub(crate) fn store_options() -> OpenOptions {
    OpenOptions::new()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024)
}

pub(crate) struct Graph {
    pub(crate) root: PathBuf,
    pub(crate) store: Option<Store>,
}

impl Graph {
    pub(crate) fn new(prefix: &str) -> Self {
        let root = crate::support::unique_temp_dir(prefix);
        std::fs::create_dir_all(&root).expect("fixture root");
        let store = Store::create_graph_store(root.join("graph"), store_options())
            .expect("create native graph store");
        Self {
            root,
            store: Some(store),
        }
    }

    pub(crate) fn store(&self) -> &Store {
        self.store.as_ref().expect("open store")
    }

    pub(crate) fn run(
        &self,
        text: &str,
        parameters: &[ParameterBinding<'_>],
    ) -> Result<CompletedGraphResult, StatementError> {
        execute(
            self.store(),
            &QueryControl::Cancel(CancelToken::new()),
            &GraphQueryOptions::default(),
            text,
            parameters,
            CompileLimits::default(),
        )
    }

    /// Runs a setup statement that must commit and return no rows.
    pub(crate) fn setup(&self, text: &str) {
        let result = self
            .run(text, &[])
            .unwrap_or_else(|error| panic!("setup {text:?}: {error}"));
        assert!(
            matches!(result.metadata().outcome, Outcome::Committed { .. }),
            "setup {text:?} outcome {:?}",
            result.metadata().outcome
        );
        // Every setup statement ends in a write clause, with no RETURN.
        assert_eq!(result.metadata().rows, 0, "setup {text:?} returned rows");
        assert!(
            result.pools().columns.is_empty(),
            "setup {text:?} returned columns"
        );
    }

    /// Closes and reopens the store from its files.
    pub(crate) fn reopen(&mut self) {
        self.store
            .take()
            .expect("open store")
            .close()
            .expect("close");
        self.store = Some(
            Store::open_graph_store(self.root.join("graph"), store_options()).expect("reopen"),
        );
    }
}

impl Drop for Graph {
    fn drop(&mut self) {
        if let Some(store) = self.store.take() {
            let _ = store.close();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
