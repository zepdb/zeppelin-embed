from pathlib import Path
import subprocess, os, json, hashlib
root=Path('/Users/aghatage/Documents/code/zeppelin-embed-wt-ze-48')
out=Path('/tmp/ze-48-evidence/mutants')
base='crates/zeppelin-embed/src/property_graph/query/'
config='/Users/aghatage/Documents/code/zeppelin-embed-wt-ze-117/.config/nextest.toml'
def replace(old,new):
 def edit(s):
  assert s.count(old)==1,(old,s.count(old))
  return s.replace(old,new)
 return edit
def body(name,new):
 def edit(s):
  a=s.index(name); begin=s.index('{',a); level=1; end=begin+1
  while level:
   if s[end]=='{':level+=1
   elif s[end]=='}':level-=1
   end+=1
  return s[:begin+1]+'\n'+new+'\n'+s[end-1:]
 return edit
cases=[
 ('numeric-rounding',base+'value.rs',body('fn integer_float(', '    (integer as f64).partial_cmp(&float)'), 'graph_query_values','mixed_numeric_predicates_never_round_integer_identity'),
 ('group-hash-bits',base+'grouping.rs',replace('if integer_float(integer, value) == Some(Ordering::Equal) {','if integer_float(integer, value) == Some(Ordering::Less) {'),'graph_query_values','grouping_hash_and_total_order_follow_equivalence_not_replay_bits'),
 ('byte-poll-omission',base+'value.rs',replace('for (a, b) in left.chunks(65_536).zip(right.chunks(65_536)) {\n        context.step()?;','for (a, b) in left.chunks(65_536).zip(right.chunks(65_536)) {\n        let _ = &context;'),'graph_query_values','deadline_is_checked_inside_byte_chunks_and_list_elements'),
 ('list-depth-overlimit',base+'list.rs',replace('depth > MAX_LIST_DEPTH','depth > MAX_LIST_DEPTH + 1'),'graph_query_values','value_limits_accept_exact_depth_and_stop_before_unreserved_work'),
 ('scope-drop',base+'plan/expression.rs',replace('scope.slot(id).ok_or(PlanError::Scope)?','scope.slot(id).unwrap_or(ValueKinds::BOOL)'),'graph_query_plan','typed_plan_revalidates_shared_expressions_after_with_scope'),
 ('unproved-span',base+'plan/accounting.rs',replace('if span.start == span.end {','if span.start <= span.end {'),'graph_query_plan','retained_region_proof_rejects_gaps_overlap_overflow_and_uncharged_capacity'),
 ('lost-lineage',base+'plan/lineage.rs',replace('pattern == other_pattern && origin != other_origin','pattern == other_pattern && origin == other_origin'),'graph_query_plan','renamed_origins_remain_checked_through_multiple_patterns'),
 ('search-obligation',base+'plan/search.rs',body('fn inventory(', '    let _ = (description, context);\n    Ok(())'),'graph_query_plan','eager_search_obligations_survive_limit_zero_and_require_singleton_sources'),
 ('mutation-barrier',base+'plan/validate.rs',replace('if !matches!(\n                op.inputs','if false && !matches!(\n                op.inputs'),'graph_query_plan','mutations_require_eager_input_and_reject_following_reads_or_search'),
 ('oracle-relation','tests/adversarial-oracle/src/graph_query.rs',replace('observed.equal != equal(left, right)','observed.equal == equal(left, right)'),'adversarial_tests','query_oracle_rejects_corrupted_primitive_observations'),
 ('runner-omission','tests/adversarial/runner.rs',replace('    super::graph_query::probe(seed, &mut coverage)?;',''),'adversarial_tests','one_runner_episode_reaches_required_query_contracts'),
]
results=[]
env=dict(os.environ,CARGO_TARGET_DIR='target/ze48')
for label,path,edit,target,test in cases:
 file=root/path; original=file.read_bytes(); digest=hashlib.sha256(original).hexdigest()
 saved=out/(label+'.original');saved.write_bytes(original)
 (out/'active.json').write_text(json.dumps({'path':str(file),'original':str(saved),'sha256':digest}))
 result={'label':label,'path':path,'test':test,'original_sha256':digest}
 try:
  file.write_text(edit(original.decode()))
  cmd=['cargo','nextest','run','--config-file',config,'-p','zeppelin-embed' if target!='adversarial_tests' else 'zeppelin-embed-workspace-tests']
  if target!='adversarial_tests':cmd+=['--features','test-support']
  cmd+=['--test',target,'-E','test('+test+')']
  result['command']=cmd
  with (out/(label+'.log')).open('w') as log: result['exit']=subprocess.run(cmd,cwd=root,env=env,stdout=log,stderr=subprocess.STDOUT).returncode
  log=(out/(label+'.log')).read_text()
  result['intended_failure']=result['exit']==100 and 'FAIL' in log and test in log
 finally:
  file.write_bytes(original)
  result['restored_sha256']=hashlib.sha256(file.read_bytes()).hexdigest()
  assert result['restored_sha256']==digest
  (out/'active.json').unlink()
  results.append(result);(out/'results.json').write_text(json.dumps(results,indent=2)+'\n')
 print(json.dumps(result),flush=True)
 if not result.get('intended_failure'):raise SystemExit('mutation did not reach intended test failure')
