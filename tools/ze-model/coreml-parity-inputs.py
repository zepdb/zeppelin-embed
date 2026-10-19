"""Send reference token ids + existing FP32 goldens to the Rust parity test.

Binary stdout: u32 rows, then (u32 tokens, i32[tokens] ids, f32[768] golden).
The pack's reference uses `query: `, strips the prefixed text, and explicitly
truncates to 96 for its goldens. Keep overlength rows so Rust proves rejection.
"""
import json
import os
from pathlib import Path
import struct
import sys

os.environ["HF_HUB_OFFLINE"] = "1"
os.environ["TRANSFORMERS_OFFLINE"] = "1"
import numpy as np
from transformers import AutoTokenizer

pack = Path(sys.argv[1])
texts = json.loads((pack / "fixtures/texts.json").read_text())["query"]
with np.load(pack / "fixtures/goldens.npz", allow_pickle=False) as archive:
    goldens = archive["query"]
assert goldens.shape == (len(texts), 768) and np.isfinite(goldens).all()
tokenizer = AutoTokenizer.from_pretrained(str(pack / "query"), local_files_only=True)
ids = tokenizer([("query: " + text).strip() for text in texts],
                truncation=True, max_length=96)["input_ids"]
out = sys.stdout.buffer
out.write(struct.pack("<I", len(ids)))
for tokens, golden in zip(ids, goldens):
    out.write(struct.pack("<I", len(tokens)))
    out.write(np.asarray(tokens, dtype="<i4").tobytes())
    out.write(golden.astype("<f4").tobytes())
