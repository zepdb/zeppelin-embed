from pathlib import Path
import hashlib, subprocess, json
root = Path.cwd()
logs = Path('/tmp/ze-113-qualification')
cases = [
 ('report-payoff', 'user_bench.rs', 'scan.p50 - graph.p50,', 'scan.p50 + graph.p50,', 'populated_user_report_keeps_backend_tier_and_missing_measurements_distinct'),
 ('recall-denominator', 'user_bench.rs', '/ exact.len() as f64', '/ (exact.len() + 1) as f64', 'user_statistics_handle_empty_sets_duplicates_and_nearest_rank_boundaries'),
 ('ignored-seed', 'user_bench.rs', 'let mut state = seed;', 'let mut state = seed & 0;', 'seeded_query_order_is_repeatable_and_contains_every_row_once'),
 ('zero-latency', 'kernel_gate.rs', 'if !ns.is_finite() || ns <= 0.0 {', 'if !ns.is_finite() {', 'kernel_gate_reports_all_invalid_measurements_before_computing_ratios'),
 ('zero-process-observation', 'process_median.rs', '!value.is_finite() || *value <= 0.0', '!value.is_finite() || *value < 0.0', 'process_summary_rejects_invalid_counts_and_values_with_actionable_diagnostics'),
 ('idle-as-measured', 'frontier/roofline.rs', 'return Ok(ComputeCalibrationOutcome::Idle { reasons });', 'return Ok(ComputeCalibrationOutcome::Measured { calibrations: Vec::new(), not_measured: Vec::new() });', 'compute_calibration_stays_idle_on_load_and_rejects_overflow_before_sampling'),
 ('lost-load-cause', 'frontier/roofline.rs', '.map_err(ComputeCalibrationError::Measurement)?;', '.map_err(|_| ComputeCalibrationError::InvalidIterations)?;', 'compute_calibration_honors_a_load_veto_after_warmup_without_publishing_a_rate'),
 ('zero-work-as-idle', 'frontier/roofline.rs', 'return Err(ComputeCalibrationError::InvalidIterations);', 'return Ok(ComputeCalibrationOutcome::Idle { reasons: Vec::new() });', 'attested_compute_calibration_cannot_override_busy_preflight_or_zero_work', 2),
 ('lost-ledger-path', 'frontier/ledger.rs', 'path: path.to_path_buf(),', 'path: PathBuf::new(),', 'ledger_io_failures_name_the_affected_path_and_preserve_observed_history'),
 ('blank-attestation', 'frontier/ledger.rs', '&& (timestamp.trim().is_empty() || machine_identifier.trim().is_empty())', '&& (timestamp.trim().is_empty() && machine_identifier.trim().is_empty())', 'ledger_rejects_blank_attestation_and_corrupt_counter_evidence_before_adoption'),
 ('lost-json-cause', 'frontier/calibration.rs', 'Self::Json(error) => Some(error),', 'Self::Json(_) => None,', 'calibration_loading_retains_io_and_json_causes_through_the_roofline_api'),
 ('odd-median-offset', 'frontier/measure.rs', 'samples[middle]\n', 'samples[middle - 1]\n', 'measurement_odd_runs_use_the_middle_observation'),
 ('nonfinite-variance', 'frontier/measure.rs', '!rsd_percent.is_finite() || rsd_percent > config.maximum_rsd_percent', 'rsd_percent > config.maximum_rsd_percent', 'measurement_rejects_nonfinite_variance_from_finite_samples'),
]
results=[]
for case in cases:
 label, rel, old, new, test, *nth=case
 path=root/'crates/zeppelin-embed-bench/src'/rel
 original=path.read_bytes()
 source=original.decode()
 occurrence=nth[0] if nth else 1
 start=0
 for _ in range(occurrence):
  index=source.index(old,start); start=index+len(old)
 mutated=source[:index]+new+source[index+len(old):]
 command=['cargo','test','-p','zeppelin-embed-bench','--test','frontier',test]
 try:
  path.write_text(mutated)
  with (logs/f'mutation-{label}.log').open('w') as log:
   outcome=subprocess.run(command,stdout=log,stderr=subprocess.STDOUT)
  text=(logs/f'mutation-{label}.log').read_text()
  assert outcome.returncode==101 and f'{test} ... FAILED' in text, (label,outcome.returncode,text[-3000:])
 finally:
  path.write_bytes(original)
  assert path.read_bytes()==original
 results.append({'mutation':label,'source':str(path.relative_to(root)),'test':test,'exit':outcome.returncode,'restored_sha256':hashlib.sha256(original).hexdigest(),'command':command})
 print(label, 'RED exit',outcome.returncode,'restored',flush=True)
(logs/'mutation-results.json').write_text(json.dumps(results,indent=2)+'\n')
