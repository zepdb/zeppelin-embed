# ZE-146 bounded lexical producer evidence

Source pin: `7fd76c761b099e7d1efaf5e3e53fde709764708f`
Approved plan SHA-256: `6a79020547186e13145f12a161e09f766c1f5fae37a4a4c44e537676b99761a3`

## RED and compile milestones

- First semantic RED: `cargo nextest run -p zeppelin-embed --features graph-cypher --test graph_lexical_prepare -j 4 --retries 0` reached the literal analyzed-empty assertion and failed with `Some(0)` versus `None`.
- The allocation policy and guarded owners compiled before tokenizer/codec wiring.
- The shared controlled tokenizer reached its 64-test milestone without an uncontrolled analyzer fallback.
- Deliberate ownership mutation: temporarily replacing the builder descriptor reservation with a zero-byte reservation made `lexical_capacity_is_owned_through_finish_decode_and_drop` fail at the independent raw-capacity assertion with `1211` reserved versus `1467` actual. The source was restored byte-for-byte before the terminal GREEN.

## Terminal GREEN

- `cargo nextest run -p zeppelin-embed --features graph-cypher --test graph_lexical_prepare -j 4 --retries 0` — 5 passed. Together with the two private groups below, this covers all seven prescribed groups.
- `cargo nextest run -p zeppelin-embed --features graph-cypher --lib -E 'test(fts::graph_build::tests::) | (test(fts::tokenizer::) & !test(fts::tokenizer::properties::)) | test(fts::index::tests::rows_are_dense_and_ascending) | test(fts::index::tests::document_length_counts_positions_not_tokens) | test(fts::index::tests::posting_lists_carry_positions) | test(fts::sealed::tests::astra_18_) | test(fts::control::tests::astra_18_)' -j 4 --retries 0` — 75 passed.
- `cargo nextest run -p zeppelin-embed --test tokenizer_conformance --test fts_format_golden --test postings_region_golden -j 4 --retries 0` — 20 passed, 2 skipped by their existing configuration.
- `cargo check -p zeppelin-embed --lib --no-default-features` — passed.
- `cargo check -p zeppelin-embed --lib --no-default-features --features graph-cypher` — passed.
- `cargo check -p zeppelin-embed --lib --no-default-features --features graph-cypher,allocation-audit` — passed.
- `cargo fmt --all -- --check` and `git diff --check` — passed after formatting only the ZE-146 allowlist.

The capacity group sums raw `Vec` and `String` capacities plus owner descriptors, restores exact baselines after drop, observes exact old-plus-replacement peak during growth, and proves storage and query allowance refusal before required growth. The long-work group requests authentic cancellation after positive probes in analysis, sorting, position encode/decode, and region encode/decode; it also covers tight checked work, typed expired deadline, cleanup, and successful controls on the same inputs.

The query golden charges `LexicalBlocks = 6`, `LexicalPostings = 4`, and `CopiedBytes = 349`, rejects zero block/posting limits and tight query memory, and preserves close-before-caller-cancel precedence. Corruption checks cover reserved header byte 20, embedded postings-per-block bytes 6..8, truncation, duplicate position delta at frozen offset 157, and a non-default legacy field shape. The producer also seals and decodes an empty fragment. Controlled tokenizer evidence covers all ten frozen streams, configured vocabulary, the `B#\u{0363}` dedup survivor, and the raw Roman numeral numeric flag rule.

## Qualification boundary

This is the graph-gated preparation/decoder seam only. Bare ZFTS region bytes do not authenticate the supplied tokenizer epoch. ZE-61 still owns NodeId/revision mappings, sparse masks, interpretation/root binding, publication, admission, compaction, and reopen integration. The deferred `fts::tokenizer::properties` campaign and full/adversarial/coverage/fuzz/performance/soak/size/release suites remain ZE-118 work and were not run.
