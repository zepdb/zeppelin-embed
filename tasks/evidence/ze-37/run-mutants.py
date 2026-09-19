from pathlib import Path
import subprocess,json,hashlib,os
root=Path('/Users/aghatage/Documents/code/zeppelin-embed-wt-ze-37');out=Path('/tmp/ze-37-evidence');os.chdir(root)
env=os.environ.copy();env['CARGO_TARGET_DIR']='target/ze37'
rows=[]
def sha(b):return hashlib.sha256(b).hexdigest()
def run(name,path,change,args,needle):
 p=root/path;original=p.read_bytes();s=original.decode();mutated=change(s)
 assert s!=mutated,name
 try:
  p.write_text(mutated)
  with (out/(name+'-red.log')).open('wb') as log:
   result=subprocess.run(['cargo','nextest','run',*args],env=env,stdout=log,stderr=subprocess.STDOUT)
  log=(out/(name+'-red.log')).read_text()
  fired=result.returncode==100 and needle in log and 'FAIL' in log
  row={'name':name,'path':path,'command':['cargo','nextest','run',*args],'original_sha256':sha(original),'mutant_sha256':sha(mutated.encode()),'exit':result.returncode,'intended_assertion':needle,'fired':fired}
 finally:p.write_bytes(original)
 row['restored_sha256']=sha(p.read_bytes());rows.append(row);(out/'mutation-results.json').write_text(json.dumps(rows,indent=2)+'\n')
 print(name,result.returncode,fired,flush=True)
 if not fired:raise SystemExit('intended mutant did not fire: '+name)
def replace(old,new):
 def f(s):
  assert s.count(old)==1,(old,s.count(old))
  return s.replace(old,new)
 return f
run('abi-arena-cap','crates/zeppelin-embed/src/property_graph/staging/result.rs',lambda s:s.replace('        || layout.abi_bytes > memory.limits.result_bytes\n','').replace('        || abi.allocated_bytes() > memory.limits.result_bytes\n',''),['-p','zeppelin-embed','--test','graph_write_staging','-E','test(result_materialization_counts)'],'the complete ABI arena must fit its result limit')
run('copied-result-view','crates/zeppelin-embed/src/property_graph/staging/result.rs',replace('if base.identity() != batch.base() {','if false && base.identity() != batch.base() {'),['-p','zeppelin-embed','--test','graph_write_staging','-E','test(final_view_drift)'],'assertion failed: matches!(result, Err(StageError::ViewMismatch))')
run('shared-owner','crates/zeppelin-embed/src/property_graph/staging/memory.rs',replace('if !charge.belongs_to(self.resources) {','if false && !charge.belongs_to(self.resources) {'),['-p','zeppelin-embed','--test','graph_write_staging','-E','test(writer_adoption_checks)'],'foreign accounting owner accepted')
run('replay-generation-pg10','crates/zeppelin-embed/src/property_graph/staging/structured.rs',replace('generation: fields.original_generation,','generation: GraphGeneration::new(fields.original_generation.get().wrapping_add(1)),'),['-p','zeppelin-embed-workspace-tests','--test','adversarial_tests','-E','test(property_graph_staging_probe)'],'PG10 private staging expected=')
run('allocation-attribution-v2','crates/zeppelin-embed/src/property_graph/staging/memory.rs',replace('crate::allocation_audit::attributed(|| values.try_reserve_exact(capacity))','values.try_reserve_exact(capacity)'),['-p','zeppelin-embed','--lib','--features','allocation-audit','-E','test(staging_owns_actual)'],'staging allocation must have a charged owner')
