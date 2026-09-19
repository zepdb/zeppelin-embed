import pathlib,subprocess,json,hashlib
p=pathlib.Path('/tmp/ze-37-integration')
def sha(b):return hashlib.sha256(b).hexdigest()
rows=[]
cases=[('query-charge-red','crates/zeppelin-embed/src/property_graph/query/resources.rs',b'.checked_add(bytes)\n                .ok_or(MemoryError::Limit)?;',b'.checked_add(0)\n                .ok_or(MemoryError::Limit)?;', ['-p','zeppelin-embed','--test','graph_write_staging','-E','test(staged_results_adopt_actual_query_memory)'],'query limit must reject at the selected ownership transfer'),('runner-route-red','tests/adversarial/runner.rs',b'    super::graph_staging::probe(seed, &mut coverage)?;\n',b'', ['-p','zeppelin-embed-workspace-tests','--test','adversarial_tests','-E','test(one_runner_episode_reaches_required_staging_contracts)'],'actual runner omitted property-graph.staging.')]
for name,file,old,new,args,expected in cases:
 f=pathlib.Path(file);before=f.read_bytes();assert before.count(old)==1,(file,before.count(old));after=before.replace(old,new);f.write_bytes(after)
 try:
  result=subprocess.run(['python3',str(p/'run.py'),name,'cargo','nextest','run',*args,'--success-output','final'])
  log=(p/(name+'.log')).read_text();assert result.returncode==100,(name,result.returncode);assert expected in log,(name,log[-2500:])
 finally:f.write_bytes(before)
 restored=f.read_bytes();assert restored==before
 rows.append(dict(name=name,path=file,original=sha(before),mutated=sha(after),restored=sha(restored),intended_failure=expected,exit_code=result.returncode))
 (p/'mutation-results.json').write_text(json.dumps(rows,indent=2)+'\n')
