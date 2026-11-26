"""Compare 1/2/4 native lanes with generated data on a real DirectStorage GPU queue."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import csv
import ctypes as C
import hashlib
import json
import os
from pathlib import Path
import random
import struct
import subprocess
import sys
import tempfile
import threading
import time
import uuid
import zlib

from repack import Codec
from test_asset_markers import Source


SIZE = 256 << 10
BAD = 40
CALLBACK = C.CFUNCTYPE(None, C.c_void_p, C.c_uint32)


def data():
    raw = [random.Random(i).randbytes(4096) * (SIZE // 4096) for i in range(BAD + 1)]
    return raw, [zlib.compress(block) for block in raw]


def make_pack(path, sdk, bc):
    codec = Codec(sdk, bc)
    raw, keys = data()
    records = []
    with path.open("wb") as f:
        f.write(bytes(24))
        for i, key in enumerate(keys):
            encoded, digest = codec.transform(key, 8, SIZE)
            records.append(struct.pack("<32s32sQIIII", hashlib.sha256(key).digest(),
                                       bytes(32) if i == BAD else digest, f.tell(),
                                       len(encoded), len(key), len(raw[i]), 8))
            f.write(encoded)
        table = f.tell()
        f.writelines(sorted(records))
        f.seek(0)
        f.write(struct.pack("<8sQQ", b"FX1GDF01", table, len(records)))


def probe(pack, sdk, lanes):
    with os.add_dll_directory(str(sdk)):
        core = C.WinDLL(str(sdk / "dstoragecore.dll"))
        api = C.CDLL(str(sdk / "fx1-storage.dll"))
    ptr, uint = C.c_void_p, C.c_uint32
    device = ptr()
    iid = uuid.UUID("189819f1-1db6-4b57-be54-1821339b85f7").bytes_le
    create = C.WinDLL("d3d12.dll").D3D12CreateDevice
    create.argtypes = [ptr, uint, ptr, C.POINTER(ptr)]
    assert create(None, 0xc000, iid, C.byref(device)) == 0
    read = api.fx1_read_gdeflate_detailed_v1
    by_digest = api.fx1_read_gdeflate_digest_v1
    read.argtypes = by_digest.argtypes = [ptr, C.c_wchar_p, ptr, uint, uint, uint, ptr, C.POINTER(uint), ptr, C.POINTER(Source)]
    finish = api.fx1_finish_gdeflate_delivery
    finish.argtypes = [C.c_int]
    finish.restype = None
    reused = api.fx1_reused_readback_bytes
    reused.restype = C.c_uint64
    raw, keys = data()
    digests = [hashlib.sha256(key).digest() for key in keys]
    local = threading.local()

    def request(i, prepare=None, digest=False, absent=False, tag=0):
        if not hasattr(local, "dst"):
            local.dst = C.create_string_buffer(SIZE)
        support = uint()
        origin = Source(9, tag, 800 + i * SIZE, 1, 0)
        fn = by_digest if digest else read
        key = digests[i] if digest else keys[i]
        if absent:
            key = bytes(32) if digest else b"missing"
        stored = len(keys[i]) if digest or not absent else len(key)
        rc = fn(device, str(pack), key, stored, SIZE, 8, local.dst, C.byref(support), prepare, C.byref(origin))
        if rc == 1:
            assert support.value & 3 and local.dst.raw == raw[i], f"wrong output for {i}"
            finish(1)
        return rc

    assert request(0, digest=True, absent=True) == 3
    entered = [threading.Event() for _ in range(lanes)]
    release = [threading.Event() for _ in range(lanes)]

    def hold(i):
        @CALLBACK
        def prepare(_p, _n):
            entered[i].set()
            release[i].wait(10)
        return request(i, prepare=prepare, digest=bool(i % 2), tag=i + 1)

    with ThreadPoolExecutor(4) as pool:
        futures = []
        try:
            for i in range(lanes):
                futures.append(pool.submit(hold, i))
                assert entered[i].wait(5), f"lane {i} unable to make independent verified progress"
                assert request(i, tag=100) == 2, "duplicate active key used another lane"
            assert request(lanes, tag=101) == 2, "exceeded configured lane bound"
            # Complete later requests while the earlier caller's destination is held.
            for i in reversed(range(lanes)):
                release[i].set()
                assert futures[i].result(timeout=5) == 1
        finally:
            for event in release:
                event.set()
        before = reused()
        for i in range(lanes):
            assert request(i, digest=True, tag=10+i) == 1
        assert reused() - before == SIZE * lanes

        # Fixed work: four clients each read their own ten distinct blocks.
        # Retry a busy lane here to measure completing the same byte count.
        def workload(worker, indices):
            bypasses = 0
            for group in indices:
                i = 4 * group + worker
                deadline = time.monotonic() + 10
                while True:
                    rc = request(i)
                    if rc == 1:
                        break
                    assert rc == 2 and time.monotonic() < deadline, (i, rc)
                    bypasses += 1
                    time.sleep(0.0001)
            return bypasses
        before = reused()
        start = time.perf_counter()
        cold_busy = sum(pool.map(lambda w: workload(w, range(1, 10)), range(4)))
        cold_ms = 1000 * (time.perf_counter() - start)
        assert reused() == before  # 36 separate inputs never submitted in this process.
        before = reused()
        start = time.perf_counter()
        busy = sum(pool.map(lambda w: workload(w, list(range(10)) * 8), range(4)))
        milliseconds = 1000 * (time.perf_counter() - start)
        hits = (reused() - before) // SIZE

        failed_prepare = []
        reject = CALLBACK(lambda p, n: failed_prepare.append(n))
        # A verified independent read may already be completing in another lane
        # when the bad digest is discovered. No failed block reaches its target.
        if lanes > 1:
            entered[0].clear()
            release[0].clear()
            pending = pool.submit(hold, 0)
            assert entered[0].wait(5)
        try:
            C.memset(local.dst, 0x5a, SIZE)
            assert request(BAD, prepare=reject, digest=True, tag=888) < 0
            assert not failed_prepare and local.dst.raw == b"Z" * SIZE
            assert request(0) == 4
        finally:
            if lanes > 1:
                release[0].set()
                assert pending.result(timeout=5) == 1
    print(json.dumps({"lanes": lanes, "cold_reads": 36, "cold_mib": 9, "cold_ms": round(cold_ms, 1),
                      "cold_busy_retries": cold_busy, "reads": 320, "mib": 80, "wall_ms": round(milliseconds, 1),
                      "verified_hits": hits, "busy_retries": busy}))


def check_markers(path, lanes):
    with path.open(newline="") as f:
        rows = list(csv.DictReader(f))
    first = [r for r in rows if r["event"] == "gpu_submit" and 0 < int(r["name_id"], 16) <= lanes][:lanes]
    assert {int(r["tile"]) // 64 for r in first} == set(range(lanes))
    requests = {}
    for row in rows:
        if row["event"] == "eviction":
            assert row["request"] in requests
            continue
        requests.setdefault(row["request"], []).append(row)
    for group in requests.values():
        events = [r["event"] for r in group]
        if int(group[0]["name_id"], 16) == 888:
            assert events == ["gpu_submit"]
        else:
            assert events in (["gpu_submit", "verified_delivery"], ["reuse", "verified_delivery"]), events
        assert len({(r["tile"], r["asset_id"]) for r in group}) == 1


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sdk", required=True, type=Path)
    parser.add_argument("--bc", required=True, type=Path)
    parser.add_argument("--probe", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    sdk = args.sdk.resolve(strict=True)
    if args.probe:
        probe(args.probe, sdk, int(os.environ["VICTORY_GDEFLATE_LANES"]))
    else:
        with tempfile.TemporaryDirectory() as temp:
            pack = Path(temp) / "generated.pack"
            make_pack(pack, sdk, args.bc)
            for lanes in (1, 2, 4):
                markers = Path(temp) / f"{lanes}.csv"
                subprocess.run([sys.executable, "-B", __file__, "--sdk", str(sdk), "--bc", str(args.bc),
                                "--probe", str(pack)], check=True,
                               env={**os.environ, "VICTORY_GDEFLATE_LANES": str(lanes), "VICTORY_GDEFLATE_LRU": "1",
                                    "VICTORY_GDEFLATE_MARKERS": str(markers), "VICTORY_GDEFLATE_MARKER_LIMIT": "65536"})
                check_markers(markers, lanes)
