mod support;

use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl};
use zeppelin_embed::property_graph::GraphStore;
use zeppelin_embed::property_graph::query::completed::{GraphQueryOptions, Outcome, Value};
use zeppelin_embed_cypher::{CompileLimits, execute};

#[test]
#[allow(clippy::expect_used, clippy::panic)]
fn ze69_cypher_executes_on_a_graph_store_through_statement_store() {
    let dir = support::unique_temp_dir("ze69-graph-store-statement");
    std::fs::create_dir(&dir).expect("create temporary directory");
    let store = GraphStore::create(
        dir.join("graph"),
        OpenOptions::new().with_max_resident_bytes(256 << 20),
        None,
    )
    .expect("create graph store");
    let control = QueryControl::Cancel(CancelToken::new());
    let created = execute(
        store.statement_store(),
        &control,
        &GraphQueryOptions::default(),
        "CREATE (:Doc {title: 'alpha'})",
        &[],
        CompileLimits::default(),
    )
    .expect("execute CREATE");
    assert!(matches!(
        created.metadata().outcome,
        Outcome::Committed { .. }
    ));

    let result = execute(
        store.statement_store(),
        &control,
        &GraphQueryOptions::default(),
        "MATCH (n:Doc) RETURN n.title AS title",
        &[],
        CompileLimits::default(),
    )
    .expect("execute MATCH");
    assert_eq!(result.metadata().rows, 1);
    assert_eq!(result.pools().columns.len(), 1);
    let Some(Value::String(title)) = result.cell(0, 0) else {
        panic!("expected a string cell");
    };
    assert_eq!(result.string(*title), Some("alpha"));
    store.close().expect("close graph store");
    std::fs::remove_dir_all(dir).expect("remove fixture");
}
