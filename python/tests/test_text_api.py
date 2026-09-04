from pathlib import Path

import zeppelin_embed as ze


def test_ingest_text_and_query_text_match_the_rust_test_hit_ids_over_the_same_bundle(
    tmp_path: Path,
) -> None:
    bundle = Path("/private/tmp/ze-model-bundles-v2-c1/arctic-m-v2-symmetric.zem")
    assert bundle.is_file(), f"required baked test bundle is absent: {bundle}"

    with ze.open_text(tmp_path / "store", bundle) as store:
        report = store.ingest_text(
            [7, 9],
            ["the bronze zeppelin", "marine biology field notes"],
            revisions=[1, 1],
        )
        assert report.generation > 0
        result = store.query_text("the bronze zeppelin", k=1, legs="dense")

    assert [hit.doc_id for hit in result.hits] == [7]
    assert [hit.text for hit in result.hits] == ["the bronze zeppelin"]
