"""Freeze a matched Step18/19 ordinary API screen without changing main defaults."""
from pathlib import Path
import hashlib,io,json,shutil,subprocess,tarfile,tempfile
repo=Path(__file__).resolve().parents[3]
old=Path('/private/tmp/ze-astra18-host-4b960428')
r=Path(tempfile.mkdtemp(prefix='ze-astra19-host-',dir='/private/tmp'))
Path('/private/tmp/ze-astra19-host-current').write_text(str(r)+'\n')
head=subprocess.check_output(['git','rev-parse','HEAD'],cwd=repo,text=True).strip();assert head=='192689b5dfb19a77a3f580aaf3d0f9b667219c5d'
def digest(p):return hashlib.sha256(p.read_bytes()).hexdigest()
adapters=[f'crates/zeppelin-embed-text/src/{name}.rs' for name in ['bundle','tokenizer','epoch']]+['crates/zeppelin-embed-text/Cargo.toml']
files=subprocess.check_output(['git','ls-files','crates/zeppelin-embed','crates/zeppelin-embed-text','Cargo.toml','Cargo.lock','.cargo/config.toml'],cwd=repo,text=True).splitlines()
verified={}
for f in files:
 if f in adapters:continue
 b=subprocess.check_output(['git','show',head+':'+f],cwd=repo);assert (old/'after'/f).read_bytes()==b,f
 verified[f]=hashlib.sha256(b).hexdigest()
receipts=json.loads((old/'build-receipts.json').read_text());before_receipt=next(x for x in receipts if x['arm']=='after');assert before_receipt['binary_sha256']==digest(old/'after-api') and before_receipt['core_features']==[]
shutil.copy2(old/'after-api',r/'before-api')
after=r/'after';after.mkdir();archive=subprocess.check_output(['git','archive',head],cwd=repo)
with tarfile.open(fileobj=io.BytesIO(archive)) as tar:tar.extractall(after,filter='data')
dirty=subprocess.check_output(['git','diff','--name-only','--','crates/zeppelin-embed','crates/zeppelin-embed-text'],cwd=repo,text=True).splitlines()
dirty+=['crates/zeppelin-embed-text/src/query_overlap_tests.rs','crates/zeppelin-embed/src/lifecycle/hybrid_overlap_tests.rs']
for f in dirty:shutil.copy2(repo/f,after/f)
for f in adapters:shutil.copy2(old/'after'/f,after/f)
shutil.copytree(old/'after/tools/matched-api',after/'tools/matched-api')
for state in ['all','tomb']:
 shutil.copy2(old/f'fixture-{state}.json',r/f'fixture-{state}.json')
 for arm in ['before','after']:
  subprocess.run(['cp','-cR',str(old/f'store-after-{state}'),str(r/f'store-{arm}-{state}')],check=True)
shutil.copy2(old/'run.py',r/'run.py')
manifest=[]
for cell in json.loads((old/'manifest.json').read_text()):
 cell['command']=[s.replace(str(old),str(r)) for s in cell['command']];manifest.append(cell)
(r/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
(r/'source.json').write_text(json.dumps({'base':head,'before_reused_binary':str(old/'after-api'),'before_original_receipt':before_receipt,'verified_unchanged_baseline_files':verified,'adapter_files':{f:digest(after/f) for f in adapters},'changed_files':{f:digest(repo/f) for f in dirty},'after_files':{str(p.relative_to(after)):digest(p) for p in after.rglob('*') if p.is_file()}},indent=2)+'\n')
(r/'source.patch').write_bytes(subprocess.check_output(['git','diff','--','crates/zeppelin-embed','crates/zeppelin-embed-text'],cwd=repo))
print(r)
