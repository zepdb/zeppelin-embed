import tempfile
from pathlib import Path

from embed_harness.convert import convert_model
from embed_harness.encode import NumpyBertEncoder, compute_length_distribution
from tests.synthetic import write_beir, write_tiny_bert


def a_length_distribution_combines_each_declared_corpus():
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        converted = root / "converted"
        convert_model(
            write_tiny_bert(root),
            converted,
            model_id="example/tiny-bert",
            model_version="test-fixture-v1",
            document_prefix="passage: ",
        )
        corpus = write_beir(root) / "corpus.jsonl"
        second = root / "second-corpus.jsonl"
        second.write_text(corpus.read_text(encoding="utf-8"), encoding="utf-8")

        result = compute_length_distribution(
            [corpus, second],
            NumpyBertEncoder(converted),
            root / "lengths.json",
        )

        assert result["documents"] == 40
        assert len(result["sources"]) == 2
        assert all(source["sha256"] for source in result["sources"])
        assert result["document_prefix"] == "passage: "
