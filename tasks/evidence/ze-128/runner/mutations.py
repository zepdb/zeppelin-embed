import hashlib,json,os,subprocess
from pathlib import Path
cases=[
('missing_route','tests/adversarial/runner.rs','    super::graph_response::probe(seed, &mut coverage)?;', '    { let _ = seed; }', 'one_runner_episode_reaches_required_graph_response_contracts','actual runner omitted'),
('missing_fire','crates/zeppelin-embed-ffi/src/graph_result/test_support.rs','state.fires = state.fires.saturating_add(1);','state.fires = state.fires;', 'property_graph_response_probe_checks','unproved Allocation'),
]
records=[]
for name,filename,old,new,test,marker in cases:
 p=Path(filename); original=p.read_bytes(); assert original.decode().count(old)==1
 log=Path('/tmp/ze-128-runner-evidence')/('05-'+name+'-red.log')
 command=['cargo','nextest','run','-p','zeppelin-embed-workspace-tests','--features','graph-cypher','--test','adversarial_tests','-E','test('+test+')']
 try:
  p.write_text(original.decode().replace(old,new,1))
  with log.open('w') as out:result=subprocess.run(command,stdout=out,stderr=subprocess.STDOUT)
  assert result.returncode==100 and marker in log.read_text(),(name,result.returncode)
 finally:p.write_bytes(original)
 sha=hashlib.sha256(original).hexdigest();restored=hashlib.sha256(p.read_bytes()).hexdigest();assert sha==restored
 records.append({'name':name,'path':filename,'before':old,'after':new,'command':command,'exit_code':result.returncode,'log':log.name,'original_sha256':sha,'restored_sha256':restored})
Path('/tmp/ze-128-runner-evidence/mutations.json').write_text(json.dumps(records,indent=2)+'\n')
