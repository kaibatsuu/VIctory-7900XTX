"""Build a private, Windows-file-identity-bound offset index for verified checkpoint source ranges."""
import argparse
from collections import defaultdict
import csv
import ctypes as C
import hashlib
import msvcrt
import os
from pathlib import Path, PureWindowsPath
import struct
import tempfile
import zlib


IDENTITY = struct.Struct("<6Q")
ENTRY = struct.Struct("<QIII32s")
PACK_RECORD = struct.Struct("<32s32sQIIII")


class ByHandle(C.Structure):
    _fields_ = [(n,C.c_uint32) for n in ("attr","creation_lo","creation_hi","access_lo","access_hi",
                "write_lo","write_hi","volume","size_hi","size_lo","links","index_hi","index_lo")]


class BasicInfo(C.Structure):
    _fields_ = [(n,C.c_int64) for n in ("created","accessed","written","changed")] + [("attributes",C.c_uint32)]


def stable_read(path):
    create = C.WinDLL("kernel32", use_last_error=True).CreateFileW
    create.argtypes = [C.c_wchar_p,C.c_uint32,C.c_uint32,C.c_void_p,C.c_uint32,C.c_uint32,C.c_void_p]
    create.restype = C.c_void_p
    handle = create(str(path), 0x80000000, 1, None, 3, 0, None)
    if handle == C.c_void_p(-1).value:
        raise C.WinError(C.get_last_error())
    return os.fdopen(msvcrt.open_osfhandle(handle, os.O_RDONLY | os.O_BINARY), "rb")


def identity(file):
    k = C.WinDLL("kernel32", use_last_error=True)
    h = C.c_void_p(msvcrt.get_osfhandle(file.fileno()))
    info, basic = ByHandle(), BasicInfo()
    if not k.GetFileInformationByHandle(h,C.byref(info)) or not k.GetFileInformationByHandleEx(h,0,C.byref(basic),C.sizeof(basic)):
        raise C.WinError(C.get_last_error())
    index = info.index_hi << 32 | info.index_lo
    if not index or not info.volume or basic.changed <= 0:
        raise ValueError("Stable Windows file identity unavailable")
    return (info.volume, index, info.size_hi << 32 | info.size_lo, basic.created, basic.written, basic.changed)


def make_index(root, pack_path, captures, output):
    wanted = defaultdict(set)
    for capture in captures:
        with (capture / "gdeflate-names.csv").open(encoding="ascii", newline="") as names:
            paths = {r["id"]:PureWindowsPath(bytes.fromhex(r["name_hex"]).decode("utf8")) for r in csv.DictReader(names)}
        with (capture / "gdeflate-markers.csv").open(encoding="ascii", newline="") as markers:
            for row in csv.DictReader(markers):
                src = paths.get(row["source_id"])
                if row["event"] != "verified_delivery" or row["source_kind"] != "file" or not src or src.anchor.lower() != "g:\\":
                    continue
                path = (root / Path(*src.parts[1:])).resolve(strict=True)
                path.relative_to(root)
                wanted[path].add(tuple(int(row[k]) for k in ("file_offset","stored_bytes","decoded_bytes","pack_offset")))
    hashes = defaultdict(list)
    for path, rows in wanted.items():
        with stable_read(path) as file:
            fid = identity(file)
            for off, stored, decoded, pack_off in sorted(rows):
                if not 0 < stored <= 64 << 20 or not 0 < decoded <= 4 << 20 or off < 0 or off + stored > fid[2]:
                    raise ValueError("Invalid captured source range")
                file.seek(off)
                raw = file.read(stored)
                if len(raw) != stored:
                    raise ValueError("Truncated source range")
                hashes[hashlib.sha256(raw).digest()].append((fid, off, stored, decoded, pack_off))
    grouped = defaultdict(dict)
    matched = 0
    with stable_read(pack_path) as pack:
        pack_id = identity(pack)
        magic, table, count = struct.unpack("<8sQQ", pack.read(24))
        if magic != b"FX1GDF01" or table < 24 or table + PACK_RECORD.size * count != pack_id[2]:
            raise ValueError("Invalid pack index extent")
        pack.seek(table)
        for pos in range(0, count, 32768):
            records = pack.read(min(32768,count-pos)*PACK_RECORD.size)
            for digest, _, pack_off, compressed, stored, decoded, flags in PACK_RECORD.iter_unpack(records):
                if digest not in hashes:
                    continue
                if not compressed or not 24 <= pack_off <= table-compressed:
                    raise ValueError("Bad indexed pack extent")
                for fid, off, n, size, observed_pack_off in hashes[digest]:
                    if (stored,decoded,pack_off) == (n,size,observed_pack_off):
                        grouped[fid][off,stored,decoded,flags] = digest
                        matched += 1
        attempted = sum(len(r) for r in wanted.values())
        if matched != attempted or not matched:
            raise ValueError(f"Only {matched}/{attempted} source ranges verified against this pack")
        payload = bytearray(b"FX1OFF01" + IDENTITY.pack(*pack_id) + struct.pack("<I",len(grouped)))
        for fid, group in sorted(grouped.items()):
            payload.extend(IDENTITY.pack(*fid) + struct.pack("<I",len(group)))
            for key, digest in sorted(group.items()):
                payload.extend(ENTRY.pack(*key,digest))
        payload.extend(struct.pack("<I",zlib.crc32(payload)))
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=output.parent,delete=False) as f:
        tmp = Path(f.name)
    try:
        tmp.write_bytes(payload)
        tmp.replace(output)
    finally:
        tmp.unlink(missing_ok=True)
    print(f"{matched} checkpoint source ranges verified across {len(grouped)} files, {len(payload)} index bytes")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for key in ("root","pack","output"):
        parser.add_argument("--"+key, type=Path, required=True)
    parser.add_argument("--capture", type=Path, action="append", required=True, help="private marker and name directory")
    a = parser.parse_args()
    make_index(a.root.resolve(strict=True), a.pack.resolve(strict=True), a.capture, a.output)
