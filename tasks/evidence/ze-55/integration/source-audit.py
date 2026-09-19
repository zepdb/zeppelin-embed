from pathlib import Path
import hashlib,json,subprocess,sys
ROOT=Path('/Users/aghatage/Documents/code/zeppelin-embed')
OUT=Path('/tmp/ze-55-integration')
SOURCE='5c9e757c05f977aa864a82fb3ab40630aff976b8'
BASE='f9d81e0806ca68c876a26ed1983a3fffaf72b902'
PARENT='8517c9e'
CONFLICTS=['crates/zeppelin-embed/CLAUDE.md','tests/adversarial-oracle/src/lib.rs','tests/adversarial/coverage.rs','tests/adversarial/mod.rs','tests/adversarial/runner.rs','tests/adversarial_tests.rs']
def git(rev,path): return subprocess.check_output(['git','show',rev+':'+path],cwd=ROOT)
def sha(data): return hashlib.sha256(data).hexdigest()
def actual(path): return git(sys.argv[1],path) if len(sys.argv)>1 else (ROOT/path).read_bytes()
records=[]
for path in json.loads((ROOT/'tasks/evidence/ze-55/source-audit.json').read_text())['owned_paths'] + ['tasks/evidence/ze-55-cypher-binding.md','tasks/evidence/ze-55/interface-review.md','tasks/evidence/ze-55/mutations.json','tasks/evidence/ze-55/raw-log-sha256.json','tasks/evidence/ze-55/raw-logs.tar.gz','tasks/evidence/ze-55/resource-review.md','tasks/evidence/ze-55/run-mutants.py','tasks/evidence/ze-55/source-audit.json','tasks/evidence/ze-55/source-sha256.json']:
 candidate=git(SOURCE,path)
 if path not in CONFLICTS: expected=candidate
 else:
  parent=git(PARENT,path); base=git(BASE,path)
  if path.endswith('runner.rs'):
   needle=b'    super::graph_runtime::probe(seed, &mut coverage)?;\n'
   addition=b'    super::graph_binding::probe(seed, &mut coverage)?;\n'
   assert candidate==base.replace(needle,needle+addition)
   expected=parent.replace(needle,needle+addition)
  elif path.endswith('coverage.rs'):
   needle=b'pub const REQUIRED_SMOKE_COVERAGE: &[&str] = &[\n'
   addition=b''.join(b'    "'+key.encode()+b'",\n' for key in ['property-graph.binding.scope','property-graph.binding.profile','property-graph.binding.bits','property-graph.binding.modes','property-graph.binding.cancel.fire','property-graph.binding.budget.fire','property-graph.binding.same-seed-control'])
   assert candidate==base.replace(needle,needle+addition)
   expected=parent.replace(needle,needle+addition)
  else:
   assert candidate.startswith(base),path
   expected=parent+candidate[len(base):]
 data=actual(path); records.append({'path':path,'exact':data==expected,'expected_sha256':sha(expected),'actual_sha256':sha(data),'expected_bytes':len(expected),'actual_bytes':len(data),'rule':'parent plus complete exact candidate delta' if path in CONFLICTS else 'exact candidate'})
label='revision-'+sys.argv[1] if len(sys.argv)>1 else 'working'
(OUT/('source-audit-'+label+'.json')).write_text(json.dumps(records,indent=2)+'\n')
failed=[r for r in records if not r['exact']]
print('integration_preserves_complete_parent_and_candidate_deltas:',len(records)-len(failed),'/',len(records),'exact')
for row in failed: print('FAIL',row['path'],'expected',row['expected_bytes'],'bytes; got',row['actual_bytes'])
sys.exit(1 if failed else 0)
