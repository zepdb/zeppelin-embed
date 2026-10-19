"""Text integration tests use the committed synthetic model, never a local bake."""

import hashlib
import json
import threading
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

import zeppelin_embed as ze

BUNDLE = (
    Path(__file__).resolve().parents[3]
    / "crates/zeppelin-embed-text/tests/fixtures/ze115/gte.zem"
)
REVISION = "0123456789abcdef0123456789abcdef01234567"


@pytest.mark.parametrize("legs", ["dense", "lexical", "hybrid"])
def test_text_query_reingest_delete_and_health(tmp_path: Path, legs: str) -> None:
    with ze.open_text(tmp_path / "store", BUNDLE) as store:
        report = store.ingest_text(
            [7, 9], ["the bronze zeppelin", "marine biology field notes"]
        )
        assert report.documents == 2
        assert report.all_threads_joined
        result = store.query_text("the bronze zeppelin", k=1, legs=legs, tier="exact")
        assert [hit.doc_id for hit in result.hits] == [7]
        assert result.hits[0].text == "the bronze zeppelin"
        assert result.diagnostics is not None
        assert (
            result.diagnostics.mode
            == ze.QueryMode["VECTOR" if legs == "dense" else legs.upper()]
        )
        assert result.embedding_calls == (0 if legs == "lexical" else 1)
        chunked = store.ingest_text(
            [7],
            ["the bronze zeppelin zeppelin zeppelin zeppelin"],
            revisions=[2],
            chunk_max=2,
        )
        assert chunked.chunks > 1
        store.ingest_text([7], ["bronze"], revisions=[3])
        hits = store.query_text("bronze zeppelin", legs=legs, k=10).hits
        assert [(hit.revision, hit.text) for hit in hits if hit.doc_id == 7] == [
            (3, "bronze")
        ]
        generation = store.delete_text([7]).generation
        assert all(
            hit.doc_id != 7 for hit in store.query_text("bronze", legs=legs).hits
        )
        health = store.health()
        assert health.generation == generation
        assert health.unresolved_fault_count == 0
        assert 0 <= health.graph_coverage <= 1
        assert (
            store.maintain(wall_time_ns=1_000_000, bytes=1_000_000).status
            in ze.MaintenanceStatus
        )


@pytest.mark.parametrize("legs", ["lexical", "hybrid"])
def test_empty_and_deleted_text_store_returns_zero_hits(
    tmp_path: Path, legs: str
) -> None:
    with ze.open_text(tmp_path / "store", BUNDLE) as store:
        assert store.query_text("bronze zeppelin", legs=legs).hits == ()
        store.ingest_text([7], ["bronze zeppelin"])
        store.delete_text([7])
        assert store.query_text("bronze zeppelin", legs=legs).hits == ()


def test_text_cancel_and_deadline(tmp_path: Path) -> None:
    with ze.open_text(tmp_path / "store", BUNDLE) as store:
        store.ingest_text([7], ["bronze zeppelin"])
        with ze.CancelToken() as token:
            token.cancel()
            with pytest.raises(ze.Cancelled):
                store.query_text("bronze", cancel_token=token)
            with pytest.raises(ze.Cancelled):
                store.ingest_text([9], ["marine biology"], cancel_token=token)
        with pytest.raises(ze.Timeout):
            store.query_text("bronze", deadline_ns=1)


def test_open_model_download_progress_cache_and_offline(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    remote = tmp_path / "remote" / "fixture/tiny/resolve" / REVISION
    remote.mkdir(parents=True)
    body = BUNDLE.read_bytes()
    (remote / "tiny.zem").write_bytes(body)
    manifest = json.dumps(
        {
            "format": 1,
            "name": "tiny",
            "bundle": "tiny.zem",
            "query_coreml": None,
            "files": [
                {
                    "path": "tiny.zem",
                    "size": len(body),
                    "sha256": hashlib.sha256(body).hexdigest(),
                }
            ],
        }
    ).encode()
    (remote / "zeppelin-model.json").write_bytes(manifest)
    server = ThreadingHTTPServer(
        ("127.0.0.1", 0),
        partial(SimpleHTTPRequestHandler, directory=str(tmp_path / "remote")),
    )
    worker = threading.Thread(target=server.serve_forever)
    worker.start()
    monkeypatch.setenv("ZE_MODEL_ENDPOINT", f"http://127.0.0.1:{server.server_port}")
    monkeypatch.setenv("ZE_MODEL_ALLOW_HTTP", "1")
    options = dict(
        repo="fixture/tiny",
        revision=REVISION,
        manifest_sha256=hashlib.sha256(manifest).hexdigest(),
        cache_dir=tmp_path / "cache",
    )
    progress: list[tuple[int, int]] = []
    try:
        with ze.open_model(
            tmp_path / "store",
            **options,
            progress=lambda done, total: progress.append((done, total)),
        ) as store:
            store.ingest_text([7], ["bronze zeppelin"])
            assert store.query_text("bronze", legs="lexical").hits[0].doc_id == 7

        def failing_progress(done: int, total: int) -> None:
            raise RuntimeError("progress failed")

        failed_options = options | {"cache_dir": tmp_path / "callback-cache"}
        with pytest.raises(RuntimeError, match="progress failed"):
            ze.open_model(
                tmp_path / "callback-store", **failed_options, progress=failing_progress
            )
        with ze.open_model(tmp_path / "callback-store", **failed_options, offline=True):
            pass  # The failed callback did not leak the newly opened writer.
    finally:
        server.shutdown()
        server.server_close()
        worker.join()
    assert progress[0] == (0, 0)
    assert progress[-1] == (len(body), len(body))
    # Server is gone: success now requires a verified cache hit.
    with ze.open_model(tmp_path / "store", **options, offline=True) as store:
        assert store.query_text("bronze", legs="hybrid").hits[0].doc_id == 7


def test_open_model_offline_miss(tmp_path: Path) -> None:
    with pytest.raises(ze.NotFound, match="offline"):
        ze.open_model(
            tmp_path / "store",
            repo="fixture/tiny",
            revision=REVISION,
            manifest_sha256="0" * 64,
            cache_dir=tmp_path / "cache",
            offline=True,
        )
