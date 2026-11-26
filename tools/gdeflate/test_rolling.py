"""Check verified readback reuse, wraparound and partial-span eviction on D3D12."""
import argparse
import ctypes as C
import hashlib
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import uuid
import zlib

from repack import Codec

TILE = 65536
SIZES = [TILE + 17, 113, 61 * TILE, TILE, 117]


def payload(index):
    pattern = bytes((x * 13 + index) & 255 for x in range(256))
    return (pattern * ((SIZES[index] + 255) // 256))[:SIZES[index]]


def make_pack(pack, sdk, bc):
    codec = Codec(sdk, bc)
    records = []
    with pack.open("wb") as out:
        out.write(bytes(24))
        for i, size in enumerate(SIZES):
            key = zlib.compress(payload(i))
            stream, digest = codec.transform(key, 8, size)
            if i == 4:
                digest = bytes(32)  # checksum negative control
            records.append(struct.pack("<32s32sQIIII", hashlib.sha256(key).digest(), digest,
                                       out.tell(), len(stream), len(key), size, 8))
            out.write(stream)
        table = out.tell()
        out.writelines(sorted(records))
        out.seek(0)
        out.write(struct.pack("<8sQQ", b"FX1GDF01", table, len(records)))


def probe(pack, sdk, rolling):
    with os.add_dll_directory(str(sdk)):
        core = C.WinDLL(str(sdk / "dstoragecore.dll"))
        api = C.CDLL(str(sdk / "fx1-storage.dll"))
    ptr, uint = C.c_void_p, C.c_uint32
    device = ptr()
    iid = uuid.UUID("189819f1-1db6-4b57-be54-1821339b85f7").bytes_le
    create = C.WinDLL("d3d12.dll").D3D12CreateDevice
    create.argtypes = [ptr, uint, ptr, C.POINTER(ptr)]
    if create(None, 0xc000, iid, C.byref(device)) != 0:
        raise RuntimeError("D3D12 device unavailable")
    read = api.fx1_read_gdeflate
    read.argtypes = [ptr, C.c_wchar_p, ptr, uint, uint, uint, ptr, C.POINTER(uint)]
    reuse = api.fx1_reused_readback_bytes
    reuse.restype = C.c_uint64
    # A, B, C fill the ring. D wraps over A. Reloading A overlaps B.
    sequence = [(0,False),(1,False),(2,False),(0,True),(3,False),(1,True),
                (0,False),(2,True),(1,False),(1,True)]
    previous = None
    for index, ring_hit in sequence:
        raw = payload(index)
        source = zlib.compress(raw)
        dst = C.create_string_buffer(len(raw))
        support = uint()
        before = reuse()
        status = read(device,str(pack),source,len(source),len(raw),8,dst,C.byref(support))
        expected_reuse = ring_hit if rolling else (index == previous)
        if status != 1 or not support.value & 3 or dst.raw != raw:
            raise RuntimeError(f"GPU decode mismatch for record {index}")
        if reuse() - before != (len(raw) if expected_reuse else 0):
            raise RuntimeError(f"Unexpected retention for record {index}")
        previous = index
    sentinel = b"_" * SIZES[4]
    dst = C.create_string_buffer(sentinel, len(sentinel))
    source = zlib.compress(payload(4))
    if read(device,str(pack),source,len(source),len(dst),8,dst,C.byref(support)) >= 0 or dst.raw != sentinel:
        raise RuntimeError("Corrupt block was published")
    print(f"rolling={int(rolling)} verified 10 reads; {reuse()} decoded bytes reused; bad digest rejected")


if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--sdk", type=Path, required=True)
    p.add_argument("--bc", type=Path, required=True)
    p.add_argument("--probe", type=Path, help=argparse.SUPPRESS)
    args = p.parse_args()
    sdk = args.sdk.resolve(strict=True)
    if args.probe:
        probe(args.probe, sdk, os.environ.get("VICTORY_GDEFLATE_ROLLING") == "1")
    else:
        with tempfile.TemporaryDirectory() as temp:
            pack = Path(temp) / "generated.pack"
            make_pack(pack, sdk, args.bc)
            for flag in ("0", "1"):
                subprocess.run([sys.executable,"-B",__file__,"--sdk",str(sdk),"--bc",str(args.bc),
                                "--probe",str(pack)],check=True,
                               env={**os.environ,"VICTORY_GDEFLATE_ROLLING":flag})
