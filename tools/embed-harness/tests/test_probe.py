import tempfile
from pathlib import Path

import numpy as np
from embed_harness.convert import convert_model
from embed_harness.encode import NumpyBertEncoder
from embed_harness.evalir import load_beir
from embed_harness.probes import (
    encode_transformer_token_rows,
    shuffle_token_rows,
    word_order_probe,
)
from embed_harness.towers import build_q4_table, encode_q4
from tests.synthetic import write_beir, write_tiny_bert


def shuffling_query_tokens_leaves_a_mean_pooled_table_encoder_bitwise_unchanged():
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        converted = root / "converted"
        convert_model(
            write_tiny_bert(root),
            converted,
            model_id="example/tiny-bert",
            model_version="test-fixture-v1",
        )
        teacher = NumpyBertEncoder(converted)
        token_rows = [teacher.token_ids("t1 t2 t3"), teacher.token_ids("t4 t5 t6")]
        shuffled = shuffle_token_rows(token_rows)
        assert shuffled != token_rows

        table = build_q4_table(teacher)
        original_q4 = encode_q4(table, token_rows)
        shuffled_q4 = encode_q4(table, shuffled)
        assert np.array_equal(original_q4, shuffled_q4)
        assert 0.0 == float(np.max(np.abs(original_q4 - shuffled_q4)))

        corpus = load_beir(write_beir(root))
        documents = teacher.encode_documents(
            [document.text for document in corpus.documents]
        )
        corpus_rows = [teacher.token_ids(query.text) for query in corpus.queries]
        p1 = word_order_probe(
            lambda rows: encode_q4(table, rows),
            corpus_rows,
            documents,
            corpus.query_ids,
            corpus.document_ids,
            corpus.qrels,
        )
        assert p1["delta_ndcg10"] == 0.0

        original_q1 = encode_transformer_token_rows(teacher, token_rows)
        shuffled_q1 = encode_transformer_token_rows(teacher, shuffled)
        assert not np.array_equal(original_q1, shuffled_q1)
