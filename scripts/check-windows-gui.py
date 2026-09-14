#!/usr/bin/env python3
"""Reject console-subsystem Windows executables, inspecting the built PE file."""
import argparse
from pathlib import Path
import struct


def subsystem(data):
    if len(data) < 64 or data[:2] != b'MZ':
        raise ValueError('missing DOS header')
    pe = struct.unpack_from('<I', data, 0x3c)[0]
    if pe < 64 or pe + 24 > len(data) or data[pe:pe + 4] != b'PE\0\0':
        raise ValueError('missing PE header')
    optional_size = struct.unpack_from('<H', data, pe + 20)[0]
    optional = pe + 24
    if optional_size < 70 or optional + optional_size > len(data):
        raise ValueError('truncated optional header')
    if struct.unpack_from('<H', data, optional)[0] not in (0x10b, 0x20b):
        raise ValueError('unsupported PE optional header')
    return struct.unpack_from('<H', data, optional + 68)[0]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('executables', nargs='+', type=Path)
    args = parser.parse_args()
    failed = False
    for path in args.executables:
        try:
            actual = subsystem(path.read_bytes())
            if actual != 2:  # IMAGE_SUBSYSTEM_WINDOWS_GUI
                raise ValueError(f'subsystem={actual}; expected GUI=2 (console=3)')
            print(f'PASS {path.name}: Windows GUI, no automatic console')
        except (OSError, ValueError) as error:
            print(f'FAIL {path.name}: {error}')
            failed = True
    return int(failed)


if __name__ == '__main__':
    raise SystemExit(main())
