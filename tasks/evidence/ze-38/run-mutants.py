from pathlib import Path
import subprocess, hashlib, json
root=Path(__file__).resolve().parents[3]
folder=root/'crates/zeppelin-embed/src/property_graph/wal'
Path('/tmp/ze-38-evidence').mkdir(exist_ok=True)
mutants=[
 ('digest', 'replay.rs', 'if rd.u64(r)? != hash(bytes.get(..cursor).ok_or(WalError::Malformed)?, r)? {', 'if rd.u64(r)? != hash(bytes.get(..cursor).ok_or(WalError::Malformed)?, r)? && false {', 'repaired_checksums_do_not_hide_frame_splicing_counts_or_state_regression'),
 ('count', 'replay.rs', 'if rd.u32(r)? != count {', 'if rd.u32(r)? != count && false {', 'repaired_checksums_do_not_hide_frame_splicing_counts_or_state_regression'),
 ('role', 'replay.rs', 'if rd.u16(r)? != tree as u16 {', 'if rd.u16(r)? != tree as u16 && false {', 'framed_objects_reject_swapped_tree_roles_and_unknown_participant_tags'),
 ('required-extent', 'replay.rs', 'validator.required(reference, RequiredRole::Canonical, r)?;', 'let _ = reference;', 'missing_middle_extent_is_observed_before_any_batch_escapes'),
 ('partition', 'maintenance.rs', 'std::cmp::Ordering::Equal => return Err(WalError::Malformed),', 'std::cmp::Ordering::Equal => { left += 1; right += 1; },', 'reclaim_completion_requires_disjoint_exact_candidate_partitions'),
 ('high-water', 'framing.rs', 'if base.high_waters.node > next.high_waters.node', 'if false', 'capacities_high_waters_and_sequence_never_wrap_or_partially_write'),
 ('final-cancel', 'replay.rs', '        r.charge(0)?;\n        self.state = state;', '        self.state = state;', 'cancellation_triggered_by_final_validation_cannot_publish_a_batch'),
]
results=[]
for name,file,old,new,test in mutants:
 p=folder/file; original=p.read_bytes(); text=original.decode(); assert text.count(old)==1,(name,text.count(old))
 before=hashlib.sha256(original).hexdigest()
 try:
  p.write_text(text.replace(old,new))
  log=Path('/tmp/ze-38-evidence')/f'mutant-{name}.log'
  with log.open('wb') as out:
   result=subprocess.run(['cargo','nextest','run','-p','zeppelin-embed','--test','graph_wal','-E',f'test({test})'],cwd=root,stdout=out,stderr=subprocess.STDOUT)
  logtext=log.read_text(); killed=result.returncode==100 and '1 failed' in logtext and test in logtext
  results.append({'mutant':name,'source':str(p.relative_to(root)),'before':before,'exit':result.returncode,'killed_by_named_runtime_test':killed,'test':test})
  if not killed: raise RuntimeError(f'mutant {name} was not killed: {log}')
 finally:
  p.write_bytes(original)
  assert hashlib.sha256(p.read_bytes()).hexdigest()==before
  if results: results[-1]['restored']=before
 Path('/tmp/ze-38-evidence/mutants.json').write_text(json.dumps(results,indent=2)+'\n')
 print(name, 'killed and restored', flush=True)
