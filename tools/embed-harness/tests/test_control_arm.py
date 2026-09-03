import json
import sys
import tempfile
from pathlib import Path

from embed_harness.convert import convert_model
from embed_harness.encode import bracket, main
from tests.synthetic import write_beir, write_tiny_bert


def a_cell_whose_bracket_drifts_outside_the_interval_is_marked_void():
    drifted = bracket(lambda: 10.0, lambda: 10.8, reference_p50_ms=10.0)
    acceptable = bracket(lambda: 10.0, lambda: 10.4, reference_p50_ms=10.0)

    assert drifted["void"] is True
    assert any("after" in note and "+/-5.0%" in note for note in drifted["notes"])
    assert acceptable["void"] is False
    assert acceptable["notes"] == []


def a_throughput_cell_is_void_when_its_control_bracket_drifts(monkeypatch):
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        model = root / "model"
        convert_model(
            write_tiny_bert(root),
            model,
            model_id="example/tiny-bert",
            model_version="test-fixture-v1",
        )
        queries = write_beir(root) / "queries.jsonl"
        lengths = root / "lengths.json"
        lengths.write_text(
            json.dumps({"histogram": {"4": 20}}), encoding="utf-8"
        )
        control = root / "control.json"
        control.write_text(
            json.dumps(
                {"reference_p50_ms": 1_000_000.0, "tokens": 4, "samples": 1}
            ),
            encoding="utf-8",
        )
        output = root / "throughput.json"
        monkeypatch.setattr(
            sys,
            "argv",
            [
                "embed-harness",
                "throughput",
                "--model",
                str(model),
                "--backend",
                "numpy",
                "--queries",
                str(queries),
                "--lengths",
                str(lengths),
                "--batch",
                "2",
                "--batches",
                "1",
                "--control",
                str(control),
                "--control-model",
                str(model),
                "--control-queries",
                str(queries),
                "--out",
                str(output),
            ],
        )

        main()

        result = json.loads(output.read_text(encoding="utf-8"))
        assert "2" in result
        assert result["void"] is True
        assert result["control"]["void"] is True
