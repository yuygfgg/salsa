#!/usr/bin/env python3
"""Require a complete SPDIF fixture in a driver recording, with zero padding."""

import argparse
import hashlib
import pathlib


def compare(reference, capture):
    if not reference:
        raise ValueError("empty reference")
    offset = capture.find(reference)
    if offset < 0:
        raise ValueError("complete fixture not found: data differ, are missing, or are interrupted")
    if any(capture[:offset]) or any(capture[offset + len(reference):]):
        raise ValueError("nonzero bytes outside the complete fixture")
    return offset


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("reference", type=pathlib.Path)
    parser.add_argument("capture", type=pathlib.Path)
    args = parser.parse_args()
    reference = args.reference.read_bytes()
    capture = args.capture.read_bytes()
    offset = compare(reference, capture)
    print(f"PASS: {args.reference.name} == {args.capture.name}[{offset}:{offset + len(reference)}]")
    print(f"matched_bytes={len(reference)} leading_zero_bytes={offset} trailing_zero_bytes={len(capture) - offset - len(reference)}")
    print(f"sha256={hashlib.sha256(capture[offset:offset + len(reference)]).hexdigest()}")


if __name__ == "__main__":
    main()
