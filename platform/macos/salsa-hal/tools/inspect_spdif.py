#!/usr/bin/env python3
"""List IEC 61937 bursts in a raw 16-bit carrier capture or fixture."""

import argparse
import collections
import struct
import sys


SYNC1 = 0xF872
SYNC2 = 0x4E1F
TYPE_NAMES = {1: "ac3", 21: "eac3", 22: "truehd"}


def words(data):
    if len(data) % 2:
        data = data[:-1]
    return struct.unpack("<%dH" % (len(data) // 2), data)


def inspect(path):
    data = open(path, "rb").read()
    w = words(data)
    bursts = []
    i = 0
    while i + 3 < len(w):
        if w[i] == SYNC1 and w[i + 1] == SYNC2:
            data_type = w[i + 2]
            length_code = w[i + 3]
            bursts.append((i * 2, data_type, length_code))
            i += 4
            continue
        i += 1
    return data, bursts


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("files", nargs="+")
    args = ap.parse_args()
    failed = False
    for path in args.files:
        data, bursts = inspect(path)
        types = collections.Counter(b[1] for b in bursts)
        print("%s: %d bytes, %d sync bursts" % (path, len(data), len(bursts)))
        for data_type, count in sorted(types.items()):
            name = TYPE_NAMES.get(data_type, "type-%d" % data_type)
            lengths = sorted(set(b[2] for b in bursts if b[1] == data_type))
            print("  %s: %d bursts, length_code %s" % (
                name, count, ", ".join(str(x) for x in lengths[:8])))
        if bursts:
            first = bursts[0]
            print("  first burst: offset %d, type %d, length_code %d" % first)
        else:
            failed = True
            print("  no IEC 61937 sync words found", file=sys.stderr)
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
