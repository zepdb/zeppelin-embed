from embed_harness.run_cell import new_result, record_skipped, validate_result


def a_skipped_step_is_recorded_as_null_with_a_note_never_as_zero():
    result = new_result("synthetic-null-check")
    record_skipped(result, "hybrid_ndcg10", "hybrid evaluation was not requested")

    validate_result(result)

    assert result["hybrid_ndcg10"] is None
    assert result["hybrid_ndcg10"] != 0
    assert "hybrid_ndcg10: hybrid evaluation was not requested" in result["notes"]
    assert all(field in result for field in result["declared_fields"])
