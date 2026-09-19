import hashlib,json,pathlib,subprocess
root=pathlib.Path('/tmp/ze-126-core-adapter-scratch'); evidence=pathlib.Path('/tmp/ze-126-core-adapters')
results=[]
def run(name, extra, expected):
 command=['cargo','nextest','run','-p','zeppelin-embed','--test','graph_compiled_context',*extra]
 result=subprocess.run(command,cwd=root,capture_output=True,text=True)
 (evidence/(name+'.log')).write_text(result.stdout+result.stderr)
 results.append({'name':name,'command':command,'exit':result.returncode})
 print(name,result.returncode,flush=True)
 assert result.returncode==expected,(name,result.returncode,(result.stdout+result.stderr)[-4000:])
run('04-scratch-green',[],0)
p=root/'crates/zeppelin-embed/src/property_graph/query/resources/inputs.rs'; before=p.read_text(); hash_before=hashlib.sha256(p.read_bytes()).hexdigest()
try:
 marker='impl QueryArena'; head,tail=before.split(marker,1)
 old='Some(self.charge.memory as *const QueryMemory<\'_> as usize),'; assert old in tail
 p.write_text(head+marker+tail.replace(old,'None,',1))
 run('05-fact-credit-mutant-red',['-E','test(charged_fact_arena_is_credited_once)'],100)
finally:
 p.write_text(before); assert hashlib.sha256(p.read_bytes()).hexdigest()==hash_before
p=root/'crates/zeppelin-embed/src/property_graph/query/runtime/driver.rs'; before=p.read_text(); hash_before=hashlib.sha256(p.read_bytes()).hexdigest()
try:
 old='    drain(context, plan, source, completion, capacity)\n}'
 new='''    context.values = super::super::ValueContext::new(
        context.view(), context.values.control(), super::super::MAX_VALUE_WORK,
    ).map_err(|error| RuntimeFailure { operator: plan.plan().description().root, error: error.into(), counters: context.counters() })?;
    drain(context, plan, source, completion, capacity)
}'''
 assert old in before; p.write_text(before.replace(old,new,1))
 run('06-reset-value-context-mutant-red',['-E','test(borrowed_driver_keeps_prior_validation) | test(borrowed_driver_cannot_restart)'],100)
finally:
 p.write_text(before); assert hashlib.sha256(p.read_bytes()).hexdigest()==hash_before
(evidence/'mutations.json').write_text(json.dumps({'results':results,'restored':True},indent=2)+'\n')
