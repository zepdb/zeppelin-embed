from pathlib import Path
import subprocess,hashlib,json,time,datetime
root=Path.cwd(); model=Path('tests/adversarial-oracle/src/graph_directory.rs'); compare=Path('tests/adversarial-oracle/src/graph_directory/compare.rs')
cases=[
('narrowed-id-comparison',compare,'&expected.incarnation,\n        &observed.incarnation,','&(expected.incarnation.kind, expected.incarnation.id as u64),\n        &(observed.incarnation.kind, observed.incarnation.id as u64),','graph_directory_comparator_detects_narrowed_ids_and_missing_dead_fences'),
('ignored-dead-fence',compare,'sequence("fences", &expected.fences, &observed.fences, fence)?;','sequence("fences", &expected.fences, &expected.fences, fence)?;','graph_directory_comparator_detects_narrowed_ids_and_missing_dead_fences'),
('ignored-label-membership',compare,'sequence("labels", &expected.labels, &observed.labels, field)?;','sequence("labels", &expected.labels, &expected.labels, field)?;','graph_directory_comparator_detects_omitted_extra_and_reordered_membership'),
('ignored-type-membership',compare,'sequence("types", &expected.types, &observed.types, field)?;','sequence("types", &expected.types, &expected.types, field)?;','graph_directory_comparator_detects_omitted_extra_and_reordered_membership'),
('ignored-canonical-bits',compare,'&expected.image.canonical,\n        &observed.image.canonical,','&expected.image.canonical,\n        &expected.image.canonical,','graph_directory_comparator_detects_canonical_bits_and_every_provenance_field'),
('ignored-original-generation',compare,'&expected.original_generation,\n        &observed.original_generation,','&expected.original_generation,\n        &expected.original_generation,','graph_directory_comparator_detects_canonical_bits_and_every_provenance_field'),
('observed-old-root-as-expectation',model,'compare::check(&self.observation(), observed)','compare::check(observed, observed)','graph_directory_comparator_rejects_wrong_old_root_and_inventory_as_liveness'),
('ignored-unkeyed-tombstone',compare,'&expected.node_tombstones,\n        &observed.node_tombstones,','&expected.node_tombstones,\n        &expected.node_tombstones,','graph_directory_unkeyed_tombstone_comparator_checks_complete_deletion_provenance'),
('disabled-image-budget',model,'image.canonical.len() > limits.max_image_bytes','image.canonical.len() > usize::MAX','graph_directory_rejects_limits_without_partial_logical_changes'),
('wrong-key-tuple-order',model,'pub namespace: Vec<u8>,\n    pub key: Vec<u8>,','pub key: Vec<u8>,\n    pub namespace: Vec<u8>,','graph_directory_key_ranges_are_byte_exact_and_capacity_bounded'),
('retired-identity-reuse',model,'self.allocated.contains(&operation.incarnation)','self.records.contains_key(&operation.incarnation)','graph_directory_snapshot_reads_filter_both_endpoints_before_capacity'),
]
result=[]; dest=Path('/tmp/ze-121-evidence'); originals={p:p.read_bytes() for p in [model,compare]}
for name,path,before,after,test in cases:
 original=path.read_bytes(); text=original.decode(); assert text.count(before)==1,(name,text.count(before))
 cmd=['cargo','nextest','run','-p','zeppelin-embed-adversarial-oracle','--test','graph_directory','-E',f'test(={test})']
 started=datetime.datetime.now(datetime.timezone.utc).isoformat(); tick=time.monotonic()
 try:
  path.write_text(text.replace(before,after))
  proc=subprocess.run(cmd,stdout=subprocess.PIPE,stderr=subprocess.STDOUT)
  (dest/f'mutant-{name}.log').write_bytes(proc.stdout)
  row={'name':name,'source':str(path),'before':before,'after':after,'test':test,'command':cmd,'started_utc':started,'elapsed_seconds':time.monotonic()-tick,'exit':proc.returncode,'original_sha256':hashlib.sha256(original).hexdigest(),'mutant_sha256':hashlib.sha256(path.read_bytes()).hexdigest()}
  assert proc.returncode==100,(name,proc.returncode,proc.stdout.decode())
  assert f'test {test} ... FAILED'.encode() in proc.stdout,name
 finally:
  path.write_bytes(original)
 row['restored_sha256']=hashlib.sha256(path.read_bytes()).hexdigest(); assert row['restored_sha256']==row['original_sha256']
 result.append(row); (dest/'mutation-results.json').write_text(json.dumps(result,indent=2)+'\n'); print(name,'exit100, exact restoration',flush=True)
assert all(p.read_bytes()==b for p,b in originals.items())
print('All',len(result),'mutants fired at named runtime assertions and sources restored.')
