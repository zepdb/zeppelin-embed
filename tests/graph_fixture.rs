#[path = "tooling_seed.rs"]
mod test_support;
use rand::Rng;
use zeppelin_embed_bench::graph_fixture::*;
#[test]
fn approved_fixture_counts_are_observed_from_every_baseline_and_stress_record() {
    for (scale, nodes, edges, vectors) in [
        (Scale::Baseline, 266375, 1003750, 182500),
        (Scale::Stress, 2663750, 10037500, 1825000),
    ] {
        let mut topics = test_support::seeded_rng("graph-fixture-v1/topics", ROOT_SEED);
        let actual = inventory(Config::new(scale), &mut || topics.random()).unwrap();
        assert_eq!(
            (actual.nodes, actual.edges, actual.vectors),
            (nodes, edges, vectors),
            "actual generated records must match approved corpus"
        );
        let scale = if scale == Scale::Baseline { 1 } else { 10 };
        assert_eq!(
            (
                actual.absent_text,
                actual.empty_text,
                actual.whitespace_text,
                actual.indexed_text
            ),
            (11825 * scale, 1825 * scale, 1825 * scale, 250900 * scale)
        );
        assert_eq!(
            (
                actual.both,
                actual.vector_only,
                actual.text_only,
                actual.neither
            ),
            (177025 * scale, 5475 * scale, 73875 * scale, 10000 * scale)
        );
        assert_eq!(actual.vector_bytes, 560640000 * scale);
        assert_eq!(
            actual.project_hub_degree,
            if scale == 1 { 24651 } else { 246375 }
        );
        assert_eq!(actual.indegree.values().sum::<u64>(), nodes);
        assert_eq!(actual.outdegree.values().sum::<u64>(), nodes);
        assert_eq!(actual.degree.values().sum::<u64>(), nodes);
        assert_eq!(
            actual.indegree.iter().map(|(d, n)| d * n).sum::<u64>(),
            edges
        );
        assert_eq!(
            actual.outdegree.iter().map(|(d, n)| d * n).sum::<u64>(),
            edges
        );
        assert_eq!(actual.max_batch_changes, 256);
        println!("scale={scale} inventory={actual:?}");
    }
}
#[test]
fn normalized_low_11_bit_recipe_matches_independent_axis_byte_goldens() {
    let mut generator = VectorGenerator::new(&mut |name| {
        let axis = if name.ends_with("centroids") { 0 } else { 1 };
        let mut n = 0;
        Box::new(move || {
            let word = if n % 768 == axis { 1025 } else { 1024 };
            n += 1;
            word
        })
    })
    .unwrap();
    let chunk = generator.chunk(0, false).unwrap();
    assert_eq!(
        &chunk[..2],
        &[0x3f7d6d54, 0x3e10d0c3],
        "ordered f64 7:1 mixture and final f32 bits"
    );
    assert!(chunk[2..].iter().all(|b| *b == 0));
    let query = generator.query(0).unwrap();
    assert_eq!(&query[..2], &[0x3f7fddee, 0x3d040f72]);
    let zero = VectorGenerator::new(&mut |_| Box::new(|| 1024));
    assert!(zero.is_err());
}

#[test]
fn serialized_fixture_is_reproducible_and_rejects_missing_or_modified_files() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::new(Scale::Small);
    let mut factory = |name: &str| {
        let mut rng = test_support::seeded_rng(name, config.seed);
        Box::new(move || rng.random()) as WordStream
    };
    let manifest = write_fixture(dir.path(), config, "test-source-pin", &mut factory).unwrap();
    assert_eq!(
        (manifest.nodes, manifest.edges, manifest.vectors),
        (131, 440, 80)
    );
    assert_eq!(manifest.initial_batches, 5);
    assert_eq!(manifest.state_b_nodes, 1);
    assert_eq!(manifest.state_b_relationships, 8);
    assert_eq!(manifest.queries, 100);
    validate_fixture(dir.path()).unwrap();
    let parsed = read_manifest(dir.path()).unwrap();
    let digests = parsed["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["name"].as_str().unwrap(), f["sha256"].as_str().unwrap()))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(
        digests["vectors-a.f32le"],
        "bb92c3a66a51f4b6032d73cc332920c982f603f2aa21e0022fc0888079c32260"
    );
    assert_eq!(
        digests["vectors-b.f32le"],
        "e14dd689b53d9b45f2c004f3a46486884177b838622e5a54224fedbcc5c873a0"
    );
    assert_eq!(
        digests["query-vectors.f32le"],
        "f4c9b9251f10ccd6e501f68dfbcb66c0cc822c66059b5b7a99bc330cdf0e1cd1"
    );
    assert_eq!(
        digests["batches-a.jsonl"],
        "66ff4862548c8449ceaa3f947baa284e1b93741171f75c0b803456e64e0f061a"
    );
    assert_eq!(
        digests["batches-b.jsonl"],
        "d09142a37ed05be7b9f5740d5bf1c8b7e3b79ba1c8960730d52ccfc22dcd4115"
    );
    assert_eq!(
        digests["queries.jsonl"],
        "b1f3fae4aa09d1317c31bfe67c49c6af0a95c99f2e558d54c9a18b2d903e9ae4"
    );
    let mut a = Vec::new();
    visit_batches(dir.path(), FixtureState::A, &mut |row| {
        a.push(row);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        a.iter()
            .map(|row| row["changes"].as_array().unwrap().len())
            .collect::<Vec<_>>(),
        [23, 137, 137, 137, 137]
    );
    let mut b = Vec::new();
    visit_batches(dir.path(), FixtureState::B, &mut |row| {
        b.push(row);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        b.iter()
            .map(|row| row["changes"].as_array().unwrap().len())
            .collect::<Vec<_>>(),
        [1, 8, 4]
    );
    for (index, row) in a.iter().enumerate() {
        assert_eq!(row["expected_disposition"], "changed");
        assert_eq!(row["mutation_ordinal"], (index + 1) as u64);
        assert_eq!(row["expected_generation"]["relative_to"], "admitted");
        assert_eq!(row["expected_generation"]["increment"], 1);
    }
    for (index, row) in b.iter().enumerate() {
        assert_eq!(row["expected_disposition"], "changed");
        assert_eq!(row["mutation_ordinal"], (index + 6) as u64);
        assert_eq!(row["expected_generation"]["relative_to"], "admitted");
        assert_eq!(row["expected_generation"]["increment"], 1);
    }
    assert_eq!(b[2]["after"], "retain-active-tail");
    assert_eq!(b[1]["after"], "checkpoint-and-consolidate");
    let old = a
        .iter()
        .flat_map(|r| r["changes"].as_array().unwrap())
        .find(|change| change["image"]["ordinal"] == 109 && change["image"]["kind"] == "node")
        .unwrap();
    let changed = &b[0]["changes"][0];
    assert_eq!(old["image"]["key"], changed["image"]["key"]);
    assert_ne!(
        old["image"]["properties"]["name"],
        changed["image"]["properties"]["name"]
    );
    assert_eq!(
        old["image"]["text"].as_str().unwrap().len(),
        changed["image"]["text"].as_str().unwrap().len()
    );
    for recreated in b[2]["changes"].as_array().unwrap() {
        let deleted = b[1]["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|change| {
                change["operation"] == "delete" && change["key"] == recreated["image"]["key"]
            })
            .unwrap();
        assert_eq!(deleted["revision"], 2);
        assert_eq!(recreated["revision"], 3);
        assert_eq!(recreated["expected"]["deletion_revision"], 2);
    }
    let other = tempfile::tempdir().unwrap();
    let mut fresh = |name: &str| {
        let mut rng = test_support::seeded_rng(name, config.seed);
        Box::new(move || rng.random()) as WordStream
    };
    write_fixture(other.path(), config, "test-source-pin", &mut fresh).unwrap();
    for name in digests.keys() {
        assert_eq!(
            std::fs::read(dir.path().join(name)).unwrap(),
            std::fs::read(other.path().join(name)).unwrap(),
            "repeated generator differs: {name}"
        );
    }
    let original_manifest = read_manifest(dir.path()).unwrap();
    let mut bad = original_manifest.clone();
    bad["version"] = "graph-fixture-v2".into();
    std::fs::write(dir.path().join("manifest.json"), bad.to_string()).unwrap();
    assert!(validate_fixture(dir.path()).is_err());
    bad = original_manifest.clone();
    bad["generation_contract"]["mutation"] = "mutation-ordinal".into();
    std::fs::write(dir.path().join("manifest.json"), bad.to_string()).unwrap();
    assert!(validate_fixture(dir.path()).is_err());
    bad = original_manifest.clone();
    bad["inventory"]["nodes"] = 130.into();
    std::fs::write(dir.path().join("manifest.json"), bad.to_string()).unwrap();
    assert!(validate_fixture(dir.path()).is_err());
    std::fs::write(
        dir.path().join("manifest.json"),
        original_manifest.to_string(),
    )
    .unwrap();
    validate_fixture(dir.path()).unwrap();
    let vectors = dir.path().join("vectors-a.f32le");
    assert_eq!(std::fs::metadata(&vectors).unwrap().len(), 245760);
    let before = std::fs::read(&vectors).unwrap();
    let mut changed = before.clone();
    changed[0] ^= 1;
    std::fs::write(&vectors, &changed).unwrap();
    assert!(validate_fixture(dir.path()).is_err());
    std::fs::write(&vectors, before).unwrap();
    validate_fixture(dir.path()).unwrap();
    std::fs::remove_file(&vectors).unwrap();
    assert!(validate_fixture(dir.path()).is_err());
}
#[test]
fn serialized_generation_expectations_follow_admitted_views_through_maintenance() {
    use zeppelin_embed_bench::graph_fixture::{
        FixtureState, WordStream, read_manifest, visit_batches, write_fixture,
    };
    let dir = tempfile::tempdir().unwrap();
    let config = Config::new(Scale::Small);
    let mut factory = |name: &str| {
        let mut rng = test_support::seeded_rng(name, config.seed);
        Box::new(move || rng.random()) as WordStream
    };
    write_fixture(dir.path(), config, "generation-control", &mut factory).unwrap();
    let manifest = read_manifest(dir.path()).unwrap();
    let mut admitted = 0_u64;
    let mut ordinal = 0_u64;
    let mut changed_generations = Vec::new();
    for state in [FixtureState::A, FixtureState::B] {
        visit_batches(dir.path(), state, &mut |row| {
            ordinal += 1;
            assert_eq!(
                row["mutation_ordinal"], ordinal,
                "PG13 mutation ordinal must not prescribe maintenance generations"
            );
            assert_eq!(row["expected_generation"]["relative_to"], "admitted");
            assert_eq!(row["expected_generation"]["increment"], 1);
            admitted += row["expected_generation"]["increment"].as_u64().unwrap();
            changed_generations.push(admitted);
            // A changed consolidation is permitted after every requested barrier.
            if row["after"] == "checkpoint-and-consolidate" {
                admitted += 1;
            }
            Ok(())
        })
        .unwrap();
        if matches!(state, FixtureState::A) {
            admitted += 1;
        }
    }
    assert_eq!(changed_generations, [1, 2, 3, 4, 5, 7, 9, 11]);
    assert_eq!(manifest["state_a"]["mutation_batches"], 5);
    assert_eq!(manifest["state_a"]["generation"], "record-after-barrier");
}
#[test]
fn pinned_fixture_vocabulary_matches_the_declared_product_analyzer() {
    use zeppelin_embed::fts::tokenizer::{Analyzer, TokenizerConfig};
    let analyzer = Analyzer::new(TokenizerConfig::text_default()).unwrap();
    let words = [
        "amber", "cedar", "cobalt", "delta", "quartz", "velvet", "zephyr",
    ];
    assert_eq!(
        analyzer
            .analyze(&words.join(" "))
            .iter()
            .map(|t| t.term.as_str())
            .collect::<Vec<_>>(),
        words
    );
    assert!(analyzer.analyze("   ").is_empty());
    println!("fixture tokenizer epoch={:?}", analyzer.epoch());
}
