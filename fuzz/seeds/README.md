# Checked-in fuzz regression seeds

`scripts/cargo-fuzz-nightly` supplies `fuzz/seeds/<target>/` as a read-only
input corpus after the ignored writable `fuzz/corpus/<target>/` directory.
Thus the documented `cargo fuzz run <target> -- ...` command always starts
from checked-in regressions without writing new mutations into this tree.

After a crash is fixed, copy its exact artifact bytes from
`fuzz/artifacts/<target>/` into `fuzz/seeds/<target>/`, retain the crash hash as
the filename, and verify that the promoted seed executes cleanly. Artifacts
remain ignored; promoted regression inputs do not.
