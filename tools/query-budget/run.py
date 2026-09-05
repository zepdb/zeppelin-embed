"""Sequential independent-process cells with provenance and a bounded idle gate."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time


def run(manifest, output, load_limit=3.0):
    output.mkdir()
    cells=json.loads(manifest.read_text())
    receipts=[]
    for cell in cells:
        deadline=time.monotonic()+300
        while os.getloadavg()[0]>load_limit:
            if time.monotonic()>deadline:
                raise RuntimeError(f'Idle gate unavailable: load {os.getloadavg()}, limit {load_limit}')
            print('WAIT_IDLE',cell['label'],os.getloadavg(),flush=True)
            time.sleep(5)
        env=os.environ.copy()
        for key in list(env):
            if key.startswith('ZE_QUERY_') or key in ['ZE_MLX_DEVICE','ZE_KERNEL','ZE_EXACT_WORKERS','ZE_BUDGET_QUERY_MAX_TOKENS','ZE_BUDGET_GRAPH_COVERAGE']:
                env.pop(key)
        env.update(cell.get('environment',{}))
        label=cell['label'];command=cell['command']
        before=os.getloadavg();start=time.time()
        binary=Path(command[0]);digest=hashlib.sha256(binary.read_bytes()).hexdigest()
        print('START',label,flush=True)
        with (output/(label+'.log')).open('x') as log:
            child=subprocess.Popen(['/usr/bin/time','-l',*command],env=env,stdout=log,stderr=subprocess.STDOUT)
            (output/'live.json').write_text(json.dumps({'pid':child.pid,'label':label,'command':command},indent=2))
            code=child.wait()
        receipt={**cell,'binary_sha256':digest,'exit_code':code,'seconds':time.time()-start,
                 'load_before':before,'load_after':os.getloadavg()}
        receipts.append(receipt)
        (output/'receipts.json').write_text(json.dumps(receipts,indent=2))
        if code:
            raise RuntimeError(receipt)
        print('DONE',label,round(receipt['seconds'],2),flush=True)
    (output/'complete.json').write_text(json.dumps({'complete':True,'processes':len(receipts)},indent=2))


if __name__=='__main__':
    parser=argparse.ArgumentParser()
    parser.add_argument('manifest',type=Path)
    parser.add_argument('output',type=Path)
    parser.add_argument('--load-limit',type=float,default=3.0)
    args=parser.parse_args()
    run(args.manifest,args.output,args.load_limit)
