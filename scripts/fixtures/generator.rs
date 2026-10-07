#![allow(clippy::expect_used, clippy::panic)]
mod common;
use std::path::Path;
use zeppelin_embed::ingest::{DeleteBatch, DocId};
use zeppelin_embed::lifecycle::{
    Store,
    durability::{CommitTier, DurabilityMode},
};
fn seed(path: &Path) {
    let store = Store::open(
        path,
        common::options(false)
            .with_schema(common::schema())
            .with_durability(DurabilityMode::Durable, CommitTier::Durable),
    )
    .expect("create release store");
    store
        .ingest(common::batch(vec![
            common::document(1, "orchard apple"),
            common::document(2, "harbor pear"),
            common::document(3, "deleted orchard"),
        ]))
        .expect("sealed documents");
    store.seal().expect("seal");
    store
        .delete(DeleteBatch::new(vec![DocId::new(3)]))
        .expect("tombstone");
    store
        .ingest(common::batch(vec![common::document(4, "orchard harbor")]))
        .expect("unsealed tail");
    // No Drop or close: the process exits after all durable acknowledgements.
    std::mem::forget(store);
}
fn oracle(path: &Path, out: &Path) {
    let store = Store::open(path, common::options(true)).expect("release reader");
    let mut json = String::from(
        "{\n  \"live_ids\": [1, 2, 4],\n  \"document_1\": \"orchard apple\",\n  \"document_2\": \"harbor pear\",\n  \"document_4\": \"orchard harbor\",\n  \"tombstoned_ids\": [3],\n  \"rank_column\": 1,\n  \"vector_input\": [1, 1, 2, 3, 4, 5, 6, 7],\n",
    );
    for query in ["orchard", "harbor", "deleted"] {
        json.push_str(&format!(
            "  \"query_{query}\": {:?},\n",
            common::text_hits(&store, query)
        ));
    }
    json.push_str(&format!(
        "  \"query_vector\": {:?},\n  \"generation\": {}\n}}\n",
        common::vector_hits(&store),
        store
            .count_documents(None, None)
            .expect("generation")
            .generation
    ));
    std::fs::write(out.join("expected.json"), json).expect("oracle");
    store.close().expect("read-only close");
}
#[cfg(release_namespace)]
fn namespaces(out: &Path) {
    for name in ["a", "b"] {
        seed(&out.join(name));
    }
    // Seeding leaked handles requires a separate process before coordinator admission.
}
fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().expect("mode");
    let out = std::path::PathBuf::from(args.next().expect("output"));
    match mode.as_str() {
        // Every release, including v0.6.0, has a plain Store fixture.
        // Namespace modes produce the separate ZE-370 diagnostic fixture only.
        "seed" => seed(&out),
        "oracle" => oracle(&out, &out),
        #[cfg(release_namespace)]
        "namespace-seed" => namespaces(&out),
        #[cfg(release_namespace)]
        "namespace-commit" => {
            use zeppelin_embed::lifecycle::{NamespaceMutation, namespace_batch};
            namespace_batch(
                &out,
                ["a", "b"]
                    .into_iter()
                    .map(|name| NamespaceMutation {
                        name: name.into(),
                        options: common::options(false)
                            .with_durability(DurabilityMode::Durable, CommitTier::Durable),
                        upserts: vec![common::document_revision(4, 2, "orchard harbor")],
                        deletes: vec![],
                        delete_where: None,
                    })
                    .collect(),
            )
            .expect("public committed prepared batch");
            oracle(&out.join("a"), &out);
        }
        #[cfg(release_namespace)]
        "namespace-cascade" => {
            use zeppelin_embed::lifecycle::{NamespaceMutation, namespace_delete_cascade};
            let generations = namespace_delete_cascade(
                &out,
                ["a", "b"]
                    .into_iter()
                    .map(|name| NamespaceMutation {
                        name: name.into(),
                        options: common::options(false)
                            .with_durability(DurabilityMode::Durable, CommitTier::Durable),
                        upserts: vec![],
                        deletes: vec![DocId::new(2)],
                        delete_where: None,
                    })
                    .collect(),
            )
            .expect("release routed cascade");
            println!("{generations:?}");
        }
        _ => panic!("unknown mode"),
    }
}
