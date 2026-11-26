"""Exercise the single native submitter on generated data and real DirectStorage hardware."""
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
import uuid

from test_multilane import BAD, CALLBACK, SIZE, data, make_pack

FIELDS = ("jobs", "groups", "bytes", "largest", "max_bytes", "fence_waits", "oversize",
          "deadlines", "byte_caps", "slot_caps", "workers", "owner_tid", "peak_pending",
          "max_age_us", "failed_groups")


def probe(pack, sdk, mode):
    with os.add_dll_directory(str(sdk)):
        core = C.WinDLL(str(sdk / "dstoragecore.dll"))
        bridge = C.CDLL(str(sdk / "fx1-storage.dll"))
    ptr, uint = C.c_void_p, C.c_uint32
    create = C.WinDLL("d3d12.dll").D3D12CreateDevice
    create.argtypes = [ptr, uint, ptr, C.POINTER(ptr)]
    device = ptr()
    iid = uuid.UUID("189819f1-1db6-4b57-be54-1821339b85f7").bytes_le
    assert create(None, 0xc000, iid, C.byref(device)) == 0
    read = bridge.fx1_read_gdeflate_digest_v1
    read.argtypes = [ptr, C.c_wchar_p, ptr, uint, uint, uint, ptr, C.POINTER(uint), ptr, ptr]
    counters = bridge.fx1_single_submit_counters_v1
    counters.argtypes = [C.POINTER(C.c_uint64), uint]
    counters.restype = None
    shutdown = bridge.fx1_shutdown_single_submit_v1
    shutdown.argtypes = []
    shutdown.restype = None
    raw, source = data()
    keys = list(map(lambda s: hashlib.sha256(s).digest(), source))
    producers = set()

    def stats():
        v = (C.c_uint64 * len(FIELDS))()
        counters(v, len(v))
        return dict(zip(FIELDS, v))

    def fetch(index, prepare=None):
        producers.add(threading.get_native_id())
        out = C.create_string_buffer(b"Z" * SIZE, SIZE)
        support = uint()
        rc = read(device, str(pack), keys[index], len(source[index]), SIZE, 8, out,
                  C.byref(support), prepare, None)
        if rc == 1:
            assert out.raw == raw[index] and support.value & 3
        else:
            assert out.raw == b"Z" * SIZE
        return rc

    assert fetch(0) == 1
    for base in range(1, 33, 4):
        gate = threading.Barrier(4)
        def burst(i):
            gate.wait(5)
            return fetch(i)
        with ThreadPoolExecutor(4) as pool:
            assert list(pool.map(burst, range(base, base + 4))) == [1] * 4
    summary = stats()
    assert summary["jobs"] == 33 and summary["bytes"] == 33 * SIZE, summary
    assert summary["workers"] == 1 and summary["owner_tid"] not in producers
    assert summary["groups"] == summary["fence_waits"] and summary["peak_pending"] <= 4
    assert summary["failed_groups"] == 0
    if mode == "oversize":
        assert summary["groups"] == summary["oversize"] == summary["jobs"]
    else:
        assert 1 < summary["largest"] <= 4 and summary["groups"] < summary["jobs"], summary
        assert summary["max_bytes"] <= int(os.environ["VICTORY_GDEFLATE_BATCH_KIB"]) * 1024
    if mode == "corrupt":
        called = []
        cb = CALLBACK(lambda p, n: called.append(n))
        assert fetch(BAD, cb) < 0 and not called and fetch(0) == 4
    else:
        # A returning callback still owns its output slot. Shutdown must wait
        # for the caller even though the group has already passed its fence.
        entered, release, started = (threading.Event() for _ in range(3))
        def hold(p, n):
            entered.set()
            release.wait(5)
        cb = CALLBACK(hold)
        def stop():
            started.set()
            shutdown()
        with ThreadPoolExecutor(2) as pool:
            pending = pool.submit(fetch, 35, cb)
            assert entered.wait(5)
            stopping = pool.submit(stop)
            try:
                assert started.wait(5)
                assert not stopping.done()
            finally:
                release.set()
            assert pending.result(5) == 1
            stopping.result(5)
        assert fetch(0) == 4
    shutdown()
    shutdown()
    print(json.dumps(dict(mode=mode, **summary)))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sdk", required=True, type=Path)
    parser.add_argument("--bc", required=True, type=Path)
    parser.add_argument("--probe", type=Path)
    parser.add_argument("--mode")
    args = parser.parse_args()
    sdk = args.sdk.resolve(strict=True)
    if args.probe:
        probe(args.probe, sdk, args.mode)
    else:
        with tempfile.TemporaryDirectory() as temp:
            pack = Path(temp) / "generated.pack"
            make_pack(pack, sdk, args.bc)
            for mode, kib in (("bounded", 512), ("oversize", 64), ("corrupt", 1024)):
                subprocess.run([sys.executable, "-B", __file__, "--sdk", sdk, "--bc", args.bc,
                                "--probe", pack, "--mode", mode], check=True, timeout=40,
                               env={**os.environ, "VICTORY_GDEFLATE_SINGLE_SUBMIT": "1",
                                    "VICTORY_GDEFLATE_LANES": "4", "VICTORY_GDEFLATE_BATCH_US": "2000",
                                    "VICTORY_GDEFLATE_LRU": "1", "VICTORY_GDEFLATE_WAIT_US": "0",
                                    "VICTORY_GDEFLATE_BATCH_KIB": str(kib)})
