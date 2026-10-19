"""Stream the pack's document token IDs and FP32 goldens for ZE-498 parity.

Wire format: u32 rows, then (u32 token count, i32[token count], f32[768]).
No model execution or regenerated reference vectors.
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
texts = json.loads((pack / "fixtures/texts.json").read_text())["document"]
with np.load(pack / "fixtures/goldens.npz", allow_pickle=False) as archive:
    goldens = archive["document"]
assert goldens.shape == (len(texts), 768) and np.isfinite(goldens).all()
tokenizer = AutoTokenizer.from_pretrained(str(pack / "document"), local_files_only=True)
ids = tokenizer([text.strip() for text in texts], truncation=True, max_length=8192)["input_ids"]
out = sys.stdout.buffer
out.write(struct.pack("<I", len(ids)))
for tokens, golden in zip(ids, goldens):
    out.write(struct.pack("<I", len(tokens)))
    out.write(np.asarray(tokens, dtype="<i4").tobytes())
    out.write(golden.astype("<f4").tobytes())
