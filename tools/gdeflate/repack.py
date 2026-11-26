"""Repack indexed Xbox storage streams in separate resumable SQLite sidecars.

Read the source only. TTPF addressing follows the published SixNG RDS5 chunk layout.
RPF inputs use SixNG's stream index; its carving coverage is not a complete TOC.
Compressed blobs can be read independently by DirectStorage. Original request keys
are relative source name, byte offset, stored length, and decoded length.
"""
import argparse
import ctypes as C
import hashlib
import json
import os
import shutil
import sqlite3
import struct
import uuid
import zlib
from pathlib import Path

U = C.c_size_t
P = C.c_void_p
COMPRESSION_LEVELS = {"fastest": -1, "default": 0, "best-ratio": 1}


class Codec:
    def __init__(self, sdk, bc_library, compression_level="fastest"):
        self.compression_level = compression_level
        self.directory = os.add_dll_directory(str(sdk.resolve()))
        self.core = C.WinDLL(str((sdk / "dstoragecore.dll").resolve()))
        self.ds = C.WinDLL(str((sdk / "dstorage.dll").resolve()))
        guid = uuid.UUID("84ef5121-9b43-4d03-b5c1-cc34606b262d")
        iid = (C.c_byte * 16).from_buffer_copy(guid.bytes_le)
        self.instance = P()
        hr = self.ds.DStorageCreateCompressionCodec(1, 1, C.byref(iid), C.byref(self.instance))
        if hr < 0:
            raise OSError(f"Cannot create GDeflate codec: {hr & 0xffffffff:08x}")
        table = C.cast(self.instance, C.POINTER(C.POINTER(P))).contents
        self.bound = C.WINFUNCTYPE(U, P, U)(table[5])
        self.compress = C.WINFUNCTYPE(C.c_long, P, P, U, C.c_int, P, U, C.POINTER(U))(table[3])
        self.decompress = C.WINFUNCTYPE(C.c_long, P, P, U, P, U, C.POINTER(U))(table[4])
        self.bc = C.CDLL(str(bc_library.resolve())).fx1_decode_bc
        self.bc.argtypes = [C.c_ubyte, P, U, P, U]

    def decode_source(self, source, flags, length):
        if not (0 < length <= 64 << 20):
            raise ValueError(f"unsupported decoded length {length}")
        data = source
        if flags & 8:
            stream = zlib.decompressobj()
            data = stream.decompress(data, (64 << 20) + 1)
            if not stream.eof or stream.unconsumed_tail or any(stream.unused_data):
                raise ValueError("incomplete or concatenated zlib stream")
        bc = flags & 7
        if bc:
            decoded = C.create_string_buffer(length)
            if self.bc(bc, data, len(data), decoded, length):
                raise ValueError(f"BCPack {bc} decode failed")
            data = decoded.raw
        if len(data) != length:
            raise ValueError(f"decoded {len(data)} bytes, expected {length}")
        return data

    def transform(self, source, flags, length):
        data = self.decode_source(source, flags, length)
        return self.encode(data), hashlib.sha256(data).digest()

    def encode(self, data):
        length = len(data)
        out = C.create_string_buffer(self.bound(self.instance, length))
        count = U()
        level = COMPRESSION_LEVELS[self.compression_level]
        if self.compress(self.instance, data, length, level, out, len(out), C.byref(count)) < 0:
            raise ValueError("GDeflate compressor rejected input")
        encoded = out.raw[:count.value]
        self.verify(encoded, data)
        return encoded

    def verify(self, encoded, data):
        length = len(data)
        count = U()
        verify = C.create_string_buffer(length)
        if self.decompress(self.instance, encoded, len(encoded), verify, length, C.byref(count)) < 0 or count.value != length or verify.raw != data:
            raise ValueError("GDeflate round-trip mismatch")


def texture_records(path):
    with path.open("rb") as f:
        header = f.read(24)
        magic, n = struct.unpack_from("<4sI", header)
        if magic != b"RDS5" or n < 2 or 16 + n * 8 > path.stat().st_size:
            raise ValueError("invalid RDS5 chunk table")
        data = f.read((n - 1) * 8)
        if len(data) != (n - 1) * 8:
            raise ValueError("truncated chunk table")
        previous = None
        for a, b in struct.iter_unpack("<II", data):
            if not (a or b):
                break
            offset = (a & 0x3ffffff) << 8
            virtual = 4096 + (b & 0xfffff) * 16384 + (a >> 30) * 4096
            if previous:
                start, vstart, flags, pad = previous
                yield vstart, start, offset - start - pad, virtual - vstart, flags
            previous = (offset, virtual, (a >> 26) & 15, b >> 20)


def texture_chunks(path):
    for _, start, stored, decoded, flags in texture_records(path):
        yield start, stored, decoded, flags


def rpf_chunks(index):
    entries = json.loads(index.read_bytes())["entries"]
    for entry in entries:
        for offset, size, decoded in entry.get("streams", []):
            # SixNG marks stored RPF streams with equal positive lengths (scan::is_raw).
            yield offset, size, decoded, 0 if size == decoded and size > 0 else 8


def convert(path, chunks, relative, out, codec, samples, limit, gpu=None):
    out.parent.mkdir(parents=True, exist_ok=True)
    db = sqlite3.connect(out)
    try:
        db.executescript("""
            CREATE TABLE IF NOT EXISTS source(name TEXT, size INTEGER, mtime INTEGER);
            CREATE TABLE IF NOT EXISTS block(off INTEGER, stored INTEGER, decoded INTEGER,
                flags INTEGER, digest BLOB, gdeflate BLOB,
                PRIMARY KEY(off,stored,decoded,flags)) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS failure(off INTEGER, stored INTEGER, decoded INTEGER,
                flags INTEGER, reason TEXT, PRIMARY KEY(off,stored,decoded,flags));
        """)
        db.execute("CREATE TABLE IF NOT EXISTS encoding (id INTEGER PRIMARY KEY, profile TEXT NOT NULL)")
        profile = "nvcomp" if gpu is not None else "directstorage:" + codec.compression_level
        recorded_profile = db.execute("SELECT profile FROM encoding WHERE id=1").fetchone()
        if recorded_profile and recorded_profile[0] != profile:
            raise ValueError(f"sidecar for {relative} uses {recorded_profile[0]}; choose a separate output directory")
        if not recorded_profile:
            if profile != "directstorage:fastest" and db.execute("SELECT 1 FROM block LIMIT 1").fetchone():
                raise ValueError(f"sidecar for {relative} has legacy blocks; choose a separate output directory")
            db.execute("INSERT INTO encoding VALUES(1,?)", (profile,))
        stat = path.stat()
        signature = (relative, stat.st_size, stat.st_mtime_ns)
        recorded = db.execute("SELECT * FROM source").fetchone()
        if recorded and recorded != signature:
            raise ValueError(f"stale sidecar for {relative}")
        if not recorded:
            db.execute("INSERT INTO source VALUES(?,?,?)", signature)
        present = set()
        count = errors = 0
        def jobs(f):
            considered = 0
            for key in chunks:
                offset, size, length, flags = key
                if not flags:
                    if path.suffix.lower() == ".rpf" and size == length and size > 0:
                        db.execute("DELETE FROM failure WHERE off=? AND stored=? AND decoded=? AND flags=8",
                                   (offset, size, length))
                    continue
                if samples and flags in present:
                    continue
                if limit and considered >= limit:
                    break
                considered += 1
                if db.execute("SELECT 1 FROM block WHERE off=? AND stored=? AND decoded=? AND flags=?",key).fetchone():
                    present.add(flags)
                    continue
                if not (0 < size <= 64 << 20 and 0 < length <= 64 << 20 and 0 <= offset <= stat.st_size - size):
                    yield key, "source stream bounds invalid"
                    continue
                f.seek(offset)
                yield key, f.read(size)

        def sequential(jobs):
            for key, source in jobs:
                if isinstance(source, str):
                    yield key, None, source
                    continue
                try:
                    encoded, digest = codec.transform(source, key[3], key[2])
                except (ValueError, zlib.error) as error:
                    yield key, None, str(error)
                else:
                    yield key, encoded, digest

        with path.open("rb") as f:
            inputs = jobs(f)
            if gpu is None:
                outputs = sequential(inputs)
            elif samples:
                # Advance the input once each previous sample result is known.
                outputs = (result for job in inputs for result in gpu.run([job]))
            else:
                outputs = gpu.run(inputs)
            for key, encoded, detail in outputs:
                if encoded is None:
                    db.execute("INSERT OR REPLACE INTO failure VALUES(?,?,?,?,?)", (*key, detail))
                    errors += 1
                else:
                    # A source index may repeat a key still in an outstanding CUDA batch.
                    added = db.execute("INSERT OR IGNORE INTO block VALUES(?,?,?,?,?,?)", (*key, detail, encoded))
                    db.execute("DELETE FROM failure WHERE off=? AND stored=? AND decoded=? AND flags=?", key)
                    present.add(key[3])
                    count += added.rowcount
                if (count + errors) % 128 == 0:
                    db.commit()
                    if shutil.disk_usage(out.parent).free < 40 * 1024 ** 3:
                        raise OSError("paused with less than 40 GiB of free disk space")
        db.commit()
        print(relative, "verified", count, "failed", errors, flush=True)
        return errors
    finally:
        db.close()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for name in ["root", "output", "sdk", "bc"]:
        p.add_argument("--"+name, required=True, type=Path)
    p.add_argument("--indexes", type=Path)
    p.add_argument("--samples", action="store_true", help="one verified stream of each codec per container")
    p.add_argument("--limit", type=int, default=0, help="maximum compressed chunks considered per container")
    p.add_argument("--encoder", choices=("directstorage", "nvcomp"), default="directstorage")
    p.add_argument("--compression-level", choices=COMPRESSION_LEVELS, help="DirectStorage encoder level (default: best-ratio)")
    p.add_argument("--workers", type=int, choices=range(1, 9), default=2, help="nvCOMP worker threads, each with its own codec")
    p.add_argument("--only-source", action="append", default=[], help="exact source name relative to the archive root")
    args = p.parse_args()
    if args.encoder != "directstorage" and args.compression_level is not None:
        p.error("--compression-level is only supported by the DirectStorage encoder")
    root = args.root.resolve()
    codec = Codec(args.sdk, args.bc, args.compression_level or "best-ratio")
    errors = 0
    inputs = [(f,texture_chunks(f)) for f in sorted(root.rglob("*.ttpf"))]
    if args.indexes:
        for index in sorted(args.indexes.glob("*.json")):
            # Read the index path from its header without loading every index at once.
            with index.open(encoding="utf8") as f:
                import re
                match = re.search(r'"path":("(?:\\.|[^"\\])*")', f.read(16384))
            if not match:
                raise ValueError(f"missing source in {index}")
            inputs.append((Path(json.loads(match[1])),rpf_chunks(index)))
    selected = [(path, chunks, path.resolve().relative_to(root).as_posix()) for path, chunks in inputs]
    if args.only_source:
        missing = set(args.only_source) - {name for _, _, name in selected}
        if missing:
            p.error(f"Unknown sources: {sorted(missing)}")
        selected = [entry for entry in selected if entry[2] in args.only_source]
    gpu = None
    if args.encoder == "nvcomp":
        from nvcomp_encoder import Encoder
        gpu = Encoder(lambda: Codec(args.sdk, args.bc), args.workers)
        print(f"nvCOMP CUDA GDeflate: {args.workers} independent workers, Microsoft DirectStorage verification", flush=True)
    try:
        for path, chunks, relative in selected:
            errors += convert(path,chunks,relative,args.output/(relative+".sqlite"),codec,args.samples,args.limit,gpu)
    finally:
        if gpu is not None:
            gpu.close()
    return bool(errors)


if __name__ == "__main__":
    raise SystemExit(main())
