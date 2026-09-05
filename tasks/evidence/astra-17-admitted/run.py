from pathlib import Path
import subprocess,json,time,re,hashlib,os
root=Path.cwd();out=root/'tasks/evidence/astra-17-admitted'
source=json.loads((out/'source.json').read_text())
for f,hashes in source['files'].items():
 assert hashlib.sha256((root/f).read_bytes()).hexdigest()==hashes['after'],f
builds={b['arm']:b for b in json.loads((out/'build-receipts.json').read_text())}
receipts=[]
for rep,order in enumerate([['before','after'],['after','before'],['before','after']],1):
 for arm in order:
  label=f'{arm}-rep{rep}';binary=builds[arm]['binary']
  assert hashlib.sha256(Path(binary).read_bytes()).hexdigest()==builds[arm]['sha256']
  deadline=time.monotonic()+300
  while os.getloadavg()[0]>3:
   if time.monotonic()>deadline:raise RuntimeError('host idle gate unavailable')
   time.sleep(5)
  destination=out/label;assert not destination.exists()
  cmd=['/usr/bin/time','-l',binary,str(destination)]
  start=time.time();before=os.getloadavg();env=os.environ.copy()
  for key in list(env):
   if key.startswith('ZE_'):env.pop(key)
  with (out/(label+'.log')).open('x') as log:
   proc=subprocess.run(cmd,env=env,stdout=log,stderr=subprocess.STDOUT)
  timing=(out/(label+'.log')).read_text();rss=re.search(r'(\d+)\s+maximum resident set size',timing)
  receipt={'label':label,'arm':arm,'rep':rep,'command':cmd,'exit_code':proc.returncode,'seconds':time.time()-start,'maximum_rss_bytes':int(rss[1]) if rss else None,'load_before':before,'load_after':os.getloadavg()}
  receipts.append(receipt);(out/'run-receipts.json').write_text(json.dumps(receipts,indent=2))
  assert proc.returncode==0,receipt
  data=json.loads((destination/'results.json').read_text());assert len(data['cases'])==6
  for case in data['cases']:
   for workload in case['workloads']:
    assert len(workload['us'])==128
    assert len(workload['controls'])==128
  print(label,'passed',round(receipt['seconds'],2),'seconds',flush=True)
(out/'complete.json').write_text(json.dumps({'complete':True,'processes':len(receipts)},indent=2))
