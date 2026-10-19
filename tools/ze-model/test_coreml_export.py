"""Exporter contracts; the real export needs the offline clean71 pack and macOS."""
import json
import os
from pathlib import Path
import runpy
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import MagicMock, patch

TOOL = Path(__file__).with_name("ze-model")
EXPORT = runpy.run_path(str(TOOL))


class CoreMlExport(unittest.TestCase):
    def test_default_query_sequence_is_fixed_96(self):
        args = EXPORT["parser"]().parse_args([
            "coreml-export", "--query", "query", "--output", "query.mlpackage"])
        self.assertEqual(args.tokens, 96)
        self.assertEqual(args.precision, "fp16")

    def test_int8_export_uses_per_channel_symmetric_weight_quantization(self):
        import coremltools as ct

        with tempfile.TemporaryDirectory() as directory:
            output = (Path(directory) / "query.mlpackage").resolve()
            args = EXPORT["parser"]().parse_args([
                "coreml-export", "--query", "query", "--output", str(output),
                "--precision", "int8"])
            converted, quantized = MagicMock(), MagicMock()

            def compile_model(command, check):
                self.assertTrue(check)
                self.assertEqual(command[:3], ["xcrun", "coremlcompiler", "compile"])
                output.with_suffix(".mlmodelc").mkdir()

            with (patch.dict(EXPORT["coreml_export"].__globals__,
                             _coreml_query_module=MagicMock()),
                  patch("torch.jit.trace"),
                  patch.object(ct, "convert", return_value=converted) as convert,
                  patch.object(ct.optimize.coreml, "linear_quantize_weights",
                               return_value=quantized) as quantize,
                  patch("subprocess.run", side_effect=compile_model)):
                EXPORT["coreml_export"](args)
            self.assertEqual(convert.call_args.kwargs["compute_precision"], ct.precision.FLOAT16)
            self.assertEqual(convert.call_args.kwargs["compute_units"], ct.ComputeUnit.CPU_AND_NE)
            self.assertEqual(convert.call_args.kwargs["outputs"][0].dtype,
                             ct.converters.mil.mil.types.fp16)
            quantize.assert_called_once()
            self.assertIs(quantize.call_args.args[0], converted)
            config = quantize.call_args.kwargs["config"].global_config
            self.assertEqual(config.mode, "LINEAR_SYMMETRIC")
            self.assertEqual(config.dtype, ct.converters.mil.mil.types.int8)
            self.assertEqual(config.granularity.name, "PER_CHANNEL")
            self.assertEqual(config.weight_threshold,
                             ct.optimize.coreml.OpLinearQuantizerConfig().weight_threshold)
            quantized.save.assert_called_once_with(str(output))
            self.assertEqual(quantized.user_defined_metadata.update.call_args.args[0]["precision"],
                             "INT8")
            receipt = json.loads(output.with_suffix(".json").read_text())
            self.assertEqual(receipt["precision"], "INT8")
            self.assertEqual(receipt["tokens"], 96)

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
