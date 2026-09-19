#!/usr/bin/env python3
"""Narrow reversible acceptance mutations; run only in the ZE55 worktree."""
from pathlib import Path
import hashlib
import json
import subprocess
import sys

ROOT = Path.cwd()
OUT = Path('/tmp/ze-55-mutants')
OUT.mkdir(exist_ok=True)
CASES = [
    ('source-error-span', 'crates/zeppelin-embed-cypher/src/binding.rs',
     'ErrorKind::UnknownVariable,\n            span,', 'ErrorKind::UnknownVariable,\n            Span::default(),',
     ['-p','zeppelin-embed-cypher','--test','binding','compile_errors_keep_exact_utf8_source_spans_and_never_consume']),
    ('scope-lookup', 'crates/zeppelin-embed-cypher/src/binding.rs',
     'if symbol.name == name {', 'if true {',
     ['-p','zeppelin-embed-cypher','--test','binding','scalar_scope_parameters_and_operator_types_bind_before_consumption']),
    ('duplicate-output', 'crates/zeppelin-embed-cypher/src/binding/projection.rs',
     'if prior.name == name {', 'if false {',
     ['-p','zeppelin-embed-cypher','--test','binding','projections_expand_scope_and_preserve_grouped_order_references']),
    ('unreserved-source-copy', 'crates/zeppelin-embed-cypher/src/resources.rs',
     'charge(resources, text.len(), span)?;', 'charge(resources, 0, span)?;',
     ['-p','zeppelin-embed-cypher','--test','compiler_allocation','compiler_reserves_real_growth_overlap_before_allocator_calls']),
    ('permissive-oracle', 'tests/adversarial-oracle/src/graph_binding.rs',
     'if observed != &expected {', 'if false {',
     ['-p','zeppelin-embed-adversarial-oracle','graph_binding::tests::binding_oracle_rejects_wrong_order_types_bits_modes_and_admission']),
]
rows=json.loads((OUT/'results.json').read_text()) if len(sys.argv)>1 and (OUT/'results.json').exists() else []
if len(sys.argv)>1: CASES=[case for case in CASES if case[0] in sys.argv[1:]]
for name, relative, needle, change, args in CASES:
    path=ROOT/relative
    original=path.read_bytes()
    before=hashlib.sha256(original).hexdigest()
    text=original.decode()
    if text.count(needle)!=1: raise RuntimeError(f'ambiguous mutation {name}')
    try:
        path.write_text(text.replace(needle,change))
        command=['cargo','nextest','run']+args
        with (OUT/f'{name}-red.log').open('wb') as log:
            result=subprocess.run(command,stdout=log,stderr=subprocess.STDOUT)
        if result.returncode!=100: raise RuntimeError(f'{name} did not expose assertion failure: {result.returncode}')
    finally:
        path.write_bytes(original)
    assert hashlib.sha256(path.read_bytes()).hexdigest()==before
    with (OUT/f'{name}-green.log').open('wb') as log:
        restored=subprocess.run(command,stdout=log,stderr=subprocess.STDOUT)
    if restored.returncode: raise RuntimeError(f'{name} restoration failed')
    rows=[row for row in rows if row['name']!=name]
    rows.append({'name':name,'source':relative,'sha256_restored':before,'command':command,'red_exit':result.returncode,'green_exit':restored.returncode})
    print(name,'RED100/GREEN0 exact source restored',flush=True)
(OUT/'results.json').write_text(json.dumps(rows,indent=2)+'\n')
