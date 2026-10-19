"""Report a fixed program's CPU_AND_NE compute plan. MUST run outside the sandbox.

The sandbox cannot see usable Neural Engine placement; its CPU-only preference
is not hardware qualification. The .mlpackage sibling supplies the fixed shape.
"""
import argparse
from collections import Counter, defaultdict
import json
from pathlib import Path

import coremltools as ct
from coremltools.models.compute_device import MLComputeDevice

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("model", type=Path, help="compiled .mlmodelc with sibling .mlpackage")
args = parser.parse_args()
spec = ct.models.MLModel(str(args.model.with_suffix(".mlpackage")), skip_model_load=True).get_spec()
constraints = [feature.type.multiArrayType for feature in spec.description.input]
if any(c.WhichOneof("ShapeFlexibility") is not None for c in constraints):
    raise ValueError("only fixed input shapes are supported")
shapes = [list(c.shape) for c in constraints]
if (len(shapes) != 2 or shapes[0] != shapes[1] or len(shapes[0]) != 2
        or shapes[0][0] != 1 or shapes[0][1] <= 0):
    raise ValueError("input_ids and attention_mask must have matching fixed [1, sequence] shapes")
plan = ct.models.compute_plan.MLComputePlan.load_from_path(
    str(args.model), compute_units=ct.ComputeUnit.CPU_AND_NE)
if plan.model_structure.program is None:
    raise ValueError("expected an ML program compute plan")
report = {"model": str(args.model), "compute_units": "CPU_AND_NE",
          "coremltools": ct.__version__,
          "available_devices": [type(d).__name__ for d in MLComputeDevice.get_all_compute_devices()],
          "tokens": shapes[0][1]}
counts = Counter()
operators = defaultdict(Counter)

def visit(block):
    for operation in block.operations:
        usage = plan.get_compute_device_usage_for_mlprogram_operation(operation)
        preferred = type(usage.preferred_compute_device).__name__ if usage else "no_device_usage"
        counts[preferred] += 1
        operators[operation.operator_name][preferred] += 1
        for child in operation.blocks:
            visit(child)

for function in plan.model_structure.program.functions.values():
    visit(function.block)
report.update(preferred_operation_counts=dict(counts), preferred_by_operator=dict(operators))
print(json.dumps(report, indent=2))
