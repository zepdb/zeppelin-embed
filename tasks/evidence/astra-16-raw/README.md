Raw test logs are committed as lossless gzip archives. The original `.log`
files remain in this checkout. Use `gzip -dk FILE.log.gz` in a fresh checkout
to restore the exact bytes referenced by `final-checks.json` and the report.
No log content was trimmed or changed.
