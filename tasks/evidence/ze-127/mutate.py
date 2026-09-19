#!/usr/bin/env python3
"""Run bounded ZE-127 source controls; always restore exact source bytes."""
import hashlib
import json
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[3]
OUT = pathlib.Path('/tmp/ze-127-evidence')
PREFIX = 'crates/zeppelin-embed/src/property_graph/query/'
CASES = [
    ('foreign-token', 'completed.rs', 'if !std::ptr::eq(input.view, context.view()) {', 'if false && !std::ptr::eq(input.view, context.view()) {', 'producer_errors_and_identical_metadata_foreign_tokens_remain_distinct', 'assertion failed'),
    ('represented-cap', 'completed.rs', 'if represented > 4 * 1024 * 1024 {', 'if represented > 8 * 1024 * 1024 {', 'expanded_descendants_and_complete_representation_caps_cannot_be_bypassed', 'root descriptors count'),
    ('expanded-bag-count', 'completed/validate.rs', '.checked_add(shape.descendants)', '.checked_add(0)', 'expanded_descendants_and_complete_representation_caps_cannot_be_bypassed', 'repeated DAG children count'),
    ('guard-before-buffer', 'resources.rs', "    values: Vec<T>,\n    charge: QueryReservation<'m, 'g>,", "    charge: QueryReservation<'m, 'g>,\n    values: Vec<T>,", 'every_actual_allocation_failure_releases_real_buffers_before_guards', 'clean.early_release'),
    ('lost-byte-owner', 'completed.rs', 'bytes: self.bytes.detach_owned(),', 'bytes: Vec::new(),', 'consuming_detach_is_allocator_denied_and_preserves_every_live_pointer', 'assertion `left == right` failed'),
    ('allocating-detach', 'completed.rs', 'bytes: self.bytes.detach_owned(),', 'bytes: self.bytes.as_slice().to_vec(),', 'consuming_detach_is_allocator_denied_and_preserves_every_live_pointer', 'memory allocation of 3 bytes failed'),
    ('allocating-vector-detach', 'completed.rs', 'vectors: self.vectors.detach_owned(),', 'vectors: self.vectors.as_slice().to_vec(),', 'every_typed_pool_uses_real_fallible_allocation_and_exact_cleanup', 'memory allocation of 4 bytes failed'),
    ('missing-copy-charge', 'completed.rs', 'context.charge(WorkKind::CopiedBytes, std::mem::size_of_val(part) as u64)?;', 'context.check_work(WorkKind::CopiedBytes, std::mem::size_of_val(part) as u64)?;', 'copy_limit_records_only_performed_chunks_and_returns_no_partial_owner', 'assertion failed'),
]
results = []
for name, relative, old, new, test, expected in CASES:
    if len(sys.argv) > 1 and name not in sys.argv[1:]:
        continue
    path = ROOT / PREFIX / relative
    original = path.read_bytes()
    before = hashlib.sha256(original).hexdigest()
    assert original.count(old.encode()) == 1, (name, 'ambiguous mutation')
    command = ['cargo', 'nextest', 'run', '-p', 'zeppelin-embed', '--test', 'graph_completed_results', '-E', 'test(=' + test + ')', '--test-threads', '1', '--retries', '0']
    # nextest exact names include the module path for allocator tests.
    command[command.index('-E') + 1] = 'test(' + test + ')'
    try:
        path.write_bytes(original.replace(old.encode(), new.encode()))
        run = subprocess.run(command, cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        output = run.stdout.decode(errors='replace')
        (OUT / ('mutant-' + name + '.log')).write_bytes(run.stdout)
    finally:
        path.write_bytes(original)
    restored = hashlib.sha256(path.read_bytes()).hexdigest()
    entry = dict(name=name, path=PREFIX+relative, source_sha256=before, restored_sha256=restored,
                 command=command, exit=run.returncode, expected=expected, fired=run.returncode==100 and expected in output)
    results.append(entry)
    (OUT / ('mutations-' + '-'.join(sys.argv[1:]) + '.json' if len(sys.argv) > 1 else 'mutations.json')).write_text(json.dumps(results, indent=2) + '\n')
    print(json.dumps(entry), flush=True)
    assert before == restored and entry['fired'], (name, output[-4000:])
