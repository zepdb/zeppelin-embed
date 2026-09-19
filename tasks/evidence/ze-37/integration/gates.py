import subprocess,json
from pathlib import Path
for r in json.loads(Path("/tmp/ze-37-integration/gates.json").read_text()):
 subprocess.run(["python3","/tmp/ze-37-integration/run.py",r["name"],*r["command"]],check=True)
