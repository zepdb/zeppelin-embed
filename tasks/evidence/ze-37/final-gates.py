import subprocess,os,json
from pathlib import Path
root=Path('/Users/aghatage/Documents/code/zeppelin-embed-wt-ze-37');out=Path('/tmp/ze-37-evidence');env=os.environ.copy();env['CARGO_TARGET_DIR']='target/ze37'
commands=[
('terminal-core',['cargo','nextest','run','-p','zeppelin-embed','--test','graph_write_staging','--test','graph_key_lifecycle','--test','graph_canonical','--success-output','final']),
('terminal-lib',['cargo','nextest','run','-p','zeppelin-embed','--lib','--features','allocation-audit','-E','test(property_graph::staging::tests::)','--success-output','final']),
('terminal-runner',['cargo','nextest','run','-p','zeppelin-embed-workspace-tests','--test','adversarial_tests','-E','test(property_graph_staging_probe) | test(property_graph_key_lifecycle_probe)','--success-output','final']),
('terminal-oracle',['cargo','nextest','run','-p','zeppelin-embed-adversarial-oracle','-E','test(primitive_staging_oracle)']),
('terminal-core-clippy',['cargo','clippy','-p','zeppelin-embed','--lib','--test','graph_write_staging','--test','graph_key_lifecycle','--features','allocation-audit,test-support','--no-deps','--','-D','warnings']),
('terminal-doc',['cargo','doc','-p','zeppelin-embed','--no-deps']),
('terminal-fmt',['cargo','fmt','--all','--','--check']),
('terminal-diff',['git','diff','--check']),
]
rows=[]
for name,cmd in commands:
 child_env=env.copy()
 if name=='terminal-doc':child_env['RUSTDOCFLAGS']='-D warnings'
 with (out/(name+'.log')).open('wb') as log:r=subprocess.run(cmd,cwd=root,env=child_env,stdout=log,stderr=subprocess.STDOUT)
 rows.append({'name':name,'command':cmd,'workdir':str(root),'CARGO_TARGET_DIR':'target/ze37','RUSTDOCFLAGS':child_env.get('RUSTDOCFLAGS'),'exit_code':r.returncode})
 (out/'final-gates.json').write_text(json.dumps(rows,indent=2)+'\n');print(name,r.returncode,flush=True)
 if r.returncode:raise SystemExit(r.returncode)
