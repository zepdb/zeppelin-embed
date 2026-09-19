# Independent layout assembly from the reviewed specification; never calls Rust codecs.
from pathlib import Path
import struct,xxhash
out=Path('crates/zeppelin-embed/tests/fixtures/format')
u128=lambda n:n.to_bytes(16,'little')
checksum=lambda b:xxhash.xxh3_64_intdigest(b)
store=(1<<100)+11;artifact=(1<<96)+23
ref=lambda offset,length,kind:u128(artifact)+struct.pack('<QIHH',offset,length,kind,1)
def write(name,b):
 text=b.hex();(out/name).write_text('\n'.join(text[i:i+64] for i in range(0,len(text),64))+'\n')
def obj(family,blocks):
 body=b'';directory=b''
 for kind,payload in blocks:
  offset=96+len(body);digest=checksum(payload)
  frame=struct.pack('<HHIQQ',kind,1,0,len(payload),digest)+payload
  body+=frame;directory+=struct.pack('<QIHHQ',offset,len(frame),kind,1,digest)
 length=96+len(body)+len(directory)+8
 data=b'ZEPEMBED'+struct.pack('<HHIQQ',family,1,0,96,length)
 data+=u128(store)+u128(artifact)+struct.pack('<QQIIQ',7,len(body),len(blocks),0,31)+body+directory
 return data+struct.pack('<Q',checksum(data))
write('native_graph_object_v1.hex',obj(17,[(4,b'graph-only\0no vectors'),(5,b'')]))
write('native_graph_root_envelope_v1.hex',obj(18,[(9,b'opaque checkpoint bytes')]))
write('native_graph_reference_v1.hex',ref(96,100,7))
def page(kind,level,cells):
 result=bytearray(16384)
 result[:32]=b'ZGTP'+struct.pack('<HHIIHHIQ',1,kind,16384,len(cells),level,0,len(cells)*8,7)
 offset=64+8*len(cells)
 for i,cell in enumerate(cells):
  result[64+8*i:72+8*i]=struct.pack('<II',offset,len(cell))
  result[offset:offset+len(cell)]=cell;offset+=len(cell)
 result[56:64]=struct.pack('<Q',checksum(result))
 return result
key=lambda raw:b'\0'*4+struct.pack('<Q',len(raw))+raw
leaf=lambda k,v:struct.pack('<II',len(k),len(v))+k+v
write('native_graph_inline_page_v1.hex',page(1,0,[leaf(key(u128(255)),b'first'),leaf(key(u128((1<<64)+1)),b'second')]))
overflow=b'\1\0\0\0'+struct.pack('<Q',8*1024*1024)+ref(96,100,7)
child=ref(196,16384+24,1)
branch=lambda k:b'\0'*4+struct.pack('<I',len(k))+child+k
infinite=b'\1\0\0\0'+struct.pack('<I',0)+child
write('native_graph_overflow_branch_v1.hex',page(3,1,[branch(overflow),infinite]))
print('Independent Python struct + xxhash',xxhash.VERSION,'fixture generator')
