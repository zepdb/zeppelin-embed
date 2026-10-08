mod support;

mod store_graph {
    use super::support;
    use zeppelin_embed::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
    use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
    use zeppelin_embed::property_graph::query::completed::GraphQueryOptions;
    use zeppelin_embed_cypher::{CompileLimits, StoreCypherExt};

    #[test]
    #[allow(clippy::expect_used, clippy::panic)]
    fn a_document_ingest_is_visible_to_cypher() {
        let dir = support::unique_temp_dir("ze359-document-cypher");
        let store = Store::open(&dir, OpenOptions::new().with_max_resident_bytes(256 << 20))
            .expect("open Store");
        store
            .ingest(IngestBatch::new(vec![
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(91), Revision::new(1)),
                    vec![1.0, 0.0],
                )
                .with_text("document text"),
            ]))
            .expect("ingest document");
        store
            .enable_graph()
            .expect("enable graph on the same Store");
        let control = QueryControl::Cancel(CancelToken::new());
        let result = store
            .cypher(
                &control,
                &GraphQueryOptions::default(),
                "MATCH (n:Document) RETURN n",
                &[],
                CompileLimits::default(),
            )
            .expect("read document through Cypher");
        assert_eq!(result.metadata().rows, 1);
        assert_eq!(
            result.pools().nodes.first().expect("document node").id,
            zeppelin_embed::property_graph::NodeId::from(DocId::new(91))
        );
        store.close().expect("close");
        std::fs::remove_dir_all(dir).expect("remove fixture");
    }
}
