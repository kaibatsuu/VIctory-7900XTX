"""Locate archived texture starts by ETPF asset hash. This index contains metadata only."""
import argparse
from bisect import bisect_right
import os
from pathlib import Path
import sqlite3
import struct
import tempfile

from repack import texture_records


def texture_rows(path, root):
    with path.open("rb") as file:
        magic, count = struct.unpack("<4sI", file.read(8))
        if magic != b"RDS5" or count < 2:
            raise ValueError(f"invalid chunk header: {path.name}")
        file.seek((16 + count * 8 + 255) & ~255)
        magic, version, textures, _ = struct.unpack("<4sIII", file.read(16))
        if magic != b"ETPF" or version != 0x27a or textures * 24 > path.stat().st_size - file.tell():
            raise ValueError(f"unrecognized texture index: {path.name}")
        descriptors = file.read(textures * 16)
        hashes = file.read(textures * 8)
        if len(hashes) != textures * 8:
            raise ValueError("truncated texture index")
    chunks = list(texture_records(path))
    starts = [chunk[0] for chunk in chunks]
    if any(chunk[3] <= 0 for chunk in chunks):
        raise ValueError(f"non-increasing virtual address: {path.name}")
    name = path.relative_to(root).as_posix()
    for (d0, d1, location), (asset,) in zip(struct.iter_unpack("<IIQ", descriptors),
                                            struct.iter_unpack("<Q", hashes), strict=True):
        address = ((location & 0x1fffff) + 1) * 4096
        i = bisect_right(starts, address) - 1
        if i >= 0 and address < starts[i] + chunks[i][3]:
            _, offset, stored, decoded, flags = chunks[i]
            within = address - starts[i]
        else:
            offset = stored = decoded = flags = within = None
        yield (f"{asset:016x}", name, 1 + (d0 & 16383), 1 + ((d0 >> 14) & 16383),
               (d1 >> 3) & 127, (d1 >> 24) & 15, d0 >> 28, address,
               offset, stored, decoded, flags, within)


def build(root, out):
    out.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=out.parent, delete=False) as f:
        tmp = Path(f.name)
    try:
        db = sqlite3.connect(tmp)
        try:
            db.execute("""CREATE TABLE texture(hash TEXT, store TEXT, width INTEGER, height INTEGER,
                dxgi INTEGER, tile_mode INTEGER, mip_field INTEGER, virtual_start INTEGER,
                source_offset INTEGER, stored INTEGER, decoded INTEGER, codec_flags INTEGER,
                offset_in_decoded_chunk INTEGER)""")
            for path in sorted(root.rglob("*.ttpf")):
                db.executemany("INSERT INTO texture VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)", texture_rows(path, root))
            db.execute("CREATE INDEX texture_by_hash ON texture(hash)")
            db.commit()
            print(db.execute("""SELECT COUNT(DISTINCT store), COUNT(*), COUNT(source_offset),
                SUM(width=4 AND height=4) FROM texture""").fetchone(),
                  "(stores, texture records, mapped starts, 4x4 records)")
        finally:
            db.close()
        os.replace(tmp, out)
    finally:
        tmp.unlink(missing_ok=True)


if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--root", type=Path)
    p.add_argument("--index", type=Path, required=True)
    p.add_argument("--hash", help="hexadecimal 64-bit asset hash")
    args = p.parse_args()
    if args.hash:
        db = sqlite3.connect(args.index.resolve().as_uri()+"?mode=ro", uri=True)
        db.row_factory = sqlite3.Row
        try:
            print([dict(row) for row in db.execute("SELECT * FROM texture WHERE hash=?",
                                                  (f"{int(args.hash,16):016x}",))])
        finally:
            db.close()
    elif args.root:
        build(args.root.resolve(), args.index.resolve())
    else:
        p.error("supply --root to build or --hash to query")
