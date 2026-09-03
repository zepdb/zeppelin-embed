from embed_harness.encode import bracket


def a_cell_whose_bracket_drifts_outside_the_interval_is_marked_void():
    drifted = bracket(lambda: 10.0, lambda: 10.8, reference_p50_ms=10.0)
    acceptable = bracket(lambda: 10.0, lambda: 10.4, reference_p50_ms=10.0)

    assert drifted["void"] is True
    assert any("after" in note and "+/-5.0%" in note for note in drifted["notes"])
    assert acceptable["void"] is False
    assert acceptable["notes"] == []
