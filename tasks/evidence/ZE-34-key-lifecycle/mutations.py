#!/usr/bin/env python3
"""Isolated behavioral mutants; restore exact original bytes even on failure.
Run from this ticket's worktree with all concurrent source builds stopped.
"""
import hashlib
import json
import os
from pathlib import Path
import subprocess

OUT = Path('/tmp/ze-34-evidence')
CORE = Path('crates/zeppelin-embed/src/property_graph/key_lifecycle.rs')
BOUNDED = CORE.parent / 'key_lifecycle/bounded.rs'
PROVENANCE = CORE.parent / 'provenance.rs'
ORACLE = Path('tests/adversarial-oracle/src/graph_key_lifecycle.rs')
PUBLIC = ['cargo', 'test', '-p', 'zeppelin-embed', '--test', 'graph_key_lifecycle']
RUNNER = ['cargo', 'test', '-p', 'zeppelin-embed-workspace-tests', '--test', 'adversarial_tests']
mutations = [
 ('old-incarnation', CORE, 'if matches!(request.expected, ExpectedGraphState::Entity(id) if id != fields.incarnation) {', 'if false && matches!(request.expected, ExpectedGraphState::Entity(id) if id != fields.incarnation) {', 'put_delete_recreate_preserve_fences_and_reject_old_incarnations', PUBLIC),
 ('hash-only-replay', CORE, '    if !bounded::shapes(left.shape, right.shape, checkpoint)? {', '    if left.fingerprint == right.fingerprint { return Ok(true); }\n    if !bounded::shapes(left.shape, right.shape, checkpoint)? {', 'exact_create_retry_preserves_original_outcome_and_rejects_equal_hash_drift', PUBLIC),
 ('resurrection', CORE, '        (true, _) => return Err(KeyLifecycleError::DeletedKey),', '        (true, _) => {},', 'every_key_transition_row_has_explicit_replay_conflict_and_change_outcomes', PUBLIC),
 ('deletion-precondition', CORE, 'if request.expected != ExpectedGraphState::Deletion(fields.installed_revision) {', 'if false && request.expected != ExpectedGraphState::Deletion(fields.installed_revision) {', 'every_key_transition_row_has_explicit_replay_conflict_and_change_outcomes', PUBLIC),
 ('delete-mode-replay', CORE, '                || request.delete_mode != fields.delete_mode', '                || (false && request.delete_mode != fields.delete_mode)', 'every_key_transition_row_has_explicit_replay_conflict_and_change_outcomes', PUBLIC),
 ('cypher-unchanged-shortcut', CORE, 'if contents_equal(current.contents, final_contents, scratch, checkpoint)? {', 'if true || contents_equal(current.contents, final_contents, scratch, checkpoint)? {', 'cypher_final_state_changes_advance_once_and_never_create_retry_receipts', PUBLIC),
 ('revision-overflow', CORE, '        .checked_next()\n        .map_err(|_| KeyLifecycleError::RevisionOverflow)?;', '        .checked_next()\n        .unwrap_or(fields.installed_revision);', 'unkeyed_cypher_entities_keep_identity_and_checked_revision_without_receipts', PUBLIC),
 ('duplicate-entity', CORE, '            if previous == Some(entity) {', '            if false && previous == Some(entity) {', 'structured_batches_reject_every_repeated_key_or_entity_target', PUBLIC),
 ('uncancellable-sort', CORE, '    bounded::sort(targets, checkpoint, |left, right, checkpoint| {\n        bounded::keys(left.key, right.key, checkpoint)\n    })?;', '    targets.sort_unstable_by_key(|target| target.key);', 'cancellation_interrupts_target_sort_before_its_first_comparison_mutates_input', PUBLIC),
 ('unchunked-name', BOUNDED, '        checkpoint()?;\n        let order = left.cmp(right);', '        let _ = &checkpoint;\n        let order = left.cmp(right);', 'cancellation_interrupts_long_key_comparison_before_reporting_key_mismatch', PUBLIC),
 ('discard-durable-work', CORE, 'if changed_items != 0 || other_durable_changes {', 'if changed_items != 0 || (false && other_durable_changes) {', 'batch_disposition_preserves_per_item_replay_generations_and_real_durable_changes', PUBLIC),
 ('generation-wrap', CORE, '                .checked_add(1)\n                .ok_or(KeyLifecycleError::GenerationOverflow)?,', '                .wrapping_add(1),', 'batch_disposition_preserves_per_item_replay_generations_and_real_durable_changes', PUBLIC),
 ('uncancellable-finalization', PROVENANCE, 'value.write_to(&mut io::sink(), checkpoint)?.bytes;', 'value.write_to(&mut io::sink(), &mut || Ok(()))?.bytes;', 'logical_finalization_checks_cancellation_while_streaming_long_key_provenance', PUBLIC),
 ('oracle-always-accept', ORACLE, '    if observed == expected {', '    if true || observed == expected {', 'adversarial::graph_key_lifecycle::lifecycle_oracle_can_fire_on_result_and_retained_field_corruption', RUNNER),
]
env = dict(os.environ, CARGO_TARGET_DIR='target/ze34')
results = []
for name, path, before, after, test, command in mutations:
    original = path.read_bytes()
    digest = hashlib.sha256(original).hexdigest()
    source = original.decode()
    assert source.count(before) == 1, (name, source.count(before))
    try:
        path.write_text(source.replace(before, after))
        args = command + [test, '--', '--exact', '--nocapture']
        log = OUT / ('mutant-' + name + '.log')
        with log.open('w') as output:
            result = subprocess.run(args, env=env, stdout=output, stderr=subprocess.STDOUT)
        data = log.read_text()
        intended = result.returncode == 101 and 'test result: FAILED.' in data and 'panicked at' in data and test in data
        results.append(dict(name=name, file=str(path), test=test, command=args, exit_code=result.returncode, intended_assertion_failure=intended, sha256_before=digest))
        assert intended, (name, data[-4000:])
    finally:
        path.write_bytes(original)
        restored = hashlib.sha256(path.read_bytes()).hexdigest()
        assert restored == digest
        if results and results[-1]['name'] == name:
            results[-1]['sha256_restored'] = restored
        (OUT / 'mutations.json').write_text(json.dumps(results, indent=2)+'\n')
    print(name + ': intended assertion RED; exact bytes restored', flush=True)
for suffix, command in [('public', PUBLIC), ('oracle', ['cargo', 'test', '-p', 'zeppelin-embed-adversarial-oracle', 'graph_key_lifecycle']), ('runner', RUNNER + ['lifecycle'])]:
    with (OUT / ('terminal-green-' + suffix + '.log')).open('w') as output:
        subprocess.run(command + ['--', '--nocapture'], env=env, stdout=output, stderr=subprocess.STDOUT, check=True)
print('terminal GREEN: restored public, oracle and runner suites', flush=True)
