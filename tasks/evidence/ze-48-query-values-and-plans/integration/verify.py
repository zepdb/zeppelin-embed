from pathlib import Path
import subprocess,json
out=Path('/tmp/ze-48-integration');commands=[
('core',['cargo','nextest','run','-p','zeppelin-embed','--features','test-support','--test','graph_query_values','--test','graph_query_plan','--locked','--no-tests=fail']),
('oracle-probes',['cargo','nextest','run','-p','zeppelin-embed-workspace-tests','--test','adversarial_tests','-E','test(property_graph_query_probe) | test(query_oracle_rejects) | test(one_runner_episode_reaches_required_query_contracts)','--locked','--no-tests=fail','--success-output','final']),
('clippy',['cargo','clippy','-p','zeppelin-embed','-p','zeppelin-embed-adversarial-oracle','-p','zeppelin-embed-workspace-tests','--all-targets','--features','zeppelin-embed/test-support','--no-deps','--','-D','warnings']),
('fmt',['cargo','fmt','--all','--check'])];results=[]
for name,args in commands:
 print('Running',name,flush=True)
 with (out/(name+'.log')).open('wb') as log:r=subprocess.run(args,stdout=log,stderr=subprocess.STDOUT)
 results.append({'name':name,'command':args,'exit_code':r.returncode});(out/'commands.json').write_text(json.dumps(results,indent=2)+'\n')
 if r.returncode:print((out/(name+'.log')).read_text()[-5000:]);raise SystemExit(r.returncode)
 print(name,'passed',flush=True)
