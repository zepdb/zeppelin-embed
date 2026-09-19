from pathlib import Path
import gzip, hashlib, json, subprocess
root=Path.cwd(); out=Path('/tmp/ze-35-integration')
results=json.loads((out/'commands.json').read_text())
assert len(results)==8 and all(r['exit_code']==0 for r in results)
crates=json.loads((out/'per-crate.json').read_text())
assert all(r['passes_90'] for r in crates.values())
base=json.loads((out/'production-baseline.json').read_text())
changed=[p for p,h in base['files'].items() if hashlib.sha256((root/p).read_bytes()).hexdigest()!=h]
assert sorted(changed)==['crates/zeppelin-embed/src/property_graph/mod.rs','tests/adversarial-oracle/src/lib.rs']
def files(path):
 return {str(Path(f['filename']).relative_to(root)):f['summary']['lines'] for f in json.loads(path.read_text())['data'][0]['files'] if f['filename'].startswith(str(root)+'/')}
old=files(Path('/tmp/ze-42-integration/clean-workspace-coverage.json')); new=files(out/'workspace-coverage.json')
prefixes=['crates/zeppelin-embed/src/','crates/zeppelin-embed-cypher/src/','crates/zeppelin-embed-ffi/src/','crates/zeppelin-embed-text/src/']
old={p:v for p,v in old.items() if any(p.startswith(r) for r in prefixes)}
new={p:v for p,v in new.items() if any(p.startswith(r) for r in prefixes)}
assert not set(old)-set(new)
deltas=[{'path':p,'before':old.get(p),'after':v} for p,v in sorted(new.items()) if p not in old or old[p]['count']!=v['count']]
(out/'denominator-audit.json').write_text(json.dumps({'no_removed_source':True,'new_feature_inventory':'Existing allocation-audit feature paths are included and exercised by the full core lib test run. Existing product source is unchanged; catalog module declaration is appended after the complete original prefix.','changed_denominators':deltas,'old_files':len(old),'new_files':len(new)},indent=2)+'\n')
feature_new={'crates/zeppelin-embed/src/allocation_audit.rs','crates/zeppelin-embed/src/fts/alloc_gate.rs'}
feature_growth={'crates/zeppelin-embed/src/fts/query.rs':29,'crates/zeppelin-embed/src/ingest/active.rs':3,'crates/zeppelin-embed/src/ingest/mod.rs':23,'crates/zeppelin-embed/src/lifecycle/mod.rs':26,'crates/zeppelin-embed/src/lifecycle/stats.rs':22,'crates/zeppelin-embed/src/property_graph/canonical.rs':60}
assert all(p.startswith('crates/zeppelin-embed/src/property_graph/catalog') or p in feature_new for p in set(new)-set(old)),set(new)-set(old)
assert all(r['before'] is None or r['after']['count']-r['before']['count']==feature_growth.get(r['path']) for r in deltas),deltas
module='crates/zeppelin-embed/src/property_graph/mod.rs'
base_module=subprocess.check_output(['git','show','77bc3fe:'+module])
assert (root/module).read_bytes().startswith(base_module)
assert old[module]['count']==new[module]['count'],(old[module],new[module])
profiles=json.loads((out/'profiles-before.json').read_text())
assert all(hashlib.sha256((root/p).read_bytes()).hexdigest()==h for p,h in profiles.items())
user=json.loads(Path('/tmp/ze-32-qualification/integration-preserve/hashes.json').read_text())
assert all(hashlib.sha256((root/p).read_bytes()).hexdigest()==h for p,h in user.items())
(out/'preservation.json').write_text(json.dumps({'user_file_hashes_match':len(user),'retained_profile_hashes_match':len(profiles),'old_production_source_changes':changed,'base':'77bc3fe0135f63a4eeff87f6f21cc29a239cb2e6','candidate':subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip()},indent=2)+'\n')
destination=root/'tasks/evidence/ze-35/integration';destination.mkdir(parents=True,exist_ok=True)
names=['qualify.py','finish.py','commands.json','per-crate.json','source-audit.json','candidate-source-inventory.json','production-baseline.json','profiles-before.json','denominator-audit.json','preservation.json']
names += [r['name']+'.log' for r in results]+['workspace-coverage.json','oracle-coverage.json','initial-per-crate.json','initial-workspace-coverage.json','initial-oracle-coverage.json','initial-commands.json']
manifest=[]
for name in names:
 data=(out/name).read_bytes(); compressed=name.endswith('.log') or name.endswith('coverage.json')
 path=destination/(name+'.gz' if compressed else name);path.write_bytes(gzip.compress(data,mtime=0) if compressed else data)
 manifest.append({'file':path.name,'raw_sha256':hashlib.sha256(data).hexdigest(),'raw_bytes':len(data),'retained_sha256':hashlib.sha256(path.read_bytes()).hexdigest(),'retained_bytes':path.stat().st_size})
(destination/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
print(json.dumps(deltas,indent=2))
print('Archived',len(manifest),'evidence files')
