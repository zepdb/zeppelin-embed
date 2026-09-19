import hashlib,json,pathlib,subprocess
root=pathlib.Path(__file__).resolve().parents[3]
path=root/'crates/zeppelin-embed/src/property_graph/query/resources.rs'
original=path.read_bytes()
sha=hashlib.sha256(original).hexdigest()
cases=[('query-cap-overlap',b'        if total > self.limit {', b'        if false && total > self.limit {'),('shared-cap-overlap',b'        let shared = self.shared.reserve(bytes)?;',b'        let shared = self.shared.reserve(0)?;')]
results=[]
for name,old,new in cases:
    command=['cargo','nextest','run','-p','zeppelin-embed','--test','graph_completed_results','-E','test(real_retained_capacity_overlap)','--test-threads','1','--retries','0']
    try:
        mutated = original.replace(old,new)
        if name == 'query-cap-overlap':
            mutated = mutated.replace(b'if total > self.memory.limit {', b'if false && total > self.memory.limit {')
        else:
            mutated = mutated.replace(b'self.shared.resize(bytes)?;', b'self.shared.resize(0)?;')
        path.write_bytes(mutated)
        p=subprocess.run(command,cwd=root,stdout=subprocess.PIPE,stderr=subprocess.STDOUT)
        pathlib.Path('/tmp/ze-127-evidence/mutant-'+name+'.log').write_bytes(p.stdout)
    finally:
        path.write_bytes(original)
    results.append(dict(name=name,command=command,exit=p.returncode,sha256=sha,restored_sha256=hashlib.sha256(path.read_bytes()).hexdigest(),fired=p.returncode==100 and b'assertion failed' in p.stdout))
    pathlib.Path('/tmp/ze-127-evidence/capacity-mutations.json').write_text(json.dumps(results,indent=2)+'\n')
    print(json.dumps(results[-1]),flush=True)
    assert results[-1]['fired'],p.stdout.decode()[-3000:]
