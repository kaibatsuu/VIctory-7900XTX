"""Decode one published block per source codec with the fx1 D3D12 bridge and verify its digest."""
import argparse
import ctypes as C
import hashlib
import os
from pathlib import Path
import sqlite3
import uuid


def verify(root, sidecars, pack, check_direct=False):
    with os.add_dll_directory(str(pack.parent)):
        core = C.WinDLL(str(pack.parent / "dstoragecore.dll"))
        bridge = C.CDLL(str(pack.parent / "fx1-storage.dll"))
    d3d = C.WinDLL("d3d12.dll")
    device = C.c_void_p()
    iid = uuid.UUID("189819f1-1db6-4b57-be54-1821339b85f7").bytes_le
    if d3d.D3D12CreateDevice(None, 0xc000, iid, C.byref(device)) < 0:
        raise RuntimeError("cannot open a D3D12 device")
    read = bridge.fx1_read_gdeflate
    read.argtypes = [C.c_void_p, C.c_wchar_p, C.c_void_p, C.c_uint32, C.c_uint32,
                     C.c_uint32, C.c_void_p, C.POINTER(C.c_uint32)]
    if check_direct:
        prepare_type = C.CFUNCTYPE(None, C.c_void_p, C.c_uint32)
        reused_bytes = bridge.fx1_reused_readback_bytes
        reused_bytes.restype = C.c_uint64
        direct = bridge.fx1_read_gdeflate_into_title
        direct.argtypes = [*read.argtypes, prepare_type]
    tested = set()
    seen = set()
    for file in sorted(sidecars.rglob("*.sqlite")):
        db = sqlite3.connect(file.as_uri() + "?mode=ro", uri=True)
        try:
            row = db.execute("SELECT name FROM source").fetchone()
            if not row:
                continue
            path = (root / row[0]).resolve()
            path.relative_to(root)
            with path.open("rb") as source:
                for flags, in db.execute("SELECT DISTINCT flags FROM block"):
                    seen.add(flags)
                    if flags in tested:
                        continue
                    candidate = db.execute(
                        "SELECT off,stored,decoded,digest FROM block WHERE flags=? AND decoded<=4194304 LIMIT 1",
                        (flags,)).fetchone()
                    if candidate is None:
                        continue
                    off, stored, decoded, digest = candidate
                    source.seek(off)
                    block = source.read(stored)
                    if len(block) != stored:
                        raise ValueError("original stream was truncated")
                    result, support = C.create_string_buffer(decoded), C.c_uint32()
                    status = read(device, str(pack), block, stored, decoded, flags, result, C.byref(support))
                    if status != 1 or not support.value & 3 or hashlib.sha256(result.raw).digest() != digest:
                        raise RuntimeError(f"GPU verification failed: flags={flags} status={status} support={support.value}")
                    if check_direct:
                        initial_reuse = reused_bytes()
                        before = bytes([0xa5]) * decoded
                        result.raw = before
                        calls = []

                        @prepare_type
                        def prepare(address, length):
                            calls.append((address == C.addressof(result), length, result.raw == before))

                        # An unknown source hash must leave both the contents and write guard alone.
                        miss = direct(device, str(pack), b"offline-lookup-miss", 19, decoded, flags,
                                      result, C.byref(support), prepare)
                        if miss != 0 or calls or result.raw != before:
                            raise RuntimeError("Lookup miss touched the title destination")
                        status = direct(device, str(pack), block, stored, decoded, flags,
                                        result, C.byref(support), prepare)
                        if status != 1 or calls != [(True, decoded, True)] or hashlib.sha256(result.raw).digest() != digest:
                            raise RuntimeError(f"Direct destination verification failed: flags={flags}")
                        print(f"Direct-to-title verified flags {flags}; guard once on hit, untouched on miss", flush=True)
                        if reused_bytes() - initial_reuse != decoded:
                            raise RuntimeError("Repeated request did not reuse the verified readback")
                    tested.add(flags)
                    print(f"GPU verified codec flags {flags}, {decoded} bytes, support {support.value:#x}", flush=True)
        finally:
            db.close()
    if seen - tested:
        raise RuntimeError(f"No block within the GPU bridge size limit for source codec flags {sorted(seen - tested)}")
    if not tested:
        raise RuntimeError("no published codec could be tested")


if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    for argument in ("root", "sidecars", "pack"):
        p.add_argument("--" + argument, type=Path, required=True)
    p.add_argument("--check-direct", action="store_true", help="also exercise verified single-copy linear destinations")
    args = p.parse_args()
    verify(args.root.resolve(), args.sidecars.resolve(), args.pack.resolve(), args.check_direct)
