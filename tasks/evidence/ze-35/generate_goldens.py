"""Independent ZGCA layout author, using Python's installed xxhash tooling only."""
from pathlib import Path
import struct
import xxhash

ROOT = Path(__file__).resolve().parents[3]

def u128(value):
    return value.to_bytes(16, 'little')

def blob(value):
    if isinstance(value, str):
        value = value.encode('utf-8')
    return struct.pack('<Q', len(value)) + value

def image(rows, document=False):
    header = b'ZGCA' + struct.pack('<HHQ', 1, 1, 0)
    header += u128((1 << 100) + 7) + u128((1 << 128) - 1) + u128((1 << 64) + 1)
    header += struct.pack('<QQQQQQB7x', 9, (1 << 64) - 1, 17, 1, 0x123456789abcdef0, len(rows), document)
    assert len(header) == 120
    body = b''
    if document:
        body = blob('model\0x') + blob('v1') + blob(bytes([1, 2, 3]))
        body += struct.pack('<IH', 2, 1) + blob('doc: ')
        body += struct.pack('<IHHB', 32, 3, 1, 1) + blob('build')
    for kind, symbol, name in rows:
        body += struct.pack('<B7xQ', kind, symbol) + blob(name)
    data = bytearray(header + body)
    struct.pack_into('<Q', data, 8, len(data) + 8)
    return bytes(data) + struct.pack('<Q', xxhash.xxh3_64_intdigest(data))

rows = [(1, 9, ''), (2, (1 << 64) - 1, 'R'), (3, 17, 'é\0'), (4, 1, 'namespace')]
for name, document in [('without_embedding', False), ('document', True)]:
    data = image(rows, document)
    filename = f'graph_catalog_{name}_v1'
    path = ROOT / 'crates/zeppelin-embed/tests/fixtures/format' / (filename + '.hex')
    assert bytes.fromhex(path.read_text()) == data, 'frozen golden drift'
    (ROOT / 'fuzz/seeds/graph_catalog' / filename).write_bytes(data)
    print(filename, len(data), data[-8:].hex())
# The emoji crosses the decoder's 64KiB text-validation boundary.
long = image([(1, 9, 'a' * (65536 - 1) + '😀' + 'z' * 65536)])
(ROOT / 'fuzz/seeds/graph_catalog/split_codepoint').write_bytes(long)
print('split_codepoint', len(long), long[-8:].hex())
