"""Local compatibility entrypoint; install at tracker/planning/validate-backlog.py."""
from pathlib import Path
import runpy

root = Path(__file__).resolve().parents[2]
runpy.run_path(str(root / "scripts/planning/validate-backlog.py"), run_name="__main__")
