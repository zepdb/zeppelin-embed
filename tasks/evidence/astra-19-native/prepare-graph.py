from pathlib import Path
import json,subprocess,shutil,hashlib,sys
r=Path(sys.argv[1]);old=Path('/private/tmp/ze-astra18-host-4b960428');subprocess.run(['cp','-cR',str(old/'after'),str(r/'before')],check=True)
for arm in ['before','after']:
 p=r/arm/'tools/matched-api/src/main.rs';s=p.read_text();oldcheck='health.graph_coverage!=0.0';assert s.count(oldcheck)==1;s=s.replace(oldcheck,'health.graph_coverage!=fixture["expected_graph_coverage"].as_f64().unwrap_or(0.0)');p.write_text(s)
 subprocess.run(['cp','-cR','/private/tmp/ze-beir-direct-graph-hao_zwxs/store-fiqa-published',str(r/f'store-{arm}-graph')],check=True)
fixture=json.loads((r/'fixture-all.json').read_text());fixture['expected_graph_coverage']=1.0;fixture['kind']='astra19 production-pair published Graph screen';(r/'fixture-graph.json').write_text(json.dumps(fixture)+'\n')
ids=json.loads(Path('/private/tmp/ze-beir-7xnojxa3/metadata/fiqa/source_ids.json').read_text());assert ids==[d['source_id'] for d in fixture['documents']]
bundle=Path('/private/tmp/ze-model-bundles-v2-c1/leaf-v1.5-pair.zem');coreml=bundle.with_suffix('.mlmodelc');assert bundle.is_file() and coreml.is_dir()
man=json.loads((r/'manifest.json').read_text());base=[x for x in man if x['state']=='all' and x['api']!='exact']
for x in base:
 x=json.loads(json.dumps(x));x['state']='graph';x['label']=x['label'].replace('-all-','-graph-');x['environment']={'ZE_QUERY_COREML':str(coreml),'ZE_QUERY_COREML_TOKENS':'64'};x['command']=[s.replace('fixture-all.json','fixture-graph.json').replace('-all-','-graph-').replace('-all','-graph') for s in x['command']];x['command'][1]=str(bundle);man.append(x)
(r/'manifest.json').write_text(json.dumps(man,indent=2)+'\n')
(r/'graph-provenance.json').write_text(json.dumps({'corpus_docs':57638,'chunks':58980,'queries':len(fixture['queries']),'query_selection':'unchanged Step18 SHA256 screen','source_index':'/private/tmp/ze-beir-direct-graph-hao_zwxs/store-fiqa-published','source_manifest':'/private/tmp/ze-beir-direct-graph-hao_zwxs/fiqa-campaign.json','bundle':str(bundle),'bundle_sha256':hashlib.sha256(bundle.read_bytes()).hexdigest(),'coreml':str(coreml),'harness_change':'validate explicit fixture graph coverage on both arms','planned_ordinary_processes':len(man)},indent=2)+'\n')
# The baseline executable must now be rebuilt with the same geometry contract.
shutil.move(r/'before-api',r/'reused-before-scan-only-api')
print(r,len(man))
