from pathlib import Path
import subprocess,json,hashlib,time
root=Path('/Users/aghatage/Documents/code/zeppelin-embed-wt-ze-124')
evidence=Path('/tmp/ze-124/mutants');evidence.mkdir(exist_ok=True)
merge=root/'crates/zeppelin-embed/src/property_graph/storage/adjacency/merge.rs'
wal=root/'crates/zeppelin-embed/src/property_graph/wal/codec.rs'
original={p:p.read_bytes() for p in [merge,wal]}
cases=[
 ('ignore-delete',merge,'winner.action == Action::Insert','true','-p zeppelin-embed-workspace-tests --test adversarial_tests -E test(property_graph_adjacency_probe_checks_edges_faults_and_comparator_controls)'),
 ('ignore-neighbor',merge,'compare(old.edge.neighbor, head.edge.neighbor, c)? != Ordering::Equal','false','-p zeppelin-embed --test graph_adjacency -E test(adjacency_all_sequences_keep_neighbor_even_behind_delete)'),
 ('narrow-id',merge,'compare(head.edge.rel, id, c)?','compare(head.edge.rel.get() as u64, RelId::get(id) as u64, c)?','-p zeppelin-embed --test graph_adjacency -E test(adjacency_merge_comparisons_keep_high_bits_between_runs)'),
 ('skip-final-control',merge,'step(c, Work::Finish)?;','// Deliberate mutant: final control bypass.','-p zeppelin-embed --test graph_adjacency -E test(adjacency_every_actual_work_checkpoint_and_final_completion_can_refuse)'),
 ('split-early',merge,'needed == MAX_BASE_ENTRIES','needed == MAX_BASE_ENTRIES - 1','-p zeppelin-embed --test graph_adjacency -E test(adjacency_split_4097_and_full_6144_output_keep_exact_intervals)'),
 ('wal-tag-refusal',wal,'        13 => Ok(AdjacencyBase),\n        14 => Ok(AdjacencyDelta),\n','','-p zeppelin-embed --test graph_adjacency -E test(adjacency_wal_reference_tags_are_append_only_through_public_decode)'),
]
records=[]
try:
 for name,path,before,after,args in cases:
  data=original[path].decode();assert data.count(before)==1,(name,data.count(before))
  path.write_text(data.replace(before,after,1))
  argv=['cargo','nextest','run']+args.split()+['--test-threads','4','--no-fail-fast']
  with (evidence/(name+'.log')).open('w') as log:
   done=subprocess.run(argv,cwd=root,stdout=log,stderr=subprocess.STDOUT)
  path.write_bytes(original[path])
  record={'name':name,'command':argv,'exit':done.returncode,'source_restored_sha256':hashlib.sha256(path.read_bytes()).hexdigest()};records.append(record)
  (evidence/'results.json').write_text(json.dumps(records,indent=2)+'\n')
  print(name,done.returncode,flush=True)
  if done.returncode!=100:raise RuntimeError('mutant did not produce intended runtime test failure')
finally:
 for path,data in original.items():path.write_bytes(data)
 (evidence/'restoration.json').write_text(json.dumps({str(p.relative_to(root)):{'before':hashlib.sha256(v).hexdigest(),'after':hashlib.sha256(p.read_bytes()).hexdigest()} for p,v in original.items()},indent=2)+'\n')
