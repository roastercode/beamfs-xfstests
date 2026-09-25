#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
#
# soak.py -- does the device keep what it is given?
#
# beamfs 0.1.18 on 2026-09-25: a region block written under the buffer
# lock and read back from the device past the page cache comes back
# identical, 405 465 times out of 405 504; it is never written again;
# and a later read, while mounted or at fsck, finds it holding zeros.
# Before that is a filesystem's fault the device has to answer for
# itself, with no filesystem on it.
#
# Random 4 KiB writes with O_DIRECT, each block carrying its number, a
# generation that goes up every time it is written, and a CRC of the
# body. Every write is read back at once. Every 30 seconds a sample of
# written blocks is read again. At the end every written block is read.
# Anything that is not the last generation written is an anomaly, and
# said with what it is: zeros, an older generation (a lost write),
# another block's contents (a misplaced write), or garbage.

import json
import mmap
import os
import random
import struct
import sys
import time
import zlib

BS = 4096
MAGIC = b"BXSOAK01"
HDR = struct.Struct("<8sQQI")          # magic, block, generation, crc(body)
ZONE = (1033, 18443)                   # the region zone of the 1 GiB -N 16384 layout


def body_for(blk, gen):
    return random.Random((blk << 32) ^ gen).randbytes(BS - 32)


def image(blk, gen):
    body = body_for(blk, gen)
    return HDR.pack(MAGIC, blk, gen, zlib.crc32(body)).ljust(32, b"\0") + body


def classify(blk, want, data):
    if data == bytes(BS):
        return "zeros", None
    magic, b, g, crc = HDR.unpack_from(data)
    if magic != MAGIC:
        return "garbage", None
    if b != blk:
        return "other-block", b
    if zlib.crc32(data[32:]) != crc:
        return "corrupt", g
    if g == want:
        return "ok", g
    return ("older" if g < want else "newer"), g


def main():
    dev, secs = sys.argv[1], int(sys.argv[2])
    fd = os.open(dev, os.O_RDWR | os.O_DIRECT)
    size = os.lseek(fd, 0, os.SEEK_END)
    nblk = size // BS
    wbuf = mmap.mmap(-1, BS)
    rbuf = mmap.mmap(-1, BS)
    gen = {}
    anomalies = []
    counts = {"writes": 0, "immediate": 0, "sampled": 0, "final": 0}
    bad = {"immediate": 0, "sampled": 0, "final": 0}

    def read_check(blk, when):
        os.lseek(fd, blk * BS, os.SEEK_SET)
        n = os.readv(fd, [rbuf])
        data = rbuf[:n]
        kind, g = classify(blk, gen[blk], data)
        counts[when] += 1
        if kind != "ok":
            bad[when] += 1
            if len(anomalies) < 200:
                anomalies.append({"when": when, "t": round(time.monotonic() - t0, 3),
                                  "blk": blk, "zone": ZONE[0] <= blk < ZONE[1],
                                  "want": gen[blk], "found": kind, "gen": g})
        return kind

    t0 = time.monotonic()
    rng = random.Random(os.getpid())
    next_sample = t0 + 30
    while time.monotonic() - t0 < secs:
        blk = rng.randrange(nblk)
        gen[blk] = gen.get(blk, 0) + 1
        wbuf[:] = image(blk, gen[blk])
        os.lseek(fd, blk * BS, os.SEEK_SET)
        if os.writev(fd, [wbuf]) != BS:
            raise SystemExit(f"short write at block {blk}")
        counts["writes"] += 1
        read_check(blk, "immediate")
        if time.monotonic() >= next_sample:
            next_sample += 30
            for b in rng.sample(sorted(gen), min(2000, len(gen))):
                read_check(b, "sampled")

    for b in sorted(gen):
        read_check(b, "final")
    os.close(fd)

    zone_written = sum(1 for b in gen if ZONE[0] <= b < ZONE[1])
    zone_bad = sum(1 for a in anomalies if a["zone"])
    report = {"device": dev, "blocks": nblk, "seconds": secs,
              "written_blocks": len(gen), "zone_blocks_written": zone_written,
              "counts": counts, "bad": bad, "zone_bad": zone_bad,
              "anomalies": anomalies}
    print(json.dumps(report))


if __name__ == "__main__":
    main()
