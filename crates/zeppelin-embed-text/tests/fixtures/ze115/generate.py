#!/usr/bin/env python3
"""Rebuild small, invented transformer fixtures and independent PyTorch CPU oracles.

Tooling only: torch and xxhash are existing ze-model tools, not Rust dependencies.
No trained weights, network access, or model accuracy claim is involved.
"""
from pathlib import Path
import math
import struct
import subprocess
import torch
import xxhash

ROOT = Path(__file__).resolve().parent
U16 = lambda n: struct.pack('<H', n)
U32 = lambda n: struct.pack('<I', n)
U64 = lambda n: struct.pack('<Q', n)
F32 = lambda n: struct.pack('<f', n)
BYTES = lambda b: U32(len(b)) + b
TEXT = lambda s: BYTES(s.encode())
torch.set_num_threads(1)


def generate(architecture):
    tensors = {}

    def add(name, shape, norm=False):
        # Integer arithmetic makes invented weights exactly reproducible.
        seed = sum((i + 1) * ord(c) for i, c in enumerate(name))
        values = [((seed + i * 7 + i * i * 3) % 29 - 14) / 32 for i in range(math.prod(shape))]
        if norm:
            values = [1 + v / 4 for v in values]
        tensors[name] = torch.tensor(values, dtype=torch.float64).reshape(shape)

    add('embeddings.word_embeddings.weight', (7, 4))
    add('embeddings.token_type_embeddings.weight', (2, 4))
    if architecture == 1:
        add('embeddings.position_embeddings.weight', (16, 4))
    add('embeddings.LayerNorm.weight', (4,), True)
    add('embeddings.LayerNorm.bias', (4,))
    base = 'encoder.layer.0.'
    if architecture == 1:
        for part in ['attention.self.query', 'attention.self.key', 'attention.self.value', 'attention.output.dense']:
            add(base + part + '.weight', (4, 4))
            add(base + part + '.bias', (4,))
        for part in ['attention.output.LayerNorm', 'output.LayerNorm']:
            add(base + part + '.weight', (4,), True)
            add(base + part + '.bias', (4,))
        add(base + 'intermediate.dense.weight', (6, 4))
        add(base + 'intermediate.dense.bias', (6,))
        add(base + 'output.dense.weight', (4, 6))
        add(base + 'output.dense.bias', (4,))
    else:
        for part, shape in [('attention.qkv_proj', (12, 4)), ('attention.o_proj', (4, 4)), ('mlp.down_proj', (4, 6))]:
            add(base + part + '.weight', shape)
            add(base + part + '.bias', (shape[0],))
        add(base + 'mlp.up_gate_proj.weight', (12, 4))
        for part in ['attn_ln', 'mlp_ln']:
            add(base + part + '.weight', (4,), True)
            add(base + part + '.bias', (4,))
    add('dense.weight', (3, 4))
    add('dense.bias', (3,))

    raw = {name: struct.pack('<' + 'f' * value.numel(), *value.flatten().tolist()) for name, value in tensors.items()}
    body = b'ZEMB0001' + U32(1) + U32(8192) + bytes([1]) + bytes(7)
    body += bytes([0]) + TEXT('ze115-tiny') + TEXT('invented-v1')
    body += BYTES(xxhash.xxh3_128_intdigest(b''.join(raw.values())).to_bytes(16, 'little'))
    dims_offset = len(body)
    body += U32(3) + U16(0) + TEXT('') + U32(16) + U16(2) + U16(1) + bytes([0])
    pooling_offset = len(body)
    body += bytes([1]) + U16(architecture) + U16(1) + U32(4) + U16(1) + U32(6) + U32(2) + U32(16)
    body += F32(1e-5) + F32(10000) + U32(3)
    body += bytes([1, 1, 0, 0]) + U32(0) + U32(1) + U32(2) + U32(3) + U32(7)
    for token in ['[PAD]', '[UNK]', '[CLS]', '[SEP]', 'the', 'bronze', 'zeppelin']:
        body += TEXT(token) + F32(0)
    body += struct.pack('<d', .5) + U32(len(tensors))
    offset = 8192
    for name, value in tensors.items():
        data = raw[name]
        body += TEXT('document/' + name) + bytes([0, value.ndim, 0, 0])
        body += b''.join(U32(n) for n in value.shape) + U64(offset) + U64(len(data)) + U64(xxhash.xxh3_64_intdigest(data))
        offset += len(data)
    body += BYTES(b'')
    assert len(body) < 8192
    body += bytes(8192 - len(body)) + b''.join(raw.values())
    body += xxhash.xxh3_128_intdigest(body).to_bytes(16, 'little')
    name = 'bert' if architecture == 1 else 'gte'
    (ROOT / (name + '.zem')).write_bytes(body)

    ids = torch.tensor([[2, 5, 6, 3, 0], [2, 6, 3, 0, 0]])
    mask = torch.tensor([[1, 1, 1, 1, 0], [1, 1, 1, 0, 0]], dtype=torch.float64)
    def linear(x, name):
        return torch.nn.functional.linear(x, tensors[name + '.weight'], tensors.get(name + '.bias'))
    def norm(x, name):
        return torch.nn.functional.layer_norm(x, (4,), tensors[name + '.weight'], tensors[name + '.bias'], 1e-5)
    x = tensors['embeddings.word_embeddings.weight'][ids] + tensors['embeddings.token_type_embeddings.weight'][0]
    if architecture == 1:
        x = x + tensors['embeddings.position_embeddings.weight'][:5]
    x = norm(x, 'embeddings.LayerNorm')
    if architecture == 1:
        q, k, v = [linear(x, base + 'attention.self.' + n) for n in ['query', 'key', 'value']]
    else:
        q, k, v = linear(x, base + 'attention.qkv_proj').chunk(3, dim=-1)
        angles = torch.arange(5, dtype=torch.float64)[:, None] * torch.tensor([1., .01])
        def rope(value):
            a, b = value.chunk(2, dim=-1)
            return torch.cat([a * angles.cos() - b * angles.sin(), b * angles.cos() + a * angles.sin()], dim=-1)
        q, k = rope(q), rope(k)
    # PyTorch CPU fused attention is independent of the MLX graph implementation.
    attention = torch.nn.functional.scaled_dot_product_attention(q[:, None], k[:, None], v[:, None], attn_mask=(mask[:, None, None] - 1) * 1e9)[:, 0]
    if architecture == 1:
        x = norm(x + linear(attention, base + 'attention.output.dense'), base + 'attention.output.LayerNorm')
        x = norm(x + linear(torch.nn.functional.gelu(linear(x, base + 'intermediate.dense')), base + 'output.dense'), base + 'output.LayerNorm')
    else:
        x = norm(x + linear(attention, base + 'attention.o_proj'), base + 'attn_ln')
        up, gate = linear(x, base + 'mlp.up_gate_proj').chunk(2, dim=-1)
        x = norm(x + linear(up * torch.nn.functional.gelu(gate), base + 'mlp.down_proj'), base + 'mlp_ln')
    pooled = [(x * mask[:, :, None]).sum(1) / mask.sum(1)[:, None], x[:, 0], x[torch.arange(2), mask.sum(1).long() - 1]]
    lines = [f'pub const {name.upper()}_REFERENCE: [[[f32; 3]; 2]; 3] = [']
    for p in pooled:
        rows = linear(p, 'dense').tolist()
        lines.append('    [' + ', '.join('[' + ', '.join(f'{v:.10f}' for v in row) + ']' for row in rows) + '],')
    lines.append('];')
    return '\n'.join(lines), dims_offset, pooling_offset

results = [generate(1), generate(2)]
assert results[0][1:] == results[1][1:]
text = '// Generated by generate.py using PyTorch CPU; never copied from MLX output.\n'
text += f'pub const DIMS_OFFSET: usize = {results[0][1]};\npub const POOLING_OFFSET: usize = {results[0][2]};\n'
text += '\n'.join(r[0] for r in results) + '\n'
(ROOT / 'reference.rs').write_text(text)
subprocess.run(['rustfmt', '--edition', '2024', str(ROOT / 'reference.rs')], check=True)
print('torch', torch.__version__, 'xxhash', xxhash.VERSION)
for name in ['bert.zem', 'gte.zem', 'reference.rs']:
    import hashlib
    data = (ROOT / name).read_bytes()
    print(name, len(data), hashlib.sha256(data).hexdigest())
