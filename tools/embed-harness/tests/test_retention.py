import tempfile
from pathlib import Path

import numpy as np
from embed_harness.convert import convert_model
from embed_harness.encode import NumpyBertEncoder
from embed_harness.evalir import dense_retrieval, load_beir, mean_ndcg_at_k, ndcg_at_k
from embed_harness.towers import build_q4_table, encode_q4
from tests.synthetic import write_beir, write_tiny_bert


def a_deliberately_corrupted_table_drops_retention_below_the_q4_gate():
    worked = ndcg_at_k(["d2", "d1", "d3"], {"d1": 1, "d2": 2, "d3": 0}, 10)
    assert worked == 1.0, "(2/log2(2) + 1/log2(3)) divided by itself is 1"

    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        converted = root / "converted"
        convert_model(write_tiny_bert(root), converted)
        corpus = load_beir(write_beir(root))
        teacher = NumpyBertEncoder(converted)
        table = build_q4_table(teacher)

        document_vectors = teacher.encode_documents(
            [doc.text for doc in corpus.documents]
        )
        q1_vectors = teacher.encode_texts([query.text for query in corpus.queries])
        q4_vectors = encode_q4(
            table,
            [teacher.token_ids(query.text) for query in corpus.queries],
        )
        q1_run = dense_retrieval(
            q1_vectors, document_vectors, corpus.query_ids, corpus.document_ids, k=10
        )
        q4_run = dense_retrieval(
            q4_vectors, document_vectors, corpus.query_ids, corpus.document_ids, k=10
        )
        q1_ndcg = mean_ndcg_at_k(q1_run, corpus.qrels, 10)
        q4_ndcg = mean_ndcg_at_k(q4_run, corpus.qrels, 10)
        assert q4_ndcg / q1_ndcg >= 0.90

        corrupted = table.copy()
        corrupted[: corrupted.shape[0] // 2] = np.float32(0.0)
        corrupted_vectors = encode_q4(
            corrupted,
            [teacher.token_ids(query.text) for query in corpus.queries],
        )
        corrupted_run = dense_retrieval(
            corrupted_vectors,
            document_vectors,
            corpus.query_ids,
            corpus.document_ids,
            k=10,
        )
        corrupted_retention = mean_ndcg_at_k(corrupted_run, corpus.qrels, 10) / q1_ndcg
        assert corrupted_retention < 0.90
