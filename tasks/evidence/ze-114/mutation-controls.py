from pathlib import Path
import hashlib,json,subprocess
root=Path.cwd()
logs=Path('/tmp/ze-114-qualification')
cases=[
 ('read-only-access','lib.rs','1 => Ok(AccessMode::ReadOnly),','1 => Ok(AccessMode::ReadWrite),','open_rejects_unknown_options_and_read_only_handles_reject_writes'),
 ('epoch-reserved','lib.rs','if request.reserved != 0 {','if request.reserved == 99 {','epoch_identity_validates_tags_and_preserves_runtime_compute_and_os_identity'),
 ('filter-children','lib.rs','if node.children_count != 0 {','if node.children_count == u32::MAX {','filter_grammar_rejects_irrelevant_children_bounds_and_values'),
 ('null-filter-value','lib.rs','0 => return Err(FfiError::invalid("filter values cannot be null")),','0 => PredicateValue::I64(0),','filter_values_reject_null_unknown_tags_wrong_columns_and_invalid_bounds'),
 ('returned-bool','lib.rs','attribute.bool_value = bool_u32(*value);','attribute.bool_value = bool_u32(!*value);','typed_attributes_round_trip_through_get_and_match_exact_filters'),
 ('zero-sized-search-free','lib.rs','return Err(FfiError::invalid(\n                        "zero-sized search result contains an allocation",\n                    ));','return Ok(());','malformed_result_frees_preserve_caller_bytes_and_live_allocations'),
 ('namespace-zero-dimensions','lib.rs','if request.dimensions == 0 {','if request.dimensions == u32::MAX {','namespace_schema_rejects_invalid_columns_flags_and_epoch_geometry'),
 ('error-copy-length','lib.rs','marshal::write_scalar(written, message.len());','marshal::write_scalar(written, message.len() + 1);','error_copy_and_scalar_outputs_reject_invalid_buffers_without_writing_them'),
 ('missing-count-bound','lib.rs','if current.missing_count > current.document_count {','if current.missing_count > current.document_count + 1 {','wrong_result_type_or_missing_count_never_consumes_the_owned_allocation'),
 ('purge-busy-code','error.rs','PurgeError::PurgeInProgress => ZeErrorCode::ZeErrBusy,','PurgeError::PurgeInProgress => ZeErrorCode::ZeErrInvalidArgument,','purge_busy_and_consumed_tokens_are_typed_without_losing_the_first_request'),
 ('maintenance-access-code','error.rs','MaintenanceError::Store(error) => Self::store(error).code,','MaintenanceError::Store(_) => ZeErrorCode::ZeErrInternal,','read_only_maintenance_and_purge_retain_access_mode_errors'),
 ('filtered-cancel-code','error.rs','FilteredSearchError::Query(error) => Self::query(error).code,','FilteredSearchError::Query(_) => ZeErrorCode::ZeErrInternal,','filtered_search_reports_cancelled_and_invalid_vector_errors_with_empty_output'),
 ('empty-ingest-code','lib.rs','ZeErrorCode::ZeErrEmptyBatch,\n                        "ingest batch is empty",','ZeErrorCode::ZeErrInvalidArgument,\n                        "ingest batch is empty",','empty_mutation_batches_and_zero_dimensions_never_advance_generation'),
 ('scan-order-admission','lib.rs','_ => Err(FfiError::invalid("scan order discriminant is out of range")),','_ => Ok(ScanOrder::Storage),','scan_cursor_and_timestamp_flags_reject_ambiguous_requests'),
 ('search-reserved','lib.rs','if request.reserved != 0 {','if request.reserved == 99 {','search_options_reject_reserved_bits_conflicting_controls_and_unknown_profiles',1),
]
# The first reserved guard is search; epoch's reserved guard is second.
cases[1]=(*cases[1],2)
results=[]
for case in cases:
 label,rel,old,new,test,*nth=case
 path=root/'crates/zeppelin-embed-ffi/src'/rel
 original=path.read_bytes();source=original.decode();start=0
 for _ in range(nth[0] if nth else 1):
  index=source.index(old,start);start=index+len(old)
 command=['cargo','test','-p','zeppelin-embed-ffi','--features','abi-panic-probe','--test','ffi_boundary_validation',test]
 try:
  path.write_text(source[:index]+new+source[index+len(old):])
  with (logs/f'mutation-{label}.log').open('w') as log:
   outcome=subprocess.run(command,stdout=log,stderr=subprocess.STDOUT)
  text=(logs/f'mutation-{label}.log').read_text()
  assert outcome.returncode==101 and f'{test} ... FAILED' in text,(label,outcome.returncode,text[-3000:])
 finally:
  path.write_bytes(original)
  assert path.read_bytes()==original
 results.append({'mutation':label,'source':str(path.relative_to(root)),'test':test,'exit':outcome.returncode,'restored_sha256':hashlib.sha256(original).hexdigest(),'command':command})
 print(label,'RED exit101 restored',flush=True)
(logs/'mutation-results.json').write_text(json.dumps(results,indent=2)+'\n')
