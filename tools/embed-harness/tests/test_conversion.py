import json
import shutil
import tempfile
from pathlib import Path

import numpy as np
from embed_harness.convert import convert_model
from embed_harness.encode import (
    MlxBertEncoder,
    NumpyBertEncoder,
    assert_embedding_match,
)
from tests.synthetic import (
    rewrite_safetensors_as_bfloat16,
    write_tiny_bert,
    write_tiny_modernbert,
)


def a_converted_model_reproduces_the_reference_embedding_within_tolerance():
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        converted = root / "converted"
        convert_model(
            write_tiny_bert(root),
            converted,
            model_id="example/tiny-bert",
            model_version="test-fixture-v1",
        )
        texts = ["t1 t2 t3", "t7 t8", "t12 t5 t1"]
        reference = NumpyBertEncoder(converted).encode_texts(texts)
        candidate = MlxBertEncoder(converted).encode_texts(texts)
        assert assert_embedding_match(reference, candidate) >= 0.999

        perturbed = root / "perturbed"
        perturbed.mkdir()
        shutil.copy2(converted / "meta.json", perturbed / "meta.json")
        shutil.copy2(converted / "tokenizer.json", perturbed / "tokenizer.json")
        archive = np.load(converted / "weights.npz", allow_pickle=False)
        weights = {name: archive[name].copy() for name in archive.files}
        word_name = next(
            name
            for name in weights
            if name.endswith("embeddings.word_embeddings.weight")
        )
        token_id = NumpyBertEncoder(converted).token_ids("t1")[0]
        weights[word_name][token_id] = np.linspace(
            -50.0, 50.0, weights[word_name].shape[1]
        )
        np.savez(perturbed / "weights.npz", **weights)
        broken = MlxBertEncoder(perturbed).encode_texts(texts)
        try:
            assert_embedding_match(reference, broken)
        except AssertionError as error:
            assert "below required" in str(error)
        else:
            raise AssertionError("the perturbed weight must trip the cosine guard")

        meta = json.loads((converted / "meta.json").read_text(encoding="utf-8"))
        assert meta["source_safetensors_sha256"] == meta["weights_digest"]


def a_conversion_records_the_explicit_model_identity_used_by_the_cell():
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        converted = root / "converted"
        convert_model(
            write_tiny_bert(root),
            converted,
            model_id="example/tiny-bert",
            model_version="0123456789abcdef",
        )

        meta = json.loads((converted / "meta.json").read_text(encoding="utf-8"))
        assert meta["model_id"] == "example/tiny-bert"
        assert meta["model_version"] == "0123456789abcdef"


def a_modernbert_conversion_preserves_its_runtime_contract():
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        converted = root / "converted"
        meta = convert_model(
            write_tiny_modernbert(root),
            converted,
            model_id="example/tiny-modernbert",
            model_version="test-fixture-v1",
        )

        assert meta["architecture"] == "ModernBertModel"
        assert meta["config"]["hidden_activation"] == "silu"
        assert meta["config"]["norm_eps"] == 1e-5
        assert meta["config"]["norm_bias"] is False
        assert meta["config"]["layer_types"] == [
            "full_attention",
            "sliding_attention",
        ]
        assert meta["config"]["rope_parameters"]["full_attention"][
            "rope_theta"
        ] == 160000.0
        assert meta["config"]["local_attention"] == 4


def a_bfloat16_model_converts_without_a_torch_dependency():
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        source = write_tiny_modernbert(root)
        rewrite_safetensors_as_bfloat16(source / "model.safetensors")

        convert_model(
            source,
            root / "converted",
            model_id="example/tiny-modernbert-bf16",
            model_version="test-fixture-v1",
        )

        archive = np.load(root / "converted" / "weights.npz", allow_pickle=False)
        assert archive["model.embeddings.norm.weight"].dtype == np.float32


def a_modernbert_numpy_forward_matches_the_transformers_reference():
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        converted = root / "converted"
        convert_model(
            write_tiny_modernbert(root),
            converted,
            model_id="example/tiny-modernbert",
            model_version="test-fixture-v1",
        )
        encoder = NumpyBertEncoder(converted)
        input_ids = np.array([[2, 3, 4, 0], [7, 8, 0, 0]], dtype=np.int64)
        attention = np.array([[1, 1, 1, 0], [1, 1, 0, 0]], dtype=np.float32)

        actual = encoder.forward(input_ids, attention)

        # Independently generated with Transformers ModernBertModel in eager mode.
        expected = np.array(
            [
                [
                    -0.213343605,
                    0.310000867,
                    0.579729974,
                    -0.045059472,
                    -0.475294054,
                    0.023945995,
                    -0.470106304,
                    0.269794464,
                ],
                [
                    -0.291339815,
                    -0.101200283,
                    0.313740075,
                    0.643883348,
                    -0.570385039,
                    -0.003256217,
                    0.185574159,
                    -0.179090902,
                ],
            ],
            dtype=np.float32,
        )
        np.testing.assert_allclose(actual, expected, rtol=1e-5, atol=1e-5)


def a_modernbert_mlx_forward_matches_the_transformers_reference():
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        converted = root / "converted"
        convert_model(
            write_tiny_modernbert(root),
            converted,
            model_id="example/tiny-modernbert",
            model_version="test-fixture-v1",
        )
        encoder = MlxBertEncoder(converted)
        input_ids = np.array([[2, 3, 4, 0], [7, 8, 0, 0]], dtype=np.int64)
        attention = np.array([[1, 1, 1, 0], [1, 1, 0, 0]], dtype=np.float32)

        actual = encoder.forward(input_ids, attention)

        expected = np.array(
            [
                [
                    -0.213343605,
                    0.310000867,
                    0.579729974,
                    -0.045059472,
                    -0.475294054,
                    0.023945995,
                    -0.470106304,
                    0.269794464,
                ],
                [
                    -0.291339815,
                    -0.101200283,
                    0.313740075,
                    0.643883348,
                    -0.570385039,
                    -0.003256217,
                    0.185574159,
                    -0.179090902,
                ],
            ],
            dtype=np.float32,
        )
        np.testing.assert_allclose(actual, expected, rtol=1e-5, atol=1e-5)
