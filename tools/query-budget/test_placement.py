import unittest
from placement import assess


class PlacementGate(unittest.TestCase):
    def test_cpu_transformer_is_rejected_even_with_preparation_allowlist(self):
        prep = {'op': 'ios18.gather', 'outputs': ['embedding'], 'preferred': 'MLCPUComputeDevice'}
        transformer = {'op': 'ios18.linear', 'outputs': ['block0'], 'preferred': 'MLCPUComputeDevice'}
        self.assertEqual(assess([prep, transformer], [prep]), [transformer])

    def test_unknown_device_is_not_treated_as_ane(self):
        unknown = {'op': 'ios18.matmul', 'outputs': ['attention'], 'preferred': None}
        self.assertEqual(assess([unknown]), [unknown])

    def test_constants_and_neural_engine_operations_pass(self):
        rows = [{'op': name, 'outputs': [], 'preferred': device} for name, device in [
            ('const', None), ('constexpr_lut_to_dense', None), ('ios18.linear', 'MLNeuralEngineComputeDevice')]]
        self.assertEqual(assess(rows), [])

    def test_allowlist_is_bound_to_output_identity(self):
        allowed = {'op': 'ios18.layer_norm', 'outputs': ['embedding_norm'], 'preferred': 'MLCPUComputeDevice'}
        wrong = dict(allowed, outputs=['transformer_norm'])
        self.assertEqual(assess([wrong], [allowed]), [wrong])


if __name__ == '__main__':
    unittest.main()
