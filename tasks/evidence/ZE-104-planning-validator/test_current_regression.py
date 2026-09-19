import subprocess
import unittest

class PlanningValidatorRegression(unittest.TestCase):
    def test_current_backlog_accepts_normal_progress(self):
        result = subprocess.run(["python3", "scripts/planning/validate-backlog.py"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)

if __name__ == "__main__":
    unittest.main()
