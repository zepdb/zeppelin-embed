#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::path::PathBuf;
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::property_graph::{
    query::resources::{QueryArena, QueryMemory},
    resources::GraphResources,
};
use zeppelin_embed_cypher::{CompileLimits, ErrorKind, ResourceError, compile_in};
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = support::unique_temp_dir("ze-55-binding-resources");
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn shared_compilation_holds_frontend_and_ir_copies_in_one_budget() {
    let directory = Scratch::new();
    let store = Store::open(
        &directory.0,
        OpenOptions::new().with_max_resident_bytes(2 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let initial = shared.reserved_bytes().unwrap();
    {
        let memory = QueryMemory::new(&shared, 1024 * 1024).unwrap();
        let base = memory.reserved_bytes();
        let control = QueryControl::Cancel(CancelToken::new());
        compile_in(
            "RETURN 'λ' AS text",
            &[],
            CompileLimits::default(),
            &memory,
            &control,
            |bound| {
                assert_eq!(bound.columns().len(), 1);
                let frontend = memory.reserved_bytes();
                assert!(frontend > base + 65536);
                let mut copied = QueryArena::<u8>::new(&memory, 2).unwrap();
                copied.push(0xce).unwrap();
                copied.push(0xbb).unwrap();
                assert_eq!(memory.reserved_bytes(), frontend + copied.reserved_bytes());
                assert_eq!(
                    shared.reserved_bytes().unwrap(),
                    initial + memory.reserved_bytes() as u64
                );
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(memory.reserved_bytes(), base);
        let tight = QueryMemory::new(&shared, 1024).unwrap();
        let tight_base = tight.reserved_bytes();
        let error = compile_in(
            "RETURN 1",
            &[],
            CompileLimits::default(),
            &tight,
            &control,
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Resource(ResourceError::Memory));
        assert_eq!(tight.reserved_bytes(), tight_base);
    }
    assert_eq!(shared.reserved_bytes().unwrap(), initial);
    store.close().unwrap();
}

#[derive(Default)]
struct Faults {
    polls: usize,
    charges: usize,
    fail_poll: usize,
    fail_charge: usize,
    fires: usize,
}
impl zeppelin_embed_cypher::Resources for Faults {
    fn charge(&mut self, _: usize) -> Result<(), ResourceError> {
        self.charges += 1;
        if self.charges == self.fail_charge {
            self.fires += 1;
            return Err(ResourceError::Memory);
        }
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ResourceError> {
        self.polls += 1;
        if self.polls == self.fail_poll {
            self.fires += 1;
            return Err(ResourceError::Cancelled);
        }
        Ok(())
    }
}
#[test]
fn binding_faults_fire_after_parsing_with_same_input_clean_controls() {
    use zeppelin_embed_cypher::{compile_with, parse_with};
    let query = "MATCH (n) WITH collect(DISTINCT n) AS eligible CALL ze.text_search('a', 1, eligible) YIELD node, score RETURN ze.node_id(node) AS id, score";
    let mut syntax = Faults::default();
    drop(parse_with(query, CompileLimits::default(), &mut syntax).unwrap());
    let mut clean = Faults::default();
    compile_with(query, &[], CompileLimits::default(), &mut clean, |_| Ok(())).unwrap();
    assert!(clean.polls > syntax.polls);
    assert!(clean.charges > syntax.charges);
    for position in [
        syntax.polls + 1,
        (syntax.polls + clean.polls) / 2,
        clean.polls - 1,
    ] {
        let mut fault = Faults {
            fail_poll: position,
            ..Faults::default()
        };
        let mut consumed = false;
        let error = compile_with(query, &[], CompileLimits::default(), &mut fault, |_| {
            consumed = true;
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Resource(ResourceError::Cancelled));
        assert_eq!(fault.fires, 1);
        assert!(!consumed);
        compile_with(
            query,
            &[],
            CompileLimits::default(),
            &mut Faults::default(),
            |_| Ok(()),
        )
        .unwrap();
    }
    for position in [syntax.charges + 1, clean.charges] {
        let mut fault = Faults {
            fail_charge: position,
            ..Faults::default()
        };
        let error =
            compile_with(query, &[], CompileLimits::default(), &mut fault, |_| Ok(())).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Resource(ResourceError::Memory));
        assert_eq!(fault.fires, 1);
        compile_with(
            query,
            &[],
            CompileLimits::default(),
            &mut Faults::default(),
            |_| Ok(()),
        )
        .unwrap();
    }
}

#[test]
fn cancelled_consumer_cannot_publish_the_compiler_handoff() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use zeppelin_embed_cypher::{Budget, compile_with};
    let flag = AtomicBool::new(false);
    let mut budget = Budget::new(1024 * 1024, Some(&flag));
    let error = compile_with(
        "RETURN 1",
        &[],
        CompileLimits::default(),
        &mut budget,
        |_| {
            flag.store(true, Ordering::Relaxed);
            Ok(7)
        },
    )
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Resource(ResourceError::Cancelled));
    flag.store(false, Ordering::Relaxed);
    assert_eq!(
        compile_with(
            "RETURN 1",
            &[],
            CompileLimits::default(),
            &mut budget,
            |_| Ok(7)
        )
        .unwrap(),
        7
    );
}

#[test]
fn shared_compilation_releases_on_timeout_and_late_cancellation() {
    use zeppelin_embed::lifecycle::Deadline;
    let directory = Scratch::new();
    let store = Store::open(
        &directory.0,
        OpenOptions::new().with_max_resident_bytes(2 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let initial = shared.reserved_bytes().unwrap();
    {
        let memory = QueryMemory::new(&shared, 1024 * 1024).unwrap();
        let base = memory.reserved_bytes();
        let token = CancelToken::new();
        let control = QueryControl::Cancel(token.clone());
        let error = compile_in(
            "RETURN 1",
            &[],
            CompileLimits::default(),
            &memory,
            &control,
            |_| {
                token.cancel();
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Resource(ResourceError::Cancelled));
        assert_eq!(memory.reserved_bytes(), base);
        let control = QueryControl::Deadline(Deadline::after(std::time::Duration::ZERO).unwrap());
        let error = compile_in(
            "RETURN 1",
            &[],
            CompileLimits::default(),
            &memory,
            &control,
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Resource(ResourceError::Timeout));
        assert_eq!(memory.reserved_bytes(), base);
    }
    assert_eq!(shared.reserved_bytes().unwrap(), initial);
    store.close().unwrap();
}

#[path = "support/search.rs"]
mod search;

#[test]
fn ze76_public_failure_preserves_real_retrieval_and_publication() {
    use zeppelin_embed::property_graph::query::completed::{GraphQueryOptions, Value};
    use zeppelin_embed::property_graph::staging::{
        StructuredOperation, StructuredWrite, WriteImage,
    };
    use zeppelin_embed::property_graph::{
        ApplicationKey, CanonicalContents, EntityKind, GraphRevision,
    };
    use zeppelin_embed_cypher::execute;
    let fixture = search::SearchFixture::create();
    let store = fixture.store();
    let resources = store.graph_resources().unwrap();
    let query = "CALL ze.vector_search([0,0],3,'exact') YIELD node,distance RETURN distance";
    let check = || {
        let ranked = fixture.run(query);
        assert_eq!(ranked.metadata().rows, 3);
        for (row, expected) in [0.0_f64, 2.0, 50.0].into_iter().enumerate() {
            assert_eq!(ranked.cell(row, 0), Some(&Value::F64(expected.to_bits())));
        }
        let lexical = fixture.run("CALL ze.text_search('amber',2) YIELD node,score RETURN score");
        // The Store indexes four document rows, including the vector-only row.
        let idf = (1.0_f64 + (4.0 - 2.0 + 0.5) / (2.0 + 0.5)).ln();
        for (row, length) in [(0, 1.0), (1, 2.0)] {
            let expected = idf * 2.2 / (1.0 + 1.2 * (0.25 + 0.75 * length / (4.0 / 4.0)));
            assert_eq!(lexical.cell(row, 0), Some(&Value::F64(expected.to_bits())));
        }
    };
    check();
    let baseline = resources.reserved_bytes().unwrap();
    let generation = fixture.run(query).metadata().generation;
    let disk = || {
        let mut files: Vec<_> = std::fs::read_dir(&fixture.root)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), entry.metadata().unwrap().len())
            })
            .collect();
        files.sort();
        files
    };
    let before = disk();
    let options = GraphQueryOptions::default()
        .with_result_row_limit(1)
        .unwrap();
    assert!(
        execute(
            store,
            &search::control(),
            &options,
            query,
            &[],
            CompileLimits::default()
        )
        .is_err()
    );
    let token = CancelToken::new();
    token.cancel();
    assert!(
        execute(
            store,
            &QueryControl::Cancel(token),
            &GraphQueryOptions::default(),
            query,
            &[],
            CompileLimits::default()
        )
        .is_err()
    );
    let text = "amber ".repeat(900000);
    let first = CanonicalContents::node(&mut [], &mut [], Some(&text), None).unwrap();
    let second = CanonicalContents::node(&mut [], &mut [], Some(&text), None).unwrap();
    let writes = [(&first, "large-a"), (&second, "large-b")].map(|(image, key)| StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "ze76", key).unwrap(),
        revision: GraphRevision::new(1).unwrap(),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(image)),
    });
    assert!(
        store
            .graph_apply(&writes, &search::control())
            .unwrap_err()
            .nothing_committed()
    );
    assert_eq!(disk(), before);
    assert_eq!(fixture.run(query).metadata().generation, generation);
    check();
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
}
