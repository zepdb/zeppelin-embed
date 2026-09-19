from pathlib import Path
import subprocess,json
out=Path('/tmp/ze-34-integration')
commands=[
('core', ['cargo','nextest','run','-p','zeppelin-embed','--features','allocation-audit','--lib','--test','graph_key_lifecycle','--test','graph_canonical','-E','binary(graph_key_lifecycle) | binary(graph_canonical) | test(=property_graph::key_lifecycle::allocation_tests::full_target_sort_and_exact_retry_have_zero_allocator_calls)','--locked','--no-tests=fail']),
('oracle-probes',['cargo','nextest','run','-p','zeppelin-embed-adversarial-oracle','-p','zeppelin-embed-workspace-tests','--lib','--test','adversarial_tests','-E','test(graph_key_lifecycle)','--locked','--no-tests=fail','--success-output','final']),
('clippy',['cargo','clippy','-p','zeppelin-embed','-p','zeppelin-embed-adversarial-oracle','-p','zeppelin-embed-workspace-tests','--all-targets','--features','zeppelin-embed/test-support,zeppelin-embed/allocation-audit','--no-deps','--','-D','warnings']),
('fmt',['cargo','fmt','--all','--check'])]
results=[]
for name,args in commands:
 print('Running',name,flush=True)
 with (out/(name+'.log')).open('wb') as log:run=subprocess.run(args,stdout=log,stderr=subprocess.STDOUT)
 results.append({'name':name,'command':args,'exit_code':run.returncode});(out/'commands.json').write_text(json.dumps(results,indent=2)+'\n')
 if run.returncode:
  print((out/(name+'.log')).read_text()[-5000:]);raise SystemExit(run.returncode)
 print(name,'passed',flush=True)
