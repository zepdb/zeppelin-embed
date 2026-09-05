"""Reject unapproved CPU placement; compiler intent is not a hardware trace."""
import argparse
import collections
import json
from pathlib import Path


def assess(operations, allowed_cpu=()):
    allowed = {(row['op'], tuple(row['outputs'])) for row in allowed_cpu}
    violations = []
    for row in operations:
        op = row['op'].split('.')[-1]
        if op == 'const' or op.startswith('constexpr_'):
            continue
        if row['preferred'] == 'MLNeuralEngineComputeDevice':
            continue
        if row['preferred'] == 'MLCPUComputeDevice' and (row['op'], tuple(row['outputs'])) in allowed:
            continue
        violations.append(row)
    return violations


def inspect(model):
    import coremltools as ct
    from coremltools.models.compute_plan import MLComputePlan
    plan = MLComputePlan.load_from_path(str(model), compute_units=ct.ComputeUnit.CPU_AND_NE)
    rows = []
    def walk(block):
        for op in block.operations:
            usage = plan.get_compute_device_usage_for_mlprogram_operation(op)
            rows.append({'op': op.operator_name, 'outputs': [x.name for x in op.outputs],
                         'preferred': type(usage.preferred_compute_device).__name__ if usage else None})
            for child in op.blocks:
                walk(child)
    walk(plan.model_structure.program.functions['main'].block)
    return rows


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('model', type=Path)
    parser.add_argument('output', type=Path)
    parser.add_argument('--baseline-preparation', type=Path,
                        help='Explicit legacy CPU prep allowlist; never means strict ANE-only passed')
    args = parser.parse_args()
    allowed = json.loads(args.baseline_preparation.read_text()) if args.baseline_preparation else []
    rows = inspect(args.model)
    rejected = assess(rows, allowed)
    report = {'model': str(args.model), 'meaning': 'anticipated compiler placement, not execution trace',
              'strict_ane_only_pass': not assess(rows), 'declared_gate_pass': not rejected,
              'allowed_cpu': allowed, 'violations': rejected, 'operations': rows,
              'counts': dict(collections.Counter(f"{row['op']} / {row['preferred']}" for row in rows))}
    with args.output.open('x') as output:
        json.dump(report, output, indent=2)
    if rejected:
        raise SystemExit(f'REJECTED: {len(rejected)} unapproved operations')


if __name__ == '__main__':
    main()
