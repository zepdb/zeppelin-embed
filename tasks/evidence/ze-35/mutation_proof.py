"""Run isolated behavioral mutations; always restore exact source bytes."""
from pathlib import Path
import hashlib
import json
import os
import subprocess

ROOT = Path(__file__).resolve().parents[3]
OUT = Path(__file__).resolve().parent
CORE = 'crates/zeppelin-embed/src/property_graph/'
mutants = [
 ('duplicate-id', CORE+'catalog.rs', 'if previous == Some(entry.symbol) {', 'if false && previous == Some(entry.symbol) {', 'catalog_rejects_duplicate_assignments_and_regressed_high_waters'),
 ('regressed-water', CORE+'catalog.rs', 'if entry.symbol.get() > self.high_waters.get(entry.symbol.kind()) {', 'if false && entry.symbol.get() > self.high_waters.get(entry.symbol.kind()) {', 'catalog_rejects_duplicate_assignments_and_regressed_high_waters'),
 ('truncated-store', CORE+'catalog/codec.rs', '&self.declaration.store.get().to_le_bytes()', '&(self.declaration.store.get() as u64 as u128).to_le_bytes()', 'catalog_independent_goldens_pin_full_identity_and_exact_document_declaration'),
 ('ignored-model-version', CORE+'catalog/interpretation.rs', 'other.model_version.as_bytes()', 'self.model_version.as_bytes()', 'catalog_admission_refuses_every_changed_document_field_or_analyzer'),
 ('missing-chunk-poll', CORE+'catalog/work.rs', '        checkpoint()?;\n        let order = left.cmp(right);', '        let order = left.cmp(right);', 'catalog_long_name_lookup_observes_cancellation_during_comparison'),
 ('reserved-bytes', CORE+'catalog/codec.rs', 'if self.take(length)?.iter().any(|v| *v != 0) {', 'if self.take(length)?.iter().any(|v| *v != 0) && false {', 'catalog_corruption_refuses_truncation_tags_overflow_duplicates_and_trailing_bytes'),
 ('broken-utf8-carry', CORE+'catalog/work.rs', 'error.valid_up_to()', 'length', 'catalog_utf8_spanning_work_chunks_is_preserved_and_invalid_continuations_fail'),
 ('blind-oracle', 'tests/adversarial-oracle/src/graph_catalog.rs', 'if observed == expected {', 'if true || observed == expected {', 'primitive_catalog_oracle_rejects_malformed_or_mismatched_observations'),
]
results=[]
for name, relative, old, new, test in mutants:
    path=ROOT/relative
    original=path.read_bytes()
    sha=hashlib.sha256(original).hexdigest()
    assert original.decode().count(old)==1, name
    command=['cargo','test','-p','zeppelin-embed-adversarial-oracle' if name=='blind-oracle' else 'zeppelin-embed','--test','graph_catalog',test,'--','--exact','--nocapture']
    try:
        path.write_text(original.decode().replace(old,new))
        with (OUT/f'mutant-{name}.log').open('w') as log:
            result=subprocess.run(command,cwd=ROOT,env={**os.environ,'CARGO_TARGET_DIR':str(ROOT/'target')},stdout=log,stderr=subprocess.STDOUT)
        text=(OUT/f'mutant-{name}.log').read_text()
        assert result.returncode != 0 and f'test {test} ... FAILED' in text, f'mutation not behaviorally detected: {name}'
    finally:
        path.write_bytes(original)
        assert hashlib.sha256(path.read_bytes()).hexdigest()==sha
    results.append({'mutation':name,'source':relative,'restored_sha256':sha,'test':test,'exit':result.returncode,'detected':True})
    (OUT/'mutations.json').write_text(json.dumps(results,indent=2)+'\n')
    print(name,'detected; source restored',sha,flush=True)
