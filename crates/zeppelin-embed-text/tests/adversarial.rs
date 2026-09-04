#![allow(clippy::expect_used)]

use tempfile::tempdir;
use zeppelin_embed_text::{
    IngestControl, IngestOptions, TextDocument, TextError, TextFaultSite, TextStore,
};

mod common;

#[test]
fn text_pipeline_fault_sites_fire_and_surface_typed_errors() {
    let directory = tempdir().expect("tempdir");
    let bundle_path = directory.path().join("fixture.zem");
    common::write_symmetric_fixture_bundle(&bundle_path);

    let cases = [
        (
            TextFaultSite::EmbedWorkerPanic,
            "embed worker",
            directory.path().join("embed-panic"),
        ),
        (
            TextFaultSite::ChannelClosedEarly,
            "tokenizer channel",
            directory.path().join("channel-close"),
        ),
        (
            TextFaultSite::SealFailureMidStream,
            "seal",
            directory.path().join("seal-failure"),
        ),
    ];

    for (site, expected_stage, store_path) in cases {
        let store =
            TextStore::open(store_path, &bundle_path, Default::default()).expect("open text store");
        let control = IngestControl::new().with_fault(site);
        let error = store
            .ingest_text_controlled(
                &[TextDocument::new(1, 1, "the bronze zeppelin")],
                IngestOptions {
                    embed_batch_size: 1,
                    seal_every: 1,
                    ..Default::default()
                },
                control.clone(),
            )
            .expect_err("armed text fault must fail the public ingest call");

        match (expected_stage, error) {
            ("seal", TextError::Seal(_)) => {}
            (stage, TextError::Pipeline { stage: actual, .. }) => assert_eq!(actual, stage),
            (_, other) => panic!("unexpected typed failure: {other}"),
        }
        assert!(control.fault_fired(site));
        println!("coverage {}=1", site.coverage_key());
    }
}
