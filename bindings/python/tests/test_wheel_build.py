"""Exercise setuptools' actual copy step with Cargo build outputs."""

import json
import os
import runpy
from pathlib import Path
from subprocess import CompletedProcess

import pytest
import setuptools


@pytest.mark.parametrize(
    "system,machine,text",
    [
        ("darwin", "arm64", True),
        ("darwin", "x86_64", False),
        ("linux", "aarch64", False),
    ],
)
def test_wheel_build_features_and_runtime_assets(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    system: str,
    machine: str,
    text: bool,
) -> None:
    monkeypatch.setattr(os, "environ", os.environ.copy())
    monkeypatch.delenv("MACOSX_DEPLOYMENT_TARGET", raising=False)
    monkeypatch.setattr(setuptools, "setup", lambda **kwargs: None)
    setup = runpy.run_path(str(Path(__file__).parents[1] / "setup.py"))
    monkeypatch.setattr(setup["sys"], "platform", system)
    monkeypatch.setattr(setup["platform_module"], "machine", lambda: machine)
    target = tmp_path / "target"
    monkeypatch.setenv("CARGO_TARGET_DIR", str(target))
    native = setup["_native_filename"]()
    (target / "release").mkdir(parents=True)
    (target / "release" / native).write_bytes(b"native")
    out = tmp_path / "mlx-output"
    (out / "build/lib").mkdir(parents=True)
    (out / "build/lib/mlx.metallib").write_bytes(b"metal")
    command: list[str] = []
    cargo_environment: dict[str, str] = {}

    def cargo(args: list[str], **kwargs: object) -> CompletedProcess[str]:
        command.extend(args)
        cargo_environment.update(kwargs.get("env", {}))
        return CompletedProcess(
            args,
            0,
            stdout=json.dumps(
                {
                    "reason": "build-script-executed",
                    "package_id": "registry+https://github.com/rust-lang/crates.io-index#mlx-sys@0.2.0",
                    "out_dir": str(out),
                }
            )
            + "\n",
        )

    monkeypatch.setattr(setup["subprocess"], "run", cargo)
    monkeypatch.setattr(setup["build_py"], "run", lambda self: None)
    builder = setup["BuildPyWithNative"](setuptools.Distribution())
    builder.ensure_finalized()
    builder.build_lib = str(tmp_path / "wheel")
    builder.run()
    assert ("--features" in command) == text
    if text:
        assert command[command.index("--features") + 1] == "text"
        assert cargo_environment["MACOSX_DEPLOYMENT_TARGET"] == "14.0"
        assert os.environ["MACOSX_DEPLOYMENT_TARGET"] == "14.0"
        monkeypatch.setattr(
            setup["bdist_wheel"],
            "get_tag",
            lambda self: ("cp311", "cp311", "macosx_26_0_arm64"),
        )
        wheel = setup["PlatformWheel"](setuptools.Distribution())
        assert wheel.get_tag() == ("py3", "none", "macosx_14_0_arm64")
    assets = Path(builder.build_lib) / "zeppelin_embed/.dylibs"
    assert (assets / native).read_bytes() == b"native"
    assert (assets / "mlx.metallib").exists() == text
    if text:
        assert str(assets / "mlx.metallib") in builder.get_outputs()


def test_text_wheel_rejects_deployment_target_below_14(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(setuptools, "setup", lambda **kwargs: None)
    setup = runpy.run_path(str(Path(__file__).parents[1] / "setup.py"))
    monkeypatch.setattr(setup["sys"], "platform", "darwin")
    monkeypatch.setattr(setup["platform_module"], "machine", lambda: "arm64")
    monkeypatch.setenv("MACOSX_DEPLOYMENT_TARGET", "13.0")
    monkeypatch.setattr(
        setup["bdist_wheel"],
        "get_tag",
        lambda self: ("cp311", "cp311", "macosx_26_0_arm64"),
    )
    wheel = setup["PlatformWheel"](setuptools.Distribution())
    with pytest.raises(RuntimeError, match="text.*14.0"):
        wheel.get_tag()
    assert os.environ["MACOSX_DEPLOYMENT_TARGET"] == "13.0"
