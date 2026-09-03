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
from tests.synthetic import write_tiny_bert


def a_converted_model_reproduces_the_reference_embedding_within_tolerance():
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        converted = root / "converted"
        convert_model(write_tiny_bert(root), converted)
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
