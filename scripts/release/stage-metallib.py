#!/usr/bin/env python3
"""Stage the MLX runtime resource from this Cargo release build, never a global cache."""
import argparse
from pathlib import Path
import shutil


def stage(release, destination):
    candidates = list(release.glob('build/mlx-sys-*/out/build/lib/mlx.metallib'))
    if len(candidates) != 1 or candidates[0].stat().st_size == 0:
        raise ValueError(f'expected one nonempty MLX metallib in {release}, found {candidates}')
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(candidates[0], destination)
    print(f'MLX runtime: {candidates[0]} -> {destination}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('release', type=Path)
    parser.add_argument('destination', type=Path)
    args = parser.parse_args()
    stage(args.release, args.destination)
