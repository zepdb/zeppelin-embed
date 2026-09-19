from pathlib import Path
import subprocess,json,time,hashlib
root=Path('/Users/aghatage/Documents/code/zeppelin-embed-wt-ze-124'); evidence=Path('/tmp/ze-124')
checks=[
 ('final-core',['cargo','nextest','run','-p','zeppelin-embed','--lib','--test','graph_adjacency','--test','graph_artifact','--features','allocation-audit','-E','binary(graph_adjacency) | binary(graph_artifact) | test(adjacency_actual_allocator_reports_zero_for_encode_merge_and_all_error_classes)','--test-threads','4','--success-output','final']),
 ('final-runner',['cargo','nextest','run','-p','zeppelin-embed-workspace-tests','--test','adversarial_tests','-E','test(property_graph_adjacency_probe_checks_edges_faults_and_comparator_controls) | test(one_runner_episode_reaches_required_adjacency_contracts)','--test-threads','4','--success-output','final']),
 ('final-oracle',['cargo','nextest','run','-p','zeppelin-embed-adversarial-oracle','-E','test(graph_adjacency)','--test-threads','4']),
 ('final-clippy-core',['cargo','clippy','-p','zeppelin-embed','--all-targets','--features','test-support,allocation-audit','--no-deps','--','-D','warnings']),
 ('final-clippy-runner',['cargo','clippy','-p','zeppelin-embed-workspace-tests','--test','adversarial_tests','-p','zeppelin-embed-adversarial-oracle','--no-deps','--','-D','warnings']),
]
newrust=list(root.glob('crates/zeppelin-embed/src/property_graph/storage/adjacency/*.rs'))+[root/p for p in ['crates/zeppelin-embed/tests/graph_adjacency.rs','crates/zeppelin-embed/src/property_graph/storage/artifact.rs','crates/zeppelin-embed/src/property_graph/storage/mod.rs','crates/zeppelin-embed/src/property_graph/wal/codec.rs','tests/adversarial-oracle/src/graph_adjacency.rs','tests/adversarial/graph_adjacency.rs','fuzz/fuzz_targets/native_graph_adjacency.rs']]
checks.append(('final-fmt',['rustfmt','--edition','2024','--check','--config','skip_children=true']+[str(p.relative_to(root)) for p in newrust]))
checks.append(('final-whitespace',['git','diff','--check']))
rows=[]
for name,cmd in checks:
 start=time.time()
 with (evidence/(name+'.log')).open('w') as out:r=subprocess.run(cmd,cwd=root,stdout=out,stderr=subprocess.STDOUT)
 rows.append({'name':name,'command':cmd,'exit':r.returncode,'seconds':round(time.time()-start,3)})
 (evidence/'final-checks.json').write_text(json.dumps(rows,indent=2)+'\n');print(name,r.returncode,flush=True)
 if r.returncode:break
