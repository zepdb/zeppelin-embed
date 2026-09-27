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

use crate::tck::{self, V};
use std::collections::{BTreeMap, BTreeSet};
use zeppelin_embed::property_graph::GraphGeneration;

type Properties = BTreeMap<String, V>;

#[derive(Debug, PartialEq)]
pub(crate) struct Snapshot {
    pub(crate) nodes: BTreeMap<String, (Vec<String>, Properties)>,
    pub(crate) rels: BTreeMap<String, (String, Properties)>,
}

impl Graph {
    pub(crate) fn generation(&self) -> Result<GraphGeneration, String> {
        let result = self
            .run("RETURN 1 AS probe", &[])
            .map_err(|e| e.to_string())?;
        if result.metadata().outcome != Outcome::Read {
            return Err("generation probe was not a read".to_owned());
        }
        Ok(result.metadata().generation)
    }

    pub(crate) fn snapshot(&self) -> Result<Snapshot, String> {
        let generation = self.generation()?;
        let mut snapshot = Snapshot {
            nodes: BTreeMap::new(),
            rels: BTreeMap::new(),
        };
        for query in [
            "MATCH (n) RETURN ze.node_id(n) AS id, n",
            "MATCH ()-[r]->() RETURN ze.relationship_id(r) AS id, r",
        ] {
            let result = self.run(query, &[]).map_err(|e| e.to_string())?;
            if result.metadata().outcome != Outcome::Read
                || result.metadata().generation != generation
            {
                return Err("snapshot read changed generation or outcome".to_owned());
            }
            for row in tck::actual_table(&result).1 {
                let inserted = match row.as_slice() {
                    [V::Str(id), V::Node(labels, props)] => snapshot
                        .nodes
                        .insert(id.clone(), (labels.clone(), props.clone()))
                        .is_none(),
                    [V::Str(id), V::Rel(kind, props)] => snapshot
                        .rels
                        .insert(id.clone(), (kind.clone(), props.clone()))
                        .is_none(),
                    _ => return Err(format!("invalid snapshot row {row:?}")),
                };
                if !inserted {
                    return Err("duplicate snapshot identity".to_owned());
                }
            }
        }
        if self.generation()? != generation {
            return Err("generation moved after snapshot".to_owned());
        }
        Ok(snapshot)
    }
}

impl Snapshot {
    pub(crate) fn diff(before: &Self, after: &Self) -> BTreeMap<&'static str, u64> {
        let mut counts = BTreeMap::new();
        let labels = |snapshot: &Self| {
            snapshot
                .nodes
                .values()
                .flat_map(|(labels, _)| labels.iter().cloned())
                .collect::<BTreeSet<_>>()
        };
        // Entity kind is part of identity: node and relationship ids have separate domains.
        // A Vec preserves typed V equality, including integer/float distinction.
        let properties = |snapshot: &Self| {
            let mut tuples = Vec::new();
            for (id, (_, props)) in &snapshot.nodes {
                for (key, value) in props {
                    tuples.push((false, id.clone(), key.clone(), value.clone()));
                }
            }
            for (id, (_, props)) in &snapshot.rels {
                for (key, value) in props {
                    tuples.push((true, id.clone(), key.clone(), value.clone()));
                }
            }
            tuples
        };
        for (old, new, nodes, rels, label, property) in [
            (
                before,
                after,
                "+nodes",
                "+relationships",
                "+labels",
                "+properties",
            ),
            (
                after,
                before,
                "-nodes",
                "-relationships",
                "-labels",
                "-properties",
            ),
        ] {
            let old_props = properties(old);
            for (name, count) in [
                (
                    nodes,
                    new.nodes
                        .keys()
                        .filter(|id| !old.nodes.contains_key(*id))
                        .count(),
                ),
                (
                    rels,
                    new.rels
                        .keys()
                        .filter(|id| !old.rels.contains_key(*id))
                        .count(),
                ),
                (label, labels(new).difference(&labels(old)).count()),
                (
                    property,
                    properties(new)
                        .iter()
                        .filter(|tuple| !old_props.contains(tuple))
                        .count(),
                ),
            ] {
                if count != 0 {
                    counts.insert(name, count as u64);
                }
            }
        }
        counts
    }
}

/// Compare original result cells, public side effects, and durable state.
/// Catch shared test-helper assertions so every failure retains its coordinate.
pub(crate) fn check_write(scenario: &tck::Scenario) -> Result<(), String> {
    let checked = std::panic::catch_unwind(|| check_write_inner(scenario));
    let result = match checked {
        Ok(result) => result,
        Err(payload) => Err(payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_else(|| "test helper panicked".to_owned())),
    };
    result.map_err(|error| format!("{}: {error}", scenario.coordinate))
}

fn check_write_inner(scenario: &tck::Scenario) -> Result<(), String> {
    use tck::Expect;
    use zeppelin_embed::property_graph::query::completed::GraphQueryErrorKind;
    let mut graph = Graph::new("ze57-write-tck");
    for setup in &scenario.setup {
        graph.setup(setup);
    }
    if !scenario.parameters.is_empty() {
        return Err("write fixture parameters are unsupported".to_owned());
    }
    let before = graph.snapshot()?;
    let generation = graph.generation()?;
    let result = graph.run(&scenario.query, &[]);
    match (&scenario.expect, result) {
        (Expect::RuntimeError { category, detail }, Err(StatementError::Query(error))) => {
            if category != "ConstraintVerificationFailed"
                || detail != "DeleteConnectedNode"
                || error.kind() != GraphQueryErrorKind::Constraint
                || !error.nothing_committed()
            {
                return Err(format!("unexpected runtime error: {error}"));
            }
            if graph.generation()? != generation {
                return Err("failed statement moved generation".to_owned());
            }
        }
        (_, Err(error)) => return Err(format!("query failed: {error}")),
        (expect, Ok(result)) => {
            let metadata = result.metadata();
            let valid = if scenario.side_effects.is_empty() {
                metadata.outcome == Outcome::NoOp && metadata.generation == generation
            } else {
                matches!(metadata.outcome, Outcome::Committed { changed } if changed > generation)
                    && metadata.generation == generation
            };
            if !valid {
                return Err(format!(
                    "unexpected outcome {:?} at {:?}, before {generation:?}",
                    metadata.outcome, metadata.generation
                ));
            }
            let (columns, actual) = tck::actual_table(&result);
            match expect {
                Expect::Empty => {
                    if !actual.is_empty() {
                        return Err(format!("expected zero rows, got {actual:?}"));
                    }
                    // No RETURN has no projected columns; an empty projection may retain its columns.
                    if !scenario
                        .query
                        .split_whitespace()
                        .any(|word| word.eq_ignore_ascii_case("RETURN"))
                        && !columns.is_empty()
                    {
                        return Err(format!("no RETURN, but columns {columns:?}"));
                    }
                }
                Expect::Table { mode, header, rows } => {
                    if &columns != header {
                        return Err(format!("columns {columns:?}, expected {header:?}"));
                    }
                    let canonical = |rows: &[Vec<V>]| {
                        let mut rows = rows.to_vec();
                        if mode == "bag-lists-unordered" {
                            for row in &mut rows {
                                row.iter_mut().for_each(V::sort_lists);
                            }
                        }
                        let mut rows: Vec<_> = rows.iter().map(|row| format!("{row:?}")).collect();
                        if mode != "ordered" {
                            rows.sort();
                        }
                        rows
                    };
                    if !matches!(mode.as_str(), "ordered" | "bag" | "bag-lists-unordered") {
                        return Err(format!("unknown mode {mode}"));
                    }
                    if canonical(rows) != canonical(&actual) {
                        return Err(format!("rows {actual:?}, expected {rows:?}"));
                    }
                }
                other => return Err(format!("unexpected success for {other:?}")),
            }
            let expected_generation = match metadata.outcome {
                Outcome::Committed { changed } => changed,
                _ => generation,
            };
            if graph.generation()? != expected_generation {
                return Err("result generation differs from store".to_owned());
            }
        }
    }
    let after = graph.snapshot()?;
    let diff: BTreeMap<String, u64> = Snapshot::diff(&before, &after)
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect();
    if diff != scenario.side_effects {
        return Err(format!(
            "side effects {diff:?}, expected {:?}",
            scenario.side_effects
        ));
    }
    graph.reopen();
    if graph.snapshot()? != after {
        return Err("reopened snapshot differs".to_owned());
    }
    Ok(())
}
