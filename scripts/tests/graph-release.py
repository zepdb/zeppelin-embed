#!/usr/bin/env python3
"""Synthetic evidence tests: GREEN qualifies the checker, never a release."""
import hashlib
import importlib.util
import json
import os
import shlex
import subprocess
import sys
from pathlib import Path
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'release/qualify-graph.py'


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(SCRIPT.is_file(), 'missing ZE-78 release-evidence gate')
        spec = importlib.util.spec_from_file_location('gate', SCRIPT)
        self.g = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.g)
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.identity = dict(source='a' * 40, build='b' * 64)
        self.m = self.g.template(self.identity)
        self.binary = self.root / 'artifact.a'
        self.binary.write_bytes(b'fixture artifact bytes')
        self.size_raw = self.root / 'size.txt'
        self.size_raw.write_text('Section (__TEXT,__text) 1000\n')
        for name, cell in self.m['cells'].items():
            cell.update(state='qualified', exit_status=0, command=['fixture-producer'],
                        target='aarch64-apple-darwin', features=['graph-cypher'],
                        fixture='c' * 64, seed=0, toolchain='fixture-rust',
                        observations=self.example_observations(name))
            raw = self.root / (name.replace('/', '_') + '.json')
            raw.write_text(json.dumps(cell['observations']))
            cell['report'] = self.ref(raw)
            cell['artifacts'] = [self.ref(raw), self.ref(self.binary)]
            cell['log'] = self.ref(raw)

    def example_observations(self, name):
        obs = {'pass': True, 'exit_status': 0}
        if name == 'coverage':
            obs.update(crates={crate: dict(count=100, covered=95) for crate in self.g.CRATES},
                       exclusions=self.g.EXCLUSIONS)
        elif name == 'shipping':
            obs['artifacts'] = [dict(id=key, features=features, exports_checked=True,
                headers_checked=True, default_graph_absent=True, target='fixture', sha256=self.ref(self.binary)['sha256'])
                for key, features in self.g.SHIPPING.items()]
        elif name.startswith('footprint/'):
            features = self.g.SHIPPING[name.split('/')[1]]
            obs.update(section_bytes=1000, physical_bytes=self.binary.stat().st_size,
                       raw_size=str(self.size_raw), raw_sha256=self.ref(self.size_raw)['sha256'], sha256=self.ref(self.binary)['sha256'], features=features,
                       reachability=['structured', 'cypher'] if features else ['legacy'])
        elif name.startswith('installed/'):
            obs.update(executed=['batch', 'structured', 'get', 'cypher'],
                       artifact_kind='graph-cypher', checksum_verified=True,
                       legacy_substitution_rejected=True)
        elif name == 'runtime/macos14-arm64':
            obs.update(architecture='arm64', os_major=14,
                       executed=['rust', 'c-static', 'c-dylib', 'swift'])
        elif name == 'faults':
            obs['pairs'] = [dict(site=site, fired=1, seed=0, control_seed=0,
                fault_pass=True, control_pass=True, comparator='independent',
                negative_control_rejected=True) for site in self.g.FAULTS]
        elif name.startswith('resources/'):
            obs.update(peak_bytes=1000, readers=4, writers=1, lying_counter_rejected=True,
                       ownership_released=True, boundaries={key: dict(cap=value,
                       at_cap=True, one_over_rejected=True) for key, value in self.g.LIMITS.items()})
            for key in ('canonical_live_bytes', 'canonical_retired_bytes', 'fence_bytes',
                        'fence_entries', 'mapped_bytes', 'mapped_resident_bytes', 'wal_bytes',
                        'disk_bytes', 'phys_footprint', 'caller_bytes', 'retained_result_bytes'):
                obs[key] = 0
        elif name.startswith('workload/'):
            obs.update(independent_oracle=True, full_domain=True,
                repetitions=[dict(failed=False, tainted=False, monitor_complete=True,
                digests_stable=True, p95_ms=10, peak_bytes=1000, samples=1000, samples_ms=[10]*1000,
                duration_seconds=1, monitor=[dict(second=i, thermal='nominal', power='normal',
                    ac=True, qos='user', nonbenchmark_cpu_percent=0) for i in range(2)]) for _ in range(5)],
                recall_at_20=.99, exact_scores_match=True, history_multipliers=[1, 5, 10],
                stress_multiplier=10, hops=[1, 2, 4, 8, 16], limit_failures_separate=True,
                readers=4, writers=1, synchronized_start=True, baseline_ingestion_batches=9204,
                checkpoint_stalls_included=True, tail_thresholds=['64-envelopes', '16-MiB'],
                read_only_zero_writes=True)
        elif name == 'durable-write-history':
            obs.update(ingestion_batches=9204, checkpoint_reopen=True, unchanged_state_rejection=True)
        elif name.startswith('parity/') or name == 'profile':
            obs.update(common_fixture=True, full_width_ids=True, unsupported_rejections=True,
                       side_effects_match=True)
            if name == 'profile':
                obs['scenarios'] = {key: dict(value, state='executed-pass' if value['support_disposition'] == 'supported' else 'unsupported-rejected')
                                    for key, value in self.g.profile_scenarios().items()}
        return obs

    def ref(self, path):
        return dict(path=str(path), sha256=hashlib.sha256(path.read_bytes()).hexdigest())

    def errors(self):
        return self.g.qualify(self.m, self.root, self.identity, {})

    def reject(self, text):
        self.assertTrue(any(text in error for error in self.errors()), self.errors())

    def change(self, name, key, value):
        self.m['cells'][name]['observations'][key] = value
        raw = Path(self.m['cells'][name]['report']['path'])
        raw.write_text(json.dumps(self.m['cells'][name]['observations']))
        self.m['cells'][name].update(report=self.ref(raw), artifacts=[self.ref(raw), self.ref(self.binary)], log=self.ref(raw))

    def test_missing_required_cell_fails(self):
        del self.m['cells']['parity/rust']
        self.reject('parity/rust')

    def test_omitted_crate_fails(self):
        self.change('coverage', 'crates', {k: v for k, v in self.example_observations('coverage')['crates'].items()
                                          if k != 'zeppelin-embed-cypher'})
        self.reject('zeppelin-embed-cypher')
        self.change('coverage', 'crates', self.example_observations('coverage')['crates'])
        crates = self.example_observations('coverage')['crates']
        crates['zeppelin-embed-cypher']['covered'] = 89
        self.change('coverage', 'crates', crates)
        self.reject('90%')

    def test_omitted_shipping_feature_fails(self):
        self.change('shipping', 'artifacts', self.example_observations('shipping')['artifacts'][:-1])
        self.reject('shipping')

    def test_failed_or_tampered_evidence_fails(self):
        self.m['cells']['parity/rust']['exit_status'] = 1
        self.reject('exit')
        self.m['cells']['parity/rust']['exit_status'] = 0
        self.change('parity/rust', 'pass', False)
        self.reject('required True')
        self.change('parity/rust', 'pass', True)
        Path(self.m['cells']['parity/rust']['log']['path']).write_text('tampered')
        self.reject('digest')

    def test_stale_evidence_fails(self):
        self.m['cells']['parity/rust']['identity']['build'] = 'd' * 64
        self.reject('stale')

    def test_unmeasured_resources_fail(self):
        self.change('resources/rust', 'peak_bytes', None)
        self.reject('peak_bytes')

    def test_incomplete_control_fails(self):
        self.change('faults', 'pairs', [dict(site='crash', fired=1, seed=0, fault_pass=True)])
        self.reject('control')

    def test_owner_exclusion_requires_decision(self):
        self.m['cells']['parity/rust'].update(state='excluded', decision='ZE-owner-1', reason='explicit scope')
        self.reject('owner decision')

    def test_unrecorded_coverage_exclusion_fails(self):
        self.change('coverage', 'exclusions', [])
        self.reject('exclusions')

    def test_shipping_structured_execution_required(self):
        self.change('installed/c-static', 'executed', ['batch', 'cypher'])
        self.reject('structured')
        self.change('installed/c-static', 'artifact_kind', 'legacy')
        self.reject('graph-cypher')

    def test_llvm_reports_keep_each_crate_and_exclusions(self):
        paths = ['crates/zeppelin-embed/src/lib.rs',
                 'crates/zeppelin-embed-cypher/src/lib.rs',
                 'crates/zeppelin-embed-bench/src/bin/excluded.rs']
        raw = self.root / 'llvm.json'
        raw.write_text(json.dumps({'data': [{'files': [dict(filename=str(self.g.ROOT / path),
            summary={'lines': dict(count=100, covered=89 if 'cypher' in path else 100)})
            for path in paths]}]}))
        report = self.g.coverage_report([raw])
        self.assertEqual(report['crates']['zeppelin-embed']['covered'], 100)
        self.assertEqual(report['crates']['zeppelin-embed-bench']['count'], 0)
        errors = self.g.check_crate_lines(report)
        self.assertTrue(any('cypher' in e and '90%' in e for e in errors))
        self.assertTrue(any('text' in e and 'unmeasured' in e for e in errors))

    def test_artifact_adapter_enforces_owner_size_budgets(self):
        for artifact, features in self.g.SHIPPING.items():
            if artifact in ('graph-core-arm64', 'text-arm64', 'legacy-consumer'):
                with self.subTest(artifact=artifact):
                    self.assertIsNone(self.g.policy('footprint/' + artifact)['section_bytes_max'])
                continue
            sizes = ((12288*1024, True), (12288*1024+1, False)) if features else (
                (5285*1024, True), (5632*1024, True), (5632*1024+1, False))
            for size, passes in sizes:
                with self.subTest(artifact=artifact, section_bytes=size):
                    report = self.example_observations('footprint/' + artifact)
                    report['section_bytes'] = size
                    errors = self.g.observations_errors('footprint/' + artifact, report)
                    if passes:
                        self.assertEqual(errors, [])
                    else:
                        self.assertTrue(any('section_bytes' in error for error in errors))

    def test_installed_adapter_rejects_batch_cypher_only(self):
        with self.assertRaisesRegex(ValueError, 'ZE-71/ZE-278'):
            self.g.installed_report({'structured_query': 'blocked'}, 'swift')

    def test_recorded_owner_exclusion_passes(self):
        record = self.root / 'owner.txt'
        record.write_text('Owner: exclude this exact cell for this release')
        self.m['cells']['parity/rust'].update(state='excluded', decision='D1', reason='scope')
        decisions = {'D1': dict(owner='release owner', cells=['parity/rust'], record=self.ref(record))}
        self.assertEqual(self.g.qualify(self.m, self.root, self.identity, decisions), [])

    def test_receipt_schema_rejects_unidentified_fixture(self):
        self.m['cells']['parity/rust']['fixture'] = 'unidentified'
        self.reject('fixture digest')

    def test_workload_report_cannot_hide_slow_samples(self):
        reps = self.example_observations('workload/rust/structured/A/project-evidence')['repetitions']
        for rep in reps:
            rep['samples_ms'] = [300] * 1000
        self.change('workload/rust/structured/A/project-evidence', 'repetitions', reps)
        self.reject('p95')

    def test_workload_missing_or_hot_monitor_fails(self):
        reps = self.example_observations('workload/rust/structured/A/project-evidence')['repetitions']
        for rep in reps:
            rep['monitor'] = []
        self.change('workload/rust/structured/A/project-evidence', 'repetitions', reps)
        self.reject('monitor')

    def test_shipping_artifact_identity_must_match_retained_bytes(self):
        inventory = self.example_observations('shipping')['artifacts']
        inventory[0]['sha256'] = '0' * 64
        self.change('shipping', 'artifacts', inventory)
        self.reject('artifact digest')

    def test_manifest_cannot_omit_workspace_or_frontend_inventory(self):
        self.m['inventory'] = {'workspace': {}}
        self.reject('inventory')

    def test_prebuilt_size_gate_retains_raw_receipt(self):
        source = self.root / 'tiny.c'
        source.write_text('int ze_test_fixture(int n) { return n + 1; }\n')
        obj = self.root / 'tiny.o'
        archive = self.root / 'tiny.a'
        subprocess.run(['clang', '-c', str(source), '-o', str(obj)], check=True, capture_output=True)
        subprocess.run(['ar', 'rcs', str(archive), str(obj)], check=True, capture_output=True)
        script = os.environ.get('ZE78_SIZE_SCRIPT', str(self.g.ROOT / 'scripts/size-budget.sh'))
        env = dict(os.environ, CARGO_TARGET_DIR=str(self.root / 'target'),
                   ZE_GRAPH_SIZE_BUDGET_KB='12288')
        result = subprocess.run(['bash', script, '--graph-archive', str(archive)],
                                env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        reports = list((self.root / 'target/size-budget').glob('*size.txt.json'))
        self.assertEqual(len(reports), 1, 'missing retained raw size receipt')
        report = json.loads(reports[0].read_text())
        self.assertEqual(report['sha256'], self.ref(archive)['sha256'])
        self.assertEqual(report['state'], 'measured')
        self.assertGreater(report['section_bytes'], 0)
        self.assertEqual(report['physical_bytes'], archive.stat().st_size)
        self.assertTrue(Path(report['raw_size']).is_file())
        core = subprocess.run(['bash', script, '--graph-core-archive', str(archive)],
            env=dict(env, ZE_SIZE_BUDGET_KB='0'), capture_output=True, text=True)
        self.assertEqual(core.returncode, 0, core.stderr)
        core_report = json.loads((self.root / 'target/size-budget/graph-core-tiny.a-size.txt.json').read_text())
        self.assertIsNone(core_report['section_bytes_max'])
        result = subprocess.run(['bash', script, '--graph-archive', str(archive)],
            env=dict(env, ZE_GRAPH_SIZE_BUDGET_KB='0'), capture_output=True, text=True)
        self.assertEqual(result.returncode, 1)
        self.assertIn('exceeds budget', result.stderr)

    def test_coverage_script_rejects_hidden_crate(self):
        # Cargo is an external-tool boundary. No tests or coverage campaign run.
        fake_bin = self.root / 'bin'
        fake_bin.mkdir()
        fixture = self.root / 'coverage.json'
        fixture.write_text(json.dumps({'data': [{'files': [dict(
            filename=str(self.g.ROOT / directory / 'src/lib.rs'),
            summary={'lines': dict(count=100, covered=89 if name == 'zeppelin-embed-cypher' else 95)})
            for name, directory in self.g.CRATES.items()]}]}))
        cargo = fake_bin / 'cargo'
        cargo.write_text('#!' + sys.executable + '\nimport pathlib, shutil, sys\n'
            + 'if "--output-path" in sys.argv:\n'
            + '    shutil.copy(' + repr(str(fixture)) + ', sys.argv[sys.argv.index("--output-path")+1])\n')
        cargo.chmod(0o755)
        (fake_bin / 'cargo-llvm-cov').write_text('#!/bin/sh\nexit 0\n')
        (fake_bin / 'cargo-llvm-cov').chmod(0o755)
        script = os.environ.get('ZE78_COVERAGE_SCRIPT', str(self.g.ROOT / 'scripts/coverage.sh'))
        env = dict(os.environ, PATH=str(fake_bin) + os.pathsep + os.environ['PATH'],
                   CARGO_TARGET_DIR=str(self.root / 'target'), CARGO_BUILD_TARGET='aarch64-apple-darwin')
        result = subprocess.run(['bash', script], env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn('zeppelin-embed-cypher', result.stderr)
        self.assertIn('below 90%', result.stderr)

    def test_windows_shipping_archive_rejects_over_budget(self):
        workflow = (self.g.ROOT / '.github/workflows/node.yml').read_text()
        windows = workflow.split('  windows-package:', 1)[1].split('  electron-package:', 1)[0]
        commands = [shlex.split(line.strip()) for line in windows.splitlines()
                    if line.strip().startswith('bash scripts/size-budget.sh --graph-archive ')]
        self.assertEqual(len(commands), 1, 'Windows shipping archive has no linked-section gate')
        command = commands[0]
        self.assertEqual(command[-1],
            'target/native-archives/x86_64-pc-windows-msvc/zeppelin_embed_ffi.lib')
        source = self.root / 'over-budget.c'
        source.write_text('char ze_size_fixture[12288 * 1024 + 1] = {1};\n')
        obj = self.root / 'over-budget.o'
        archive = self.root / 'over-budget.a'
        subprocess.run(['clang', '-c', str(source), '-o', str(obj)], check=True, capture_output=True)
        subprocess.run(['ar', 'rcs', str(archive), str(obj)], check=True, capture_output=True)
        command[-1] = str(archive)
        env = dict(os.environ, CARGO_TARGET_DIR=str(self.root / 'target'),
                   ZE_GRAPH_SIZE_BUDGET_KB='12288')
        result = subprocess.run(command, cwd=self.g.ROOT, env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn('exceeds budget 12288 KB', result.stderr)
        report = json.loads(next((self.root / 'target/size-budget').glob('*size.txt.json')).read_text())
        self.assertGreater(report['section_bytes'], 12288 * 1024)
        self.assertEqual(report['sha256'], self.ref(archive)['sha256'])

    def test_installed_checker_requires_structured_handoff(self):
        spec = importlib.util.spec_from_file_location('installed',
            self.g.ROOT / 'scripts/release/check-installed-graph.py')
        installed = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(installed)
        report = {'reachability': {kind: dict(executed=['batch', 'cypher'],
            artifact_kind='graph-cypher', exit_status=0, resources=True)
            for kind in ('c-static', 'c-dylib', 'swift', 'rust')}}
        with self.assertRaisesRegex(ValueError, 'ZE-71/ZE-278'):
            installed.require_structured_execution(report)
        for receipt in report['reachability'].values():
            receipt['executed'] = ['batch', 'structured', 'get', 'cypher']
        installed.require_structured_execution(report)

    def test_lexical_score_failure_blocks_qualification(self):
        self.change('workload/rust/structured/A/lexical-evidence', 'exact_scores_match', False)
        self.reject('exact_scores_match')

    def test_complete_evidence_passes(self):
        self.assertEqual(self.errors(), [])


if __name__ == '__main__':
    unittest.main()
