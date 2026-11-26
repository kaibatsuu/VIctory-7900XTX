"""Test bounded submit coalescing on a real D3D12/DirectStorage queue and generated input."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import ctypes as C
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import uuid

from test_multilane import BAD, SIZE, data, make_pack

FIELDS = ("requests", "batches", "bytes", "peak_fill", "peak_bytes", "sum_age_us",
          "max_age_us", "deadline", "byte_cap", "lane_cap", "oversize", "late_us",
          "max_late_us", "timer_error")


def probe(pack, sdk, mode):
    with os.add_dll_directory(str(sdk)):
        core = C.WinDLL(str(sdk / "dstoragecore.dll"))
        api = C.CDLL(str(sdk / "fx1-storage.dll"))
    ptr, uint = C.c_void_p, C.c_uint32
    device = ptr()
    iid = uuid.UUID("189819f1-1db6-4b57-be54-1821339b85f7").bytes_le
    create = C.WinDLL("d3d12.dll").D3D12CreateDevice
    create.argtypes = [ptr, uint, ptr, C.POINTER(ptr)]
    assert create(None, 0xc000, iid, C.byref(device)) == 0
    read = api.fx1_read_gdeflate_digest_v1
    read.argtypes = [ptr, C.c_wchar_p, ptr, uint, uint, uint, ptr, C.POINTER(uint), ptr, ptr]
    counts = api.fx1_batch_counters_v1
    counts.argtypes = [C.POINTER(C.c_uint64), uint]
    counts.restype = None
    reuse = api.fx1_reused_readback_bytes
    reuse.restype = C.c_uint64
    raw, source = data()
    keys = [hashlib.sha256(s).digest() for s in source]
    lanes = int(os.environ["VICTORY_GDEFLATE_LANES"])

    def stats():
        values = (C.c_uint64 * len(FIELDS))()
        counts(values, len(FIELDS))
        return dict(zip(FIELDS, values))

    def fetch(i, missing=False):
        dest = C.create_string_buffer(SIZE)
        C.memset(dest, 0x5a, SIZE)
        support = uint()
        rc = read(device, str(pack), bytes(32) if missing else keys[i], len(source[i]),
                  SIZE, 8, dest, C.byref(support), None, None)
        if rc == 1:
            assert dest.raw == raw[i] and support.value & 3
        else:
            assert dest.raw == b"Z" * SIZE
        return rc

    assert fetch(0, missing=True) == 3 and not any(stats().values())
    # A single isolated miss has no producer to wake the batch.
    assert fetch(0) == 1
    first = stats()
    if mode not in ("off", "oversize", "single_lane"):
        assert first["requests"] == first["batches"] == first["deadline"] == 1, first
    assert fetch(0) == 1 and reuse() == SIZE and stats() == first
    timing = []
    with ThreadPoolExecutor(4) as pool:
        if mode == "deadline":
            # Release a follower midway through the fixed deadline. Warm the
            # Python and worker paths first. Track actual wake lateness separate
            # from the algorithm's configured deadline; scheduling can run late.
            pool.submit(lambda: None).result(2)
            for i in range(1, 26, 2):
                base = stats()
                owner = pool.submit(fetch, i)
                while stats()["requests"] == base["requests"] and not owner.done():
                    pass
                start = time.perf_counter_ns()
                while time.perf_counter_ns() - start < 900_000:
                    pass
                assert fetch(i + 1) == owner.result(2) == 1
                post = stats()
                if post["batches"] - base["batches"] == 1:
                    assert post["deadline"] == base["deadline"] + 1
                    timing.append(post["sum_age_us"] - base["sum_age_us"])
            assert timing and min(timing) < 2850, timing
        else:
            for base in range(1, 33, lanes):
                gate = threading.Barrier(lanes)
                def worker(i):
                    gate.wait(3)
                    return fetch(i)
                futures = [pool.submit(worker, base + j) for j in range(lanes)]
                assert all(f.result(3) == 1 for f in futures)
    # A corrupt decoded digest cannot modify its destination; later calls
    # bypass the disabled bridge. Counts below include the rejected submit.
    assert fetch(BAD) < 0 and fetch(0) == 4
    summary = stats()
    if mode in ("off", "single_lane"):
        assert not any(summary.values()), summary
    elif mode == "oversize":
        assert summary["oversize"] == 34 and not summary["requests"] and not summary["batches"], summary
    else:
        assert summary["requests"] == (28 if mode == "deadline" else 34)
        assert summary["batches"] == sum(summary[n] for n in ("deadline", "lane_cap", "byte_cap", "timer_error"))
        assert not summary["timer_error"] and not summary["oversize"]
        assert summary["bytes"] == SIZE * summary["requests"]
        assert summary["peak_bytes"] <= int(os.environ["VICTORY_GDEFLATE_BATCH_KIB"]) * 1024
        if mode in ("burst", "exact"):
            assert summary["peak_fill"] >= 2 and summary["batches"] < summary["requests"]
            assert summary["lane_cap" if mode == "burst" else "byte_cap"] > 0
        if mode == "spill":
            assert summary["peak_fill"] == 1 and summary["byte_cap"] > 0
    print(json.dumps(dict(mode=mode, lanes=lanes, fixed_deadline_observed_us=timing, **summary)))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sdk", type=Path, required=True)
    parser.add_argument("--bc", type=Path, required=True)
    parser.add_argument("--probe", type=Path)
    parser.add_argument("--mode")
    args = parser.parse_args()
    sdk = args.sdk.resolve(strict=True)
    if args.probe:
        probe(args.probe, sdk, args.mode)
    else:
        with tempfile.TemporaryDirectory() as tmp:
            pack = Path(tmp) / "generated.pack"
            make_pack(pack, sdk, args.bc)
            for mode, lanes, ttl, kib in (("off", 4, 0, 512), ("single_lane", 1, 250, 512),
                                          ("burst", 2, 250, 4096), ("exact", 4, 2000, 512),
                                          ("spill", 4, 2000, 384), ("oversize", 4, 250, 128),
                                          ("deadline", 4, 2000, 4096)):
                subprocess.run([sys.executable, "-B", __file__, "--sdk", sdk, "--bc", args.bc,
                                "--probe", pack, "--mode", mode], check=True, timeout=60,
                               env={**os.environ, "VICTORY_GDEFLATE_LANES": str(lanes),
                                    "VICTORY_GDEFLATE_BATCH_US": str(ttl), "VICTORY_GDEFLATE_BATCH_KIB": str(kib),
                                    "VICTORY_GDEFLATE_LRU": "1", "VICTORY_GDEFLATE_WAIT_US": "0"})
