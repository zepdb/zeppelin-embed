#!/usr/bin/env python3
"""Reject absent, empty, skipped or failing XCTest qualification suites."""
import re
import sys
from pathlib import Path


def validate_swift_tests(output, suite):
    summaries = re.findall(
        r"Test Suite '" + re.escape(suite) + r"' (?:passed|failed) at[^\n]*\n"
        r"\s*Executed (\d+) tests?, with (?:(\d+) tests? skipped and )?(\d+) failures?",
        output)
    if len(summaries) != 1:
        raise ValueError(suite + ': missing or ambiguous XCTest execution summary')
    executed, skipped, failures = (int(value or 0) for value in summaries[0])
    if executed == 0 or skipped or failures:
        raise ValueError(f'{suite}: executed={executed}, skipped={skipped}, failures={failures}; qualification refused')
    print(f'{suite}: executed={executed}, skipped={skipped}, failures={failures}')


if __name__ == '__main__':
    try:
        validate_swift_tests(Path(sys.argv[1]).read_text(), sys.argv[2])
    except ValueError as error:
        sys.exit(str(error))
