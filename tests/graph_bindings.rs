#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::result_large_err
)]
#[path = "support/graph_search_apps.rs"]
mod apps;
#[path = "support/graph_bindings.rs"]
mod bindings;
#[path = "support/graph_search.rs"]
mod graph_search;
use graph_search::*;
use zeppelin_embed::lifecycle::Store;
use zeppelin_embed_adversarial_oracle::graph_fixture as oracle;
use zeppelin_embed_ffi::*;
fn rust_cell(
    r: &zeppelin_embed::property_graph::query::completed::CompletedGraphResult,
    v: zeppelin_embed::property_graph::query::completed::Value,
) -> String {
    use zeppelin_embed::property_graph::query::completed::Value;
    match v {
        Value::Null => "N".into(),
        Value::Bool(b) => format!("B{}", u32::from(b)),
        Value::I64(i) => format!("I{i}"),
        Value::F64(f) => format!("F{:016x}", f),
        Value::String(s) => {
            let s = r.string(s).unwrap();
            format!("S{}:{s}", s.len())
        }
        Value::Node(i) => format!("D{:032x}", r.pools().nodes[i as usize].id.get()),
        Value::Relationship(i) => format!("R{:032x}", r.pools().relationships[i as usize].id.get()),
        Value::List { children, .. } => {
            let values = r.pools().children
                [children.start as usize..(children.start + children.len) as usize]
                .iter()
                .map(|i| rust_cell(r, r.pools().values[i.0 as usize]))
                .collect::<Vec<_>>();
            format!("L0[{}]", values.join(","))
        }
    }
}
fn rust_observe(
    r: &zeppelin_embed::property_graph::query::completed::CompletedGraphResult,
) -> String {
    (0..r.metadata().rows as usize)
        .map(|row| {
            (0..r.pools().columns.len())
                .map(|col| rust_cell(r, *r.cell(row, col).unwrap()))
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn canonical(cell: &oracle::Cell) -> String {
    match cell {
        oracle::Cell::Null => "N".into(),
        oracle::Cell::Bool(b) => format!("B{}", u32::from(*b)),
        oracle::Cell::I64(i) => format!("I{i}"),
        oracle::Cell::Score(f) => format!("F{f:016x}"),
        oracle::Cell::String(s) => format!("S{}:{s}", s.len()),
        oracle::Cell::Node(id) => format!("D{id:032x}"),
        oracle::Cell::Relationship(id) => format!("R{id:032x}"),
        oracle::Cell::List(v) => format!(
            "L0[{}]",
            v.iter().map(canonical).collect::<Vec<_>>().join(",")
        ),
    }
}
fn check_c_nodes(r: &ZeGraphResponse, snapshot: &oracle::Snapshot, payloads: bool) {
    let bytes = unsafe {
        if r.pool.byte_count == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(r.pool.bytes, r.pool.byte_count)
        }
    };
    let text = |range: ZeGraphRange| {
        std::str::from_utf8(&bytes[range.start as usize..(range.start + range.count) as usize])
            .unwrap()
    };
    if r.pool.node_count == 0 {
        return;
    }
    for node in unsafe { std::slice::from_raw_parts(r.pool.nodes, r.pool.node_count) } {
        let id = (u128::from(node.id.high) << 64) | u128::from(node.id.low);
        let truth = snapshot.nodes.iter().find(|n| n.id == id).unwrap();
        assert_eq!(node.revision, truth.revision);
        assert_eq!(node.last_change_generation, truth.generation);
        let key = truth.key.as_ref().unwrap();
        assert_eq!(node.has_key, 1);
        assert_eq!(text(node.namespace_name), key.namespace);
        assert_eq!(text(node.key), key.value);
        let names = unsafe { std::slice::from_raw_parts(r.pool.names, r.pool.name_count) };
        let labels = names
            [node.labels.start as usize..(node.labels.start + node.labels.count) as usize]
            .iter()
            .map(|n| text(*n).to_string())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(labels, truth.labels);
        let props = unsafe { std::slice::from_raw_parts(r.pool.properties, r.pool.property_count) };
        let values = unsafe { std::slice::from_raw_parts(r.pool.values, r.pool.value_count) };
        assert_eq!(node.properties.count, truth.properties.len() as u32);
        for p in &props[node.properties.start as usize
            ..(node.properties.start + node.properties.count) as usize]
        {
            let value = match &truth.properties[text(p.name)] {
                oracle::Property::Scalar(oracle::Scalar::String(s)) => format!("S{}:{s}", s.len()),
                oracle::Property::Scalar(oracle::Scalar::I64(i)) => format!("I{i}"),
                _ => panic!("property outside bounded application corpus"),
            };
            assert_eq!(bindings::c_cell(r, values[p.value as usize]), value);
        }
        if payloads {
            assert_eq!(node.has_text, u32::from(truth.text.is_some()));
            if let Some(expected) = &truth.text {
                assert_eq!(text(node.text), expected);
            }
            assert_eq!(node.has_vector, u32::from(truth.vector.is_some()));
            if let Some(expected) = &truth.vector {
                let vectors =
                    unsafe { std::slice::from_raw_parts(r.pool.vectors, r.pool.vector_count) };
                assert_eq!(
                    vectors[node.vector.start as usize
                        ..(node.vector.start + node.vector.count) as usize]
                        .iter()
                        .map(|v| v.to_bits())
                        .collect::<Vec<_>>(),
                    *expected
                );
            }
        }
    }
}
#[test]
fn ze72_rust_c_applications_match_independent_oracle() {
    let mut c = Corpus::new();
    let mut cases = Vec::new();
    for shape in 0..3 {
        let q = match shape {
            0 => oracle::Query::ProjectEvidence {
                project: 1,
                limit: 20,
            },
            1 => oracle::Query::SemanticContext {
                vector: vec![0.0; 2],
                k: 20,
            },
            _ => oracle::Query::AliceProjectRanking {
                person: 2,
                project: 1,
                vector: vec![0.0; 2],
                k: 20,
            },
        };
        let rows = oracle::query(&c.snapshot(), &q).unwrap();
        let expected = rows
            .iter()
            .map(|r| r.iter().map(canonical).collect::<Vec<_>>().join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        for r in [
            apps::structured_application(c.graph(), shape),
            c.run(apps::APPLICATIONS[shape]),
        ] {
            oracle::compare_scored_rows(&rows, &observe(&r), ABSOLUTE, RELATIVE).unwrap();
            assert_eq!(r.pools().reports.len(), usize::from(shape != 0));
            for report in r.pools().reports {
                assert_eq!(report.generation.get(), c.model.generation);
                assert_eq!(report.call.0, 0);
                assert_eq!(
                    report.kind,
                    zeppelin_embed::property_graph::query::completed::SearchKind::Vector
                );
            }
        }
        cases.push((
            format!("application-{shape}"),
            apps::APPLICATIONS[shape].to_string(),
            expected,
            usize::from(shape != 0),
        ));
    }
    for q in [
        "CALL ze.vector_search([0,0],2,'exact',[]) YIELD node RETURN node",
        "CALL ze.vector_search([0,0],2,'exact') YIELD node RETURN count(*) LIMIT 0",
    ] {
        let r = c.run(q);
        assert_eq!(r.metadata().rows, 0);
        assert_eq!(r.pools().reports.len(), 1);
        cases.push(("empty-report".into(), q.into(), String::new(), 1));
    }
    let snapshot = c.snapshot();
    let rust_ids = snapshot
        .nodes
        .iter()
        .map(|n| zeppelin_embed::property_graph::NodeId::new(n.id).unwrap())
        .collect::<Vec<_>>();
    let rust_nodes = c
        .graph()
        .get_nodes(
            &rust_ids,
            zeppelin_embed::property_graph::GraphGetOptions {
                text: true,
                vector: true,
            },
            &control(),
        )
        .unwrap();
    let rust_rel_ids = snapshot
        .relationships
        .iter()
        .map(|r| zeppelin_embed::property_graph::RelId::new(r.id).unwrap())
        .collect::<Vec<_>>();
    let rust_rels = c
        .graph()
        .get_relationships(&rust_rel_ids, &control())
        .unwrap();
    assert!(
        c.graph()
            .get_nodes(&[], Default::default(), &control())
            .unwrap()
            .nodes()
            .is_empty()
    );
    assert!(
        c.graph()
            .get_relationships(&[], &control())
            .unwrap()
            .relationships()
            .is_empty()
    );
    // Rust and C read the identical persisted corpus; truth is the PG13 model.
    assert_eq!(c.store.take().unwrap().release(), 0);
    for (node, truth) in rust_nodes.nodes().iter().zip(&snapshot.nodes) {
        let node = node.unwrap();
        assert_eq!(node.id.get(), truth.id);
        assert_eq!(node.revision.get(), truth.revision);
        assert_eq!(node.generation.get(), truth.generation);
        assert_eq!(
            node.text.and_then(|s| rust_nodes.string(s)),
            truth.text.as_deref()
        );
        assert_eq!(
            node.vector.map(|s| rust_nodes.vector(s)),
            truth.vector.as_deref()
        );
        let key = node.key.unwrap();
        let expected = truth.key.as_ref().unwrap();
        assert_eq!(
            rust_nodes.string(key.namespace),
            Some(expected.namespace.as_str())
        );
        assert_eq!(rust_nodes.string(key.value), Some(expected.value.as_str()));
    }
    for (rel, truth) in rust_rels
        .relationships()
        .iter()
        .zip(&snapshot.relationships)
    {
        let rel = rel.unwrap();
        assert_eq!(rel.id.get(), truth.id);
        assert_eq!(rel.source.get(), truth.source);
        assert_eq!(rel.target.get(), truth.target);
        assert_eq!(rel.revision.get(), truth.revision);
        assert_eq!(rel.generation.get(), truth.generation);
        assert_eq!(
            rust_rels.string(rel.relationship_type),
            Some(truth.relationship_type.as_str())
        );
    }
    let path = c.dir.path().join("graph");
    let bytes = path.to_str().unwrap().as_bytes();
    let model = b"ze41-document";
    let version = b"1";
    let digest = [0x41, 0xa5];
    let prefix = b"doc: ";
    let tower = ZeEmbeddingTower {
        model_id: model.as_ptr(),
        model_id_len: model.len(),
        model_version: version.as_ptr(),
        model_version_len: version.len(),
        weights_digest: digest.as_ptr(),
        weights_digest_len: digest.len(),
        dims: 2,
        normalization: 0,
        prompt_prefix: prefix.as_ptr(),
        prompt_prefix_len: prefix.len(),
        max_tokens: 32,
        runtime: 3,
        compute_units: 1,
        has_os_build: 0,
        os_build: std::ptr::null(),
        os_build_len: 0,
    };
    let mut open: ZeGraphOpenRequest = bindings::sized();
    open.path = ZeGraphBytes {
        data: bytes.as_ptr(),
        count: bytes.len(),
    };
    open.mode = 1;
    open.max_resident_bytes = 256 << 20;
    open.document_tower = &tower;
    let mut handle = ZeGraphHandle { token: 0 };
    assert_eq!(ze_graph_open(&open, &mut handle), ZeErrorCode::ZeOk);
    for (name, q, expected, reports) in &cases {
        let mut r = bindings::query(handle, q);
        let mut actual = bindings::c_observe(&r);
        if std::env::var("ZE72_MUTATION").as_deref() == Ok("bag") && name == "application-0" {
            actual = actual.lines().next().unwrap().into();
        }
        bindings::compare(name, expected, &actual).unwrap();
        assert_eq!(r.report_count, *reports);
        check_c_nodes(&r, &c.snapshot(), false);
        let native_reports = unsafe {
            if r.report_count == 0 {
                &[]
            } else {
                std::slice::from_raw_parts(r.reports, r.report_count)
            }
        };
        let mut observed_reports = native_reports
            .iter()
            .map(|report| (report.call_id, report.kind, report.generation))
            .collect::<Vec<_>>();
        if std::env::var("ZE72_MUTATION").as_deref() == Ok("provenance") {
            observed_reports.clear();
        }
        zeppelin_embed_adversarial_oracle::graph_c_entry::compare_reports(
            *reports,
            c.model.generation,
            &observed_reports,
        )
        .unwrap();
        assert_eq!(ze_graph_response_free(&mut r), ZeErrorCode::ZeOk);
    }
    let snapshot = c.snapshot();
    let ids = snapshot
        .nodes
        .iter()
        .map(|n| ZeNodeId {
            high: (n.id >> 64) as u64,
            low: n.id as u64,
        })
        .collect::<Vec<_>>();
    let mut get: ZeGraphGetNodesRequest = bindings::sized();
    get.ids = ids.as_ptr();
    get.id_count = ids.len();
    get.include_text = 1;
    get.include_vector = 1;
    let mut payloads: ZeGraphResponse = bindings::sized();
    assert_eq!(
        ze_graph_get_nodes(handle, &get, &mut payloads),
        ZeErrorCode::ZeOk
    );
    assert_eq!(payloads.row_count, ids.len());
    check_c_nodes(&payloads, &snapshot, true);
    assert_eq!(ze_graph_close(handle), ZeErrorCode::ZeOk);
    check_c_nodes(&payloads, &snapshot, true);
    assert_eq!(ze_graph_response_free(&mut payloads), ZeErrorCode::ZeOk);
    if let Ok(output) = std::env::var("ZE72_CORPUS_OUTPUT") {
        let mut observations = String::new();
        for (name, q, expected, reports) in cases {
            observations.push_str(&format!(
                "{name}\t{q}\t{}\t{reports}\t{}\n",
                expected.replace('\n', "\\n"),
                c.model.generation
            ));
        }
        std::fs::write(&output, observations).unwrap();
        let kept = std::mem::replace(&mut c.dir, tempfile::tempdir().unwrap()).keep();
        std::fs::write(
            format!("{output}.path"),
            kept.join("graph").to_str().unwrap(),
        )
        .unwrap();
    }
}
#[test]
fn ze72_rust_shared_semantics_match_independent_oracle() {
    let dir = tempfile::tempdir().unwrap();
    let graph = zeppelin_embed::lifecycle::Store::create_graph(
        dir.path().join("graph"),
        zeppelin_embed::lifecycle::OpenOptions::new().with_max_resident_bytes(256 << 20),
        None,
    )
    .unwrap();
    let run = |q| {
        zeppelin_embed_cypher::execute(
            &graph,
            &control(),
            &Default::default(),
            q,
            &[],
            Default::default(),
        )
        .unwrap()
    };
    run("CREATE (:Fixture), (:Fixture)");
    for (name, q, expected) in bindings::cases() {
        bindings::compare(name, &expected, &rust_observe(&run(q))).unwrap();
    }
    graph.close_graph().unwrap();
}

#[test]
fn ze72_rust_stored_list_kinds_match_shared_fixture() {
    use zeppelin_embed::property_graph::query::completed::Value;
    use zeppelin_embed::property_graph::staging::*;
    use zeppelin_embed::property_graph::*;
    let dir = tempfile::tempdir().unwrap();
    let graph = Store::create_graph(
        dir.path().join("properties"),
        zeppelin_embed::lifecycle::OpenOptions::new().with_max_resident_bytes(256 << 20),
        None,
    )
    .unwrap();
    let cases = bindings::property_cases();
    let mut labels = [GraphName::new("Payload").unwrap()];
    let mut props = cases
        .iter()
        .map(|(name, kind, _)| {
            GraphProperty::new(
                GraphName::new(name).unwrap(),
                PropertyValue::new(match kind {
                    1 => PropertyData::Bools(&[]),
                    2 => PropertyData::Integers(&[]),
                    3 => PropertyData::Floats(&[]),
                    4 => PropertyData::Strings(&[]),
                    5 => PropertyData::EmptyList { count: 0 },
                    _ => panic!("invalid fixture kind"),
                })
                .unwrap(),
            )
        })
        .collect::<Vec<_>>();
    let image = CanonicalContents::node(&mut labels, &mut props, Some(""), None).unwrap();
    let written = graph
        .graph_apply(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze72", "payload").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &control(),
        )
        .unwrap();
    let EntityId::Node(id) = written.receipts()[0].entity else {
        panic!("node receipt");
    };
    let result = graph
        .get_nodes(
            &[id],
            GraphGetOptions {
                text: true,
                vector: true,
            },
            &control(),
        )
        .unwrap();
    graph.close_graph().unwrap();
    let node = result.nodes()[0].unwrap();
    assert_eq!(node.text.and_then(|s| result.string(s)), Some(""));
    assert!(node.vector.is_none());
    for (name, _, expected) in cases {
        let p = result
            .properties(node.properties)
            .iter()
            .find(|p| result.string(p.name) == Some(name))
            .unwrap();
        let Value::List { element, children } = result.value(p.value).unwrap() else {
            panic!("stored list lost");
        };
        assert_eq!(children.len, 0);
        let kind = match element {
            zeppelin_embed::property_graph::query::completed::ListKind::Query => 0,
            zeppelin_embed::property_graph::query::completed::ListKind::Bool => 1,
            zeppelin_embed::property_graph::query::completed::ListKind::I64 => 2,
            zeppelin_embed::property_graph::query::completed::ListKind::F64 => 3,
            zeppelin_embed::property_graph::query::completed::ListKind::String => 4,
            zeppelin_embed::property_graph::query::completed::ListKind::Empty => 5,
        };
        bindings::compare(name, expected, &format!("L{kind}[]")).unwrap();
    }
}
