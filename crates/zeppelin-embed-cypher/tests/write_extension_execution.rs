//! Local ZE-57 extensions, not original TCK scenarios.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::result_large_err
)]
#[path = "support/graph.rs"]
mod graph;
mod support;
#[path = "support/tck.rs"]
mod tck;

use zeppelin_embed::property_graph::query::completed::GraphQueryErrorKind;
use zeppelin_embed_cypher::StatementError;

/// Local pin: ZE-190 owns enabling fence-only publication; flip this when it lands.
#[test]
fn ze57_fence_only_statement_is_refused_until_ze190() {
    let graph = graph::Graph::new("ze57-fence-only");
    let before = graph.generation().unwrap();
    match graph.run("CREATE (m)-[r:R]->(k) DELETE m, k, r", &[]) {
        Err(StatementError::Query(error)) => {
            assert_eq!(error.kind(), GraphQueryErrorKind::InvalidPlan);
            assert!(error.nothing_committed());
        }
        Err(error) => panic!("expected query refusal, got {error}"),
        Ok(_) => panic!("expected fence-only refusal, executed"),
    }
    assert_eq!(graph.generation().unwrap(), before);
    graph.setup("CREATE ()");
    assert!(graph.generation().unwrap() > before);
}
