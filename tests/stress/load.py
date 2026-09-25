#!/usr/bin/env python3
"""Allocate memory in phases, hold it, then touch every page once.

usage: load.py KIND:MB [KIND:MB ...]
  KIND is `comp` (about 4x compressible under zstd) or `random` (incompressible).
Phases run in order and stay allocated, so `comp:3072 random:2560` puts random
data on top of compressible data; the final re-read faults everything back in,
which is the churn that stresses a grown zram pool.
"""

import os
import sys
import time

CHUNK = 64 * 1024 * 1024
PAGE = 4096


def block(kind: str) -> bytearray:
    if kind == "random":
        return bytearray(os.urandom(CHUNK))
    # 1 KiB random + 3 KiB repeated text per page: roughly 4x under zstd.
    data = bytearray()
    for _ in range(CHUNK // PAGE):
        data += os.urandom(1024) + b"swap-test-" * 307 + b"xy"
    return data


def main() -> None:
    start = time.monotonic()
    held = []
    for phase in sys.argv[1:]:
        kind, mb = phase.split(":")
        if kind not in ("comp", "random"):
            sys.exit(f"unknown kind {kind!r}; use comp or random")
        for _ in range(int(mb) // 64):
            held.append(block(kind))
        print(f"{time.monotonic() - start:7.1f}s {kind} {mb}MB done", flush=True)
    time.sleep(20)
    reread = time.monotonic()
    total = sum(b[i] for b in held for i in range(0, len(b), PAGE))
    print(f"readback {time.monotonic() - reread:.1f}s sum={total}", flush=True)


if __name__ == "__main__":
    main()
