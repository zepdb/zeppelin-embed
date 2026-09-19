#![allow(clippy::unwrap_used, clippy::panic)]
use zeppelin_embed_cypher::{compile_with, Budget, CompileLimits};
fn binds(query: &str) {
    let result = compile_with(query, &[], CompileLimits::default(), &mut Budget::default(), |_| Ok(()));
    assert!(result.is_ok(), "supported query rejected: {query}: {result:?}");
}
#[test]
fn computed_boolean_property_list_must_bind() {
    binds("CREATE (n {p: [1 = 1]}) RETURN n.p");
}
#[test]
fn computed_numeric_property_list_must_bind() {
    binds("MATCH (n) SET n.p = [n.x + 1] RETURN n.p");
}
#[test]
fn nullable_scalar_property_list_must_bind() {
    binds("MATCH (n) OPTIONAL MATCH ()-[r]->() SET n.p = [type(r)] RETURN n.p");
}

#[test]
fn indexed_deleted_entity_requires_runtime_validation() {
    let query = "MATCH (n) WITH n, [n] AS refs DELETE n RETURN refs[0]";
    compile_with(query, &[], CompileLimits::default(), &mut Budget::default(), |bound| {
        assert!(bound.requires_deleted_runtime_validation(), "index can return the deleted entity: {query}");
        Ok(())
    }).unwrap();
}
#[test]
fn independent_alias_of_deleted_entity_requires_runtime_validation() {
    let query = "MATCH (n), (m) DELETE n RETURN m";
    compile_with(query, &[], CompileLimits::default(), &mut Budget::default(), |bound| {
        assert!(bound.requires_deleted_runtime_validation(), "m can be the same entity as n: {query}");
        Ok(())
    }).unwrap();
}

#[test]
fn nullable_path_list_of_deleted_relationship_requires_runtime_validation() {
    let query = "MATCH ()-[r]->() OPTIONAL MATCH ()-[rs:R*1..2]->() DELETE r RETURN rs";
    compile_with(query, &[], CompileLimits::default(), &mut Budget::default(), |bound| {
        assert!(bound.requires_deleted_runtime_validation(), "optional path can contain deleted r: {query}");
        Ok(())
    }).unwrap();
}

#[test]
fn indexed_delete_target_keeps_subsequent_runtime_validation() {
    let query = "MATCH (n) WITH n, [n][0] AS victim DELETE victim RETURN n";
    compile_with(query, &[], CompileLimits::default(), &mut Budget::default(), |bound| {
        assert!(bound.requires_deleted_runtime_validation(), "DELETE target loses origin but deletes n: {query}");
        Ok(())
    }).unwrap();
}

#[test]
fn order_expression_respects_shadowing_projected_alias() {
    use zeppelin_embed::property_graph::query::plan::{Expression, UnaryExpression};
    use zeppelin_embed_cypher::NodeKind;
    let query = "WITH 1 AS x RETURN DISTINCT -x AS x ORDER BY -x";
    compile_with(query, &[], CompileLimits::default(), &mut Budget::default(), |bound| {
        let order = bound.syntax().nodes().iter().find(|node| matches!(node.kind, NodeKind::Order { .. })).unwrap();
        let root = order.children().first().copied().unwrap();
        let index = bound.syntax().nodes().iter().enumerate().find(|(_, node)| std::ptr::eq(*node, bound.syntax().node(root).unwrap())).unwrap().0;
        assert!(matches!(bound.expressions().get(index), Some(Expression::Unary { operation: UnaryExpression::Negate, .. })), "ORDER -x must negate the projected alias, not substitute the projection value: {:?}", bound.expressions().get(index));
        Ok(())
    }).unwrap();
}

#[test]
fn order_variable_respects_swapped_projected_aliases() {
    use zeppelin_embed::property_graph::query::plan::Expression;
    use zeppelin_embed_cypher::NodeKind;
    let query = "WITH 1 AS x, 2 AS y RETURN DISTINCT x AS y, y AS x ORDER BY x";
    compile_with(query, &[], CompileLimits::default(), &mut Budget::default(), |bound| {
        let projected_x = bound.columns().iter().find(|column| column.name == "x").unwrap();
        let order = bound.syntax().nodes().iter().find(|node| matches!(node.kind, NodeKind::Order { .. })).unwrap();
        let root = order.children().first().copied().unwrap();
        let index = bound.syntax().nodes().iter().enumerate().find(|(_, node)| std::ptr::eq(*node, bound.syntax().node(root).unwrap())).unwrap().0;
        assert!(matches!(bound.expressions().get(index), Some(Expression::Slot(slot)) if *slot == projected_x.slot), "ORDER x must use projected x: got {:?}, expected {:?}", bound.expressions().get(index), projected_x.slot);
        Ok(())
    }).unwrap();
}
