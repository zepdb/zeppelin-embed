from pathlib import Path
import subprocess,hashlib,json
root=Path.cwd();out=Path('/tmp/ze-73-evidence');results=[]
model='tests/adversarial-oracle/src/graph_fixture/model.rs';query='tests/adversarial-oracle/src/graph_fixture/query.rs';compare='tests/adversarial-oracle/src/graph_fixture.rs';vectors='crates/zeppelin-embed-bench/src/graph_fixture/vectors.rs';files='crates/zeppelin-embed-bench/src/graph_fixture/files.rs'
cases=[
 ('accept-missing-edge',compare,'if sorted(expected) == sorted(observed) {','if true || sorted(expected) == sorted(observed) {','zeppelin-embed-adversarial-oracle','primitive_fixture_comparator_rejects_a_missing_parallel_edge','PG13 missing edge must fire'),
 ('accept-narrowed-id',compare,'nodes.sort();','for n in &mut nodes {n.id=n.id as u64 as u128;} for e in &mut relationships {e.source=e.source as u64 as u128;e.target=e.target as u64 as u128;} nodes.sort();','zeppelin-embed-adversarial-oracle','full_width_identity_and_row_bags_reject_narrowing_and_lost_multiplicity','PG13 narrowed ID must fire'),
 ('resurrect-key',model,'return Err(Rejection::Deleted);','()','zeppelin-embed-adversarial-oracle','independent_key_history_never_resurrects_deleted_keys','assertion `left == right` failed'),
 ('deduplicate-bag',query,'        observed.sort();','        observed.sort(); expected.dedup(); observed.dedup();','zeppelin-embed-adversarial-oracle','full_width_identity_and_row_bags_reject_narrowing_and_lost_multiplicity','PG13 row multiplicity must fire'),
 ('ten-bit-vector',vectors,'words() & 2047','words() & 1023','zeppelin-embed-workspace-tests','normalized_low_11_bit_recipe_matches_independent_axis_byte_goldens','ordered f64 7:1 mixture and final f32 bits'),
 ('renormalize-absent-leg',query,'let fused = weight * v + (1.0 - weight) * l;','let fused = if d.is_none(){l}else if b.is_none(){v}else{weight * v + (1.0 - weight) * l};','zeppelin-embed-adversarial-oracle','hybrid_optional_modalities_keep_query_weights_and_absence_distinct','assertion `left == right` failed'),
 ('skip-file-digest',files,'|| digest(&path)? != file["sha256"].as_str().ok_or("missing file hash")?','|| false && digest(&path)? != file["sha256"].as_str().ok_or("missing file hash")?','zeppelin-embed-workspace-tests','serialized_fixture_is_reproducible_and_rejects_missing_or_modified_files','assertion failed: validate_fixture(dir.path()).is_err()'),
]
for name,file,old,new,package,test,needle in cases:
 p=root/file;before=p.read_bytes();source=before.decode();assert source.count(old)==1,(name,source.count(old));before_sha=hashlib.sha256(before).hexdigest();cmd=['cargo','nextest','run','-p',package,'--test','graph_fixture','-E',f'test(={test})']
 try:
  p.write_text(source.replace(old,new));run=subprocess.run(cmd,stdout=subprocess.PIPE,stderr=subprocess.STDOUT);log=run.stdout.decode();(out/f'mutant-{name}.log').write_text(log)
  record={'name':name,'source':file,'command':cmd,'exit':run.returncode,'assertion_seen':needle in log,'original_sha256':before_sha};print(json.dumps(record),flush=True);assert run.returncode==100 and needle in log,(name,run.returncode,log[-2500:])
 finally:
  p.write_bytes(before);assert hashlib.sha256(p.read_bytes()).hexdigest()==before_sha
 record['restored_sha256']=hashlib.sha256(p.read_bytes()).hexdigest();results.append(record);(out/'mutation-results.json').write_text(json.dumps(results,indent=2)+'\n')
