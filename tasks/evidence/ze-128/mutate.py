import hashlib,json,os,subprocess
from pathlib import Path
root=Path.cwd(); evidence=Path('/tmp/ze-128-evidence')
mutants=[
 ('arena_drop', 'crates/zeppelin-embed-ffi/src/graph_result.rs', 'if self.layout.size() != 0 {', 'if false && self.layout.size() != 0 {', 'graph_result_actual_allocator', 'assertion'),
 ('geometry', 'crates/zeppelin-embed-ffi/src/graph_result/registration.rs', 'fn same_geometry(a: &ZeGraphResponse, b: &ZeGraphResponse) -> bool {', 'fn same_geometry(a: &ZeGraphResponse, b: &ZeGraphResponse) -> bool {\n    return true;', 'graph_result_every_authoritative', 'abi_size'),
 ('final_cancel', 'crates/zeppelin-embed-ffi/src/graph_result/registration.rs', '        context.checkpoint()?;\n        Ok(prepared)', '        Ok(prepared)', 'graph_result_seeded_cancel', 'assertion'),
]
records=[]
for name,relative,before,after,test,marker in mutants:
 p=root/relative; original=p.read_bytes(); beforehash=hashlib.sha256(original).hexdigest()
 assert original.decode().count(before)==1,(name,'anchor count')
 try:
  p.write_text(original.decode().replace(before,after,1))
  command=['cargo','nextest','run','-p','zeppelin-embed-ffi','--features','graph-cypher','--lib','-E',f'test({test})']
  log=evidence/f'23-mutant-{name}.log'
  with log.open('w') as output:r=subprocess.run(command,stdout=output,stderr=subprocess.STDOUT,env={**os.environ,'ZE_TEST_SEED':'128'})
  observed=log.read_text(); assert r.returncode==100 and marker in observed,(name,r.returncode)
 finally:
  p.write_bytes(original)
 restored=hashlib.sha256(p.read_bytes()).hexdigest()
 assert restored==beforehash
 records.append({'name':name,'file':relative,'before':before,'after':after,'command':command,'ZE_TEST_SEED':'128','exit_code':r.returncode,'log':log.name,'original_sha256':beforehash,'restored_sha256':restored})
(evidence/'mutants.json').write_text(json.dumps(records,indent=2)+'\n')
