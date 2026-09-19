#!/usr/bin/env python3
"""Independent explicit ZGWF v1 layout; requires tooling-only Python xxhash.
No core encoder or generated Rust bytes are imported. Synthetic required-object
references pin carriage only; their participant payloads are not a GC proof.
"""
from pathlib import Path
import struct
import xxhash

OUT = Path(__file__).resolve().parents[3] / 'crates/zeppelin-embed/tests/fixtures/graph-wal'
u8 = lambda n: struct.pack('<B', n)
u16 = lambda n: struct.pack('<H', n)
u32 = lambda n: struct.pack('<I', n)
u64 = lambda n: struct.pack('<Q', n)
u128 = lambda n: n.to_bytes(16, 'little')
hash64 = lambda b: u64(xxhash.xxh3_64_intdigest(b))
STORE = (1 << 100) + 3
ARTIFACT = (1 << 96) + 9
BATCH = (1 << 101) + 5
NODE = (1 << 80) + 19
REL = (1 << 79) + 7

def descriptor(artifact=ARTIFACT):
    return u128(STORE)+u128(artifact)+u64(0)+u64(1)+u32(200)+u16(17)+u16(1)+u64(7)

def reference(kind=10, artifact=ARTIFACT):
    return descriptor(artifact)+u128(artifact)+u64(96)+u32(64)+u16(kind)+u16(1)

def optional(ref=None):
    return u8(ref is not None)+bytes(7)+(ref or b'')

def refs(values):
    return u32(len(values))+bytes(4)+b''.join(values)

def state(sequence):
    prefix=u128(STORE)+u64(sequence)+u64(sequence)+u128(NODE)+u128(REL)
    prefix+=b''.join(u64(n) for n in [3,5,7,11])+u64(1)
    roots=b''.join(optional(reference(1, ARTIFACT+i+1)) for i in range(8))
    participants=reference()+optional(reference(10,ARTIFACT+20))+optional(reference(10,ARTIFACT+21))+optional(reference(10,ARTIFACT+22))
    return prefix+roots+participants+refs([reference(10,ARTIFACT+30),reference(10,ARTIFACT+31)])

def record(tag, index, sequence, payload):
    h=b'ZGWF'+u16(tag)+u16(1)+u32(len(payload))+u32(index)+u64(sequence)+u128(BATCH+sequence-1)+bytes(16)
    assert len(h)==56
    h+=hash64(h)
    return h+payload+hash64(h+payload)

def envelope(sequence, kind, changes):
    # Commit has fixed size independently of the digest; include all framing.
    commit_payload=u32(len(changes))+bytes(4)+bytes(8)+state(sequence)
    length=128+sum(72+len(p) for _,p in changes)+72+len(commit_payload)
    begin=u128(STORE)+u8(kind)+bytes(7)+u64(sequence-1)+u64(sequence)+u32(len(changes))+bytes(4)+u64(length)
    encoded=record(1,0,sequence,begin)
    for index,(tag,payload) in enumerate(changes,1):
        encoded+=record(tag,index,sequence,payload)
    commit_payload=u32(len(changes))+bytes(4)+hash64(encoded)+state(sequence)
    encoded+=record(6,len(changes)+1,sequence,commit_payload)
    assert len(encoded)==length
    return encoded

namespace='n\0é'.encode()
key='recreated'.encode()
provenance=b'ZGOP'+u16(1)+u8(4)+u8(1)+u8(1)+u64(len(namespace))+namespace+u64(len(key))+key
provenance+=u64(31)+u64(31)+u8(3)+u64(29)+u8(1)+u128(NODE)+u8(0)+u64(1)
mutation=u8(1)+u8(10)+bytes(6)+u64(len(provenance))+provenance+optional(reference(4))
intent=(1<<110)+2
candidate=descriptor(ARTIFACT+100)
pending=candidate+u8(3)+bytes(7)+u128(intent)
proof=u128(intent)+u64(1)+u64(1)+u64(1)+u8(1)+bytes(7)+reference()+u64(0x1020304050607080)+reference()+u64(0x8877665544332211)+refs([candidate])
reclaimed=candidate+u8(4)+bytes(7)+u128(intent)
complete=u128(intent)+reference()+u8(3)+bytes(7)+refs([candidate])+refs([])
header=b'ZEPEMBED'+u16(19)+u16(1)+u32(0)+u64(64)+u64(0)+u128(STORE)+u64(1)
wire=header+hash64(header)+envelope(1,1,[(2,mutation)])+envelope(2,2,[(3,pending),(4,proof)])+envelope(3,2,[(3,reclaimed),(5,complete)])
OUT.mkdir(parents=True,exist_ok=True)
(OUT/'complete-v1.bin').write_bytes(wire)
(OUT/'complete-v1.hex').write_text(wire.hex()+'\n')
print(f'{len(wire)} bytes; xxh3-64 {xxhash.xxh3_64_hexdigest(wire)}')

# Optional reproducible seed corpus. Fuzzing may add files to its own copy.
if __name__ == '__main__':
    import argparse
    parser = argparse.ArgumentParser()
    parser.add_argument('--fuzz-corpus', type=Path)
    args = parser.parse_args()
    if args.fuzz_corpus:
        args.fuzz_corpus.mkdir(parents=True, exist_ok=True)
        args.fuzz_corpus.joinpath('complete-v1').write_bytes(wire)
        namespace = ('x' * 65535 + '🧭' + 'z' * 131072).encode()
        provenance = b'ZGOP'+u16(1)+u8(4)+u8(1)+u8(1)+u64(len(namespace))+namespace+u64(1)+b'k'
        provenance += u64(31)+u64(31)+u8(3)+u64(29)+u8(1)+u128(NODE)+u8(0)+u64(1)
        mutation = u8(1)+u8(10)+bytes(6)+u64(len(provenance))+provenance+optional(reference(4))
        seed = header+hash64(header)+envelope(1,1,[(2,mutation)])
        args.fuzz_corpus.joinpath('split-codepoint').write_bytes(seed)
        print(f'fuzz seeds: {len(wire)} and {len(seed)} bytes')
