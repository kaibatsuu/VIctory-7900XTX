"""Exercise short worker-only admission waits using a generated pack and a real GPU queue."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import ctypes as C
import hashlib
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import threading
import time
import uuid
import zlib

from repack import Codec


def data(i):
    return bytes((i + j * 17) & 255 for j in range(257))


def pack_fixture(path, sdk, bc):
    records = []
    codec = Codec(sdk, bc)
    with path.open("wb") as f:
        f.write(bytes(24))
        for i in range(6):
            raw = data(i)
            source = zlib.compress(raw)
            encoded, digest = codec.transform(source, 8, len(raw))
            if i == 5:
                digest = bytes(32)
            records.append(struct.pack("<32s32sQIIII", hashlib.sha256(source).digest(), digest,
                            f.tell(), len(encoded), len(source), len(raw), 8))
            f.write(encoded)
        table = f.tell()
        f.writelines(sorted(records))
        f.seek(0)
        f.write(struct.pack("<8sQQ", b"FX1GDF01", table, len(records)))


def probe(pack, sdk, budget):
    lanes = int(os.environ["VICTORY_GDEFLATE_LANES"])
    with os.add_dll_directory(str(sdk)):
        core = C.WinDLL(str(sdk / "dstoragecore.dll"))
        api = C.CDLL(str(sdk / "fx1-storage.dll"))
    ptr, uint = C.c_void_p, C.c_uint32
    iid = uuid.UUID("189819f1-1db6-4b57-be54-1821339b85f7").bytes_le
    device = ptr()
    create = C.WinDLL("d3d12.dll").D3D12CreateDevice
    create.argtypes = [ptr, uint, ptr, C.POINTER(ptr)]
    assert create(None, 0xc000, iid, C.byref(device)) == 0
    raw_read = api.fx1_read_gdeflate_detailed_v1
    digest_read = api.fx1_read_gdeflate_digest_v1
    raw_read.argtypes = digest_read.argtypes = [ptr, C.c_wchar_p, ptr, uint, uint, uint, ptr, C.POINTER(uint), ptr, ptr]
    read_counts = api.fx1_slot_wait_counters_v1
    read_counts.argtypes = [C.POINTER(C.c_uint64), uint]
    read_counts.restype = None
    reused = api.fx1_reused_readback_bytes
    reused.restype = C.c_uint64

    def stats():
        values = (C.c_uint64 * 9)()
        read_counts(values, 9)
        return list(values)

    def read(i, hold=None, digest=False):
        expected = data(i)
        source = zlib.compress(expected)
        result = C.create_string_buffer(b"Z" * len(expected), len(expected))
        support = uint()
        fn = digest_read if digest else raw_read
        key = hashlib.sha256(source).digest() if digest else source
        code = fn(device, str(pack), key, len(source), len(expected), 8, result, C.byref(support), hold, None)
        if code == 1:
            assert result.raw == expected and support.value & 3
        else:
            assert result.raw == b"Z" * len(expected)
        return code

    entered, release = threading.Event(), threading.Event()

    @C.CFUNCTYPE(None, ptr, uint)
    def hold(_ptr, _size):
        entered.set()
        release.wait(5)

    # Warm the DLL/pack before timing the admission path. Block a lane just
    # before writing its verified destination; no unfinished GPU bytes can leak.
    assert read(0) == 1
    with ThreadPoolExecutor(6) as pool:
        owner = pool.submit(read, 0, hold)
        try:
            assert entered.wait(3)
            baseline = stats()
            assert read(0, digest=True) == 2
            timed = stats()
            assert read(0) == 2
            if budget:
                assert timed[2] == baseline[2] + 1
                assert stats()[0] == timed[0], "original-byte retry took a second wait"
            else:
                assert stats() == baseline
            if lanes > 1:
                assert read(1) == 1, "independent key should use free lane while duplicate remains busy"
        finally:
            release.set()
        assert owner.result(3) == 1
        if budget == 2000 and lanes == 1:
            # Prestart both waiters. Retry only when OS scheduling loses the
            # short overlap; timing of the observer is not a decoder invariant.
            before_reuse = reused()
            for _ in range(20):
                entered.clear()
                release.clear()
                barrier = threading.Barrier(3)
                def wait_read(i):
                    barrier.wait(timeout=3)
                    return read(i)
                owner = pool.submit(read, 0, hold)
                assert entered.wait(3)
                first = pool.submit(wait_read,0)
                second = pool.submit(wait_read,1)
                try:
                    barrier.wait(timeout=3)
                    deadline = time.perf_counter() + .02
                    while stats()[8] != 2 and time.perf_counter() < deadline:
                        pass
                    prev = stats()
                    if prev[8] == 2:
                        assert read(2) == 2
                        overflowed = stats()[3] > prev[3]
                    else:
                        overflowed = False
                finally:
                    release.set()
                assert owner.result(3) == 1
                a, b = first.result(3), second.result(3)
                if overflowed and a == 1 and b == 1:
                    break
            else:
                raise AssertionError("two waiters did not complete after a bounded admission")
            assert stats()[4] == 2 and stats()[8] == 0 and reused() > before_reuse
        assert read(5) < 0
        assert read(0) == 4
    print(f"lanes={lanes} budget_us={budget} queued,admitted,timeouts,overflow,peak,elapsed_us,max_us,gate_busy,current={stats()}")


if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--sdk", type=Path, required=True)
    p.add_argument("--bc", type=Path, required=True)
    p.add_argument("--probe", type=Path, help=argparse.SUPPRESS)
    a = p.parse_args()
    sdk = a.sdk.resolve(strict=True)
    if a.probe:
        probe(a.probe, sdk, int(os.environ["VICTORY_GDEFLATE_WAIT_US"]))
    else:
        with tempfile.TemporaryDirectory() as tmp:
            pack = Path(tmp) / "generated.pack"
            pack_fixture(pack, sdk, a.bc)
            for lanes, budget in ((1,0),(1,250),(1,2000),(2,250)):
                subprocess.run([sys.executable,"-B",__file__,"--sdk",sdk,"--bc",a.bc,"--probe",pack],
                               env=dict(os.environ, VICTORY_GDEFLATE_WAIT_US=str(budget),
                                        VICTORY_GDEFLATE_LANES=str(lanes), VICTORY_GDEFLATE_LRU="1"), check=True)
