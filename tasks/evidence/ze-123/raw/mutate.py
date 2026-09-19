from pathlib import Path
import subprocess,hashlib,json
root=Path('/Users/aghatage/Documents/code/zeppelin-embed-wt-ze-123')
folder=Path('/tmp/ze-123-evidence')
validate=root/'crates/zeppelin-embed/src/property_graph/query/plan/validate.rs'
mod=root/'crates/zeppelin-embed/src/property_graph/query/plan/mod.rs'
cases=[
 ('omit-or-array',validate,'                accounting.span(relationship_types, context)?;','                // planted omission of type-array backing','typed_pattern_requires_array_and_every_alternative_name_backing'),
 ('allow-private-output-alias',validate,'                if predicate.current_edge == node || predicate.current_edge == relationships {\n                    return Err(PlanError::Scope);\n                }','                // planted alias acceptance','typed_pattern_predicate_rejects_aliases_foreign_scope_and_output_escape'),
 ('predicate-sees-new-outputs',validate,'                let mut scope = input.clone();','                let mut scope = output.clone();','typed_pattern_predicate_rejects_aliases_foreign_scope_and_output_escape'),
 ('ordinal-reads-unused-cell',mod,'        let slot = self.slots.get(..self.width)?.get(ordinal)?;','        let slot = self.slots.get(ordinal)?;','typed_pattern_contract_preserves_or_types_and_private_edge_scope'),
]
rows=[]
for name,path,old,new,test in cases:
 original=path.read_bytes();text=original.decode();assert text.count(old)==1,(name,text.count(old));before=hashlib.sha256(original).hexdigest()
 cmd=['cargo','nextest','run','-p','zeppelin-embed','--test','graph_query_plan','-E','test('+test+')']
 try:
  path.write_text(text.replace(old,new))
  r=subprocess.run(cmd,cwd=root,stdout=subprocess.PIPE,stderr=subprocess.STDOUT)
  (folder/(name+'.log')).write_bytes(r.stdout)
  assert r.returncode==100,(name,r.returncode,r.stdout[-1500:])
  assert b'assertion' in r.stdout and test.encode() in r.stdout,(name,'missing runtime assertion')
 finally: path.write_bytes(original)
 after=hashlib.sha256(path.read_bytes()).hexdigest();assert before==after
 rows.append({'name':name,'path':str(path.relative_to(root)),'command':cmd,'exit':r.returncode,'original_sha256':before,'restored_sha256':after})
 (folder/'mutation-results.json').write_text(json.dumps(rows,indent=2)+'\n')
 print(name,'RED100 restored',flush=True)
