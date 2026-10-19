"""Explicit CoreML compute-plan inspection through the text C ABI."""

import platform
import subprocess
from pathlib import Path

import pytest

import zeppelin_embed as ze

ROOT = Path(__file__).resolve().parents[3]
FIXTURES = ROOT / "crates/zeppelin-embed-text/tests/fixtures/ze115"
requires_text = pytest.mark.skipif(not ze.TEXT_AVAILABLE, reason="text API unavailable")


@requires_text
def test_text_observed_compute_units_loads_tiny_coreml_plan(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    version = tuple(int(part) for part in platform.mac_ver()[0].split(".")[:2])
    if version < (14, 4):
        pytest.skip("MLComputePlan requires macOS 14.4 or later")
    subprocess.run(
        ["xcrun", "coremlcompiler", "compile", str(FIXTURES / "tiny.mlmodel"), str(tmp_path)],
        check=True,
        capture_output=True,
        text=True,
    )
    model = tmp_path / "tiny.mlmodelc"
    assert model.is_dir()
    monkeypatch.setenv("ZE_QUERY_COREML", str(model))
    bundle = ROOT / "crates/zeppelin-embed-ffi/tests/fixtures/compute_units.zem"
    with ze.open_text(tmp_path / "store", bundle) as store:
        counts = store.observed_compute_units()
        assert isinstance(counts, ze.ObservedComputeUnits)
        assert sum((counts.cpu_ops, counts.gpu_ops, counts.neural_engine_ops, counts.other_ops)) == 1


@requires_text
def test_text_observed_compute_units_is_absent_without_coreml(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("ZE_QUERY_COREML", "off")
    with ze.open_text(tmp_path / "store", FIXTURES / "gte.zem") as store:
        assert store.observed_compute_units() is None
