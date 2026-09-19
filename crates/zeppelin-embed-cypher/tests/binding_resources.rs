#![allow(clippy::unwrap_used, clippy::expect_used)]
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
        let path =
            std::env::temp_dir().join(format!("ze-55-binding-resources-{}", std::process::id()));
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
