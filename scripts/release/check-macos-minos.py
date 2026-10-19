#!/usr/bin/env python3
"""Reject Mach-O files or archives requiring a newer macOS than declared."""
import argparse
import re
import subprocess
import sys


def version(value):
    if not re.fullmatch(r'\d+(?:\.\d+){0,2}', value):
        raise ValueError(f'invalid macOS version: {value!r}')
    parts = tuple(map(int, value.split('.')))
    return parts + (0,) * (3 - len(parts))


def check(declared, path):
    result = subprocess.run(['otool', '-l', path], capture_output=True, text=True)
    if result.returncode:
        raise RuntimeError(f'{path}: otool -l failed: {result.stderr.strip()}')
    member = path
    command = platform = None
    count = 0
    for line in result.stdout.splitlines():
        if line and not line[0].isspace() and line.endswith(':'):
            member = line[:-1]
            command = platform = None
        fields = line.split()
        if len(fields) != 2:
            continue
        key, value = fields
        if key == 'cmd':
            command, platform = value, None
        elif command == 'LC_BUILD_VERSION' and key == 'platform':
            platform = value
        elif ((command == 'LC_BUILD_VERSION' and key == 'minos') or
              (command == 'LC_VERSION_MIN_MACOSX' and key == 'version')):
            if command == 'LC_BUILD_VERSION' and platform != '1':
                raise RuntimeError(f'{member}: expected macOS platform 1, got {platform}')
            count += 1
            if version(value) > version(declared):
                raise RuntimeError(f'{member}: minos {value} exceeds declared target {declared}')
    if count == 0:
        raise RuntimeError(f'{path}: no macOS version load commands')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('declared_target')
    parser.add_argument('paths', nargs='+')
    args = parser.parse_args()
    try:
        version(args.declared_target)
        for path in args.paths:
            check(args.declared_target, path)
    except (OSError, ValueError, RuntimeError) as error:
        print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
