"""Exporter contracts; the real export needs the offline clean71 pack and macOS."""
import json
import os
from pathlib import Path
import runpy
import subprocess
import sys
import tempfile
import unittest

TOOL = Path(__file__).with_name("ze-model")
EXPORT = runpy.run_path(str(TOOL))


class CoreMlExport(unittest.TestCase):
    def test_default_query_sequence_is_fixed_96(self):
        args = EXPORT["parser"]().parse_args([
            "coreml-export", "--query", "query", "--output", "query.mlpackage"])
        self.assertEqual(args.tokens, 96)

    @unittest.skipUnless(os.environ.get("ZE497_PACK") and os.environ.get("ZE497_EXPORT_DIR"),
                         "requires clean71 pack, CoreML compiler and writable export directory")
    def test_clean71_export_compiles_fp16_program_with_int32_inputs(self):
        import coremltools as ct
        Path(os.environ["ZE497_EXPORT_DIR"]).mkdir(parents=True, exist_ok=True)
        directory = tempfile.TemporaryDirectory(dir=os.environ["ZE497_EXPORT_DIR"])
        self.addCleanup(directory.cleanup)
        output = Path(directory.name) / "clean71-fp16-s96.mlpackage"
        subprocess.run([sys.executable, str(TOOL), "coreml-export", "--query",
                        str(Path(os.environ["ZE497_PACK"]) / "query"),
                        "--output", str(output)], check=True)
        self.assertTrue(output.with_suffix(".mlmodelc").is_dir())
        metadata = json.loads(output.with_suffix(".json").read_text())
        self.assertEqual(metadata["tokens"], 96)
        self.assertEqual(metadata["compute_units"], "CPU_AND_NE")
        self.assertEqual(metadata["precision"], "FLOAT16")
        self.assertIn("torch", metadata)
        self.assertIn("coremltools", metadata)
        spec = ct.models.MLModel(str(output), skip_model_load=True).get_spec()
        self.assertEqual(spec.WhichOneof("Type"), "mlProgram")
        for feature in spec.description.input:
            self.assertIn(feature.name, ("input_ids", "attention_mask"))
            self.assertIsNone(feature.type.multiArrayType.WhichOneof("ShapeFlexibility"))
            self.assertEqual(list(feature.type.multiArrayType.shape), [1, 96])
            self.assertEqual(feature.type.multiArrayType.dataType, ct.proto.FeatureTypes_pb2.ArrayFeatureType.INT32)
        self.assertEqual(spec.description.output[0].name, "embedding")
        self.assertEqual(list(spec.description.output[0].type.multiArrayType.shape), [1, 768])
        self.assertEqual(spec.description.output[0].type.multiArrayType.dataType,
                         ct.proto.FeatureTypes_pb2.ArrayFeatureType.FLOAT16)


if __name__ == "__main__":
    unittest.main()
