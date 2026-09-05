from pathlib import Path
import subprocess,os,json,time,shutil,hashlib,tempfile,tarfile,io,sys
root=Path.cwd();out=root/'tasks/evidence/astra-17-admitted'
def sha(p): return hashlib.sha256(Path(p).read_bytes()).hexdigest()
head=subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip()
assert head.startswith('501948b'),head
scratch=Path(sys.argv[1]) if len(sys.argv)>1 else Path(tempfile.mkdtemp(prefix='ze-astra17-'))
parent=scratch/'parent'
if not parent.exists():
 parent.mkdir()
 archive=subprocess.check_output(['git','archive',head])
 with tarfile.open(fileobj=io.BytesIO(archive)) as tf: tf.extractall(parent,filter='data')
example='crates/zeppelin-embed-bench/examples/live-df-cost.rs'
shutil.copy2(root/example,parent/example)
changed=subprocess.check_output(['git','diff','--name-only'],text=True).splitlines()
changed = [f for f in changed if f.startswith('crates/zeppelin-embed/')]
changed += ['crates/zeppelin-embed/src/fts/live_df.rs',example]
changed = sorted(set(changed))
(root/'tasks/evidence/astra-17-admitted/source.patch').write_bytes(subprocess.check_output(['git','diff']))
source={'head':head,'parent_source':str(parent),'scratch':str(scratch),'files':{
 f:{'before':sha(parent/f) if (parent/f).exists() else None,'after':sha(root/f)} for f in changed},
 'rustc':subprocess.check_output(['rustc','-Vv'],text=True),
 'os':subprocess.check_output(['sw_vers'],text=True),
 'hardware':subprocess.check_output(['sysctl','-n','hw.model','machdep.cpu.brand_string','hw.memsize'],text=True)}
(out/'source.json').write_text(json.dumps(source,indent=2))
receipts=[{**json.loads((root/'tasks/evidence/astra-17-measured/build-receipts.json').read_text())[0], 'reused_from':'astra-17-measured'}]
for arm,tree in [('after',root)]:
 package=scratch/arm;package.mkdir(exist_ok=True)
 manifest=f'''[package]
name = "live-df-screen"
version = "0.0.0"
edition = "2024"
[workspace]
[dependencies]
zeppelin-embed = {{ path = "{tree/'crates/zeppelin-embed'}", default-features = false }}
serde_json = "1"
[[bin]]
name = "live-df-screen"
path = "{tree/example}"
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
strip = "symbols"
panic = "unwind"
'''
 (package/'Cargo.toml').write_text(manifest)
 shutil.copy2(root/'Cargo.lock',package/'Cargo.lock')
 target=scratch/'target'
 cmd=['cargo','build','--release','--offline','--manifest-path',str(package/'Cargo.toml'),'--message-format=json']
 env=os.environ.copy();env['CARGO_TARGET_DIR']=str(target)
 start=time.time()
 with (out/(arm+'-build.log')).open('w') as log:
  proc=subprocess.run(cmd,env=env,stdout=log,stderr=subprocess.STDOUT)
 receipt={'arm':arm,'command':cmd,'env':{'CARGO_TARGET_DIR':str(target)},'exit_code':proc.returncode,'seconds':time.time()-start}
 receipts.append(receipt);(out/'build-receipts.json').write_text(json.dumps(receipts,indent=2))
 assert proc.returncode==0,receipt
 for line in (out/(arm+'-build.log')).read_text().splitlines():
  try: e=json.loads(line)
  except ValueError: continue
  if e.get('reason')=='compiler-artifact' and e.get('target',{}).get('name')=='zeppelin_embed':
   receipt['core_features']=e['features'];assert e['features']==[],e['features']
 binary=out/('screen-'+arm);shutil.copy2(target/'release/live-df-screen',binary)
 receipt.update(binary=str(binary),sha256=sha(binary),bytes=binary.stat().st_size)
 (out/'build-receipts.json').write_text(json.dumps(receipts,indent=2))
 print(arm,'built',round(receipt['seconds'],2),'seconds; core features',receipt['core_features'],flush=True)
