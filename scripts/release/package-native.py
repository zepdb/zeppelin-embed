#!/usr/bin/env python3
"""Prepare native distribution copies without changing Cargo's LTO inputs."""
import argparse
from functools import cache
import os
from pathlib import Path
import re
import shutil
import shlex
import subprocess
import sys
import tempfile


@cache
def llvm_tool(name):
    libdir = Path(subprocess.check_output(['rustc', '--print', 'target-libdir'], text=True).strip())
    tool = libdir.parent / 'bin' / (name + ('.exe' if sys.platform == 'win32' else ''))
    if not tool.is_file():
        raise RuntimeError(f'{tool} is required; install rustup component llvm-tools-preview')
    return str(tool)


LLVM_SECTIONS = ['__LLVM,__bitcode', '__LLVM,__cmdline', '.llvmbc', '.llvmcmd']


def clean_coff_archive(source, output):
    # Rust MSVC archives include short import objects. llvm-objcopy rejects
    # those, so preserve them verbatim and transform only ordinary COFF objects.
    # Keep duplicate member names (e.g. several kernel32.dll imports) in order.
    data = source.read_bytes()
    if not data.startswith(b'!<arch>\n'):
        raise ValueError('expected a regular COFF archive')
    entries, names, offset = [], b'', 8
    while offset < len(data):
        header = data[offset:offset + 60]
        if len(header) != 60 or header[58:] != b'`\n':
            raise ValueError('invalid archive member header')
        size = int(header[48:58])
        end = offset + 60 + size
        if size < 0 or end > len(data):
            raise ValueError('invalid archive member extent')
        name = header[:16].decode('ascii').strip()
        body = data[offset + 60:end]
        offset = end + size % 2
        if name == '//':
            names = body
        elif name not in ('/', '/SYM64/'):
            entries.append((name, body))
    if offset != len(data) or not entries:
        raise ValueError('invalid or empty archive')
    with tempfile.TemporaryDirectory(dir=output.parent) as directory:
        work = Path(directory)
        members = []
        for index, (name, body) in enumerate(entries):
            if name.startswith('/'):
                start = int(name[1:])
                if not 0 <= start < len(names):
                    raise ValueError('invalid archive long-name offset')
                name = re.split(b'\x00|/\n', names[start:], maxsplit=1)[0].decode('utf-8')
            else:
                name = name.removesuffix('/')
            # Sysroot compiler-builtins members can retain build-host paths.
            # Archive labels are not link identities; retain their basenames.
            name = name.replace('\\', '/').rsplit('/', 1)[-1]
            if not name or name in ('.', '..') or ':' in name:
                raise ValueError('invalid archive member name')
            member_dir = work / str(index)
            member_dir.mkdir()
            member = member_dir / name
            member.write_bytes(body)
            # IMPORT_OBJECT_HEADER: signature 0/ffff, version 0, AMD64.
            if body[:8] != b'\x00\x00\xff\xff\x00\x00\x64\x86':
                subprocess.run([llvm_tool('llvm-objcopy'),
                                *['--remove-section=' + section for section in LLVM_SECTIONS],
                                str(member)], check=True)
            members.append(member)
        response = work / 'members.rsp'
        response.write_text('\n'.join(shlex.quote(str(member)) for member in members), encoding='utf-8')
        # Rebuild both COFF linker indexes after member lengths changed.
        output.unlink()
        subprocess.run([llvm_tool('llvm-ar'), '--format=coff', '--rsp-quoting=posix',
                        'qcsD', str(output), '@' + str(response)], check=True)


def package(mode, source, destination):
    if mode == 'archive' and source.resolve() == destination.resolve():
        raise ValueError('archive output must be a distribution copy, not the Cargo input')
    destination.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=destination.name + '.', dir=destination.parent)
    os.close(fd)
    try:
        if mode == 'archive':
            if source.suffix.lower() == '.lib':
                clean_coff_archive(source, Path(temporary))
            else:
                subprocess.run([
                    llvm_tool('llvm-objcopy'),
                    *['--remove-section=' + section for section in LLVM_SECTIONS],
                    str(source), temporary,
                ], check=True)
            sections = subprocess.check_output(
                [llvm_tool('llvm-readobj'), '--sections', temporary], text=True
            )
            if re.search(r'Name: (?:__bitcode|__cmdline|\.llvmbc|\.llvmcmd)(?: |\n)', sections):
                raise RuntimeError('distribution archive still contains embedded LLVM bitcode')
        else:
            shutil.copyfile(source, temporary)
            subprocess.run([llvm_tool('llvm-strip'), '--strip-all', temporary], check=True)
        shutil.copymode(source, temporary)
        os.replace(temporary, destination)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['archive', 'windows-addon'])
    parser.add_argument('source', type=Path)
    parser.add_argument('destination', type=Path)
    args = parser.parse_args()
    package(args.mode, args.source, args.destination)
