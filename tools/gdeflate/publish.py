"""Publish verified blocks as one DirectStorage-readable file. Inputs remain read-only."""
import argparse
from contextlib import closing
import hashlib
import os
from pathlib import Path
import sqlite3
import struct
import tempfile

HEADER = struct.Struct("<8sQQ")
RECORD = struct.Struct("<32s32sQIIII")


def publish(root, sidecars, out, per_source):
    out.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=out.parent, delete=False) as pack:
        path = Path(pack.name)
    try:
        # The full archive has millions of streams. Keep one sortable key
        # per stream on disk rather than retaining every record in Python.
        with tempfile.TemporaryDirectory(dir=out.parent) as work, path.open("w+b") as pack:
            pack.write(bytes(HEADER.size))
            with closing(sqlite3.connect(Path(work) / "entries.sqlite")) as entries:
                entries.execute("PRAGMA journal_mode=OFF")
                entries.execute("PRAGMA cache_size=-16384")
                entries.execute("""CREATE TABLE record(
                    digest BLOB, flags INTEGER, decoded INTEGER, payload BLOB NOT NULL,
                    PRIMARY KEY (digest, flags, decoded)) WITHOUT ROWID""")
                count = write_sources(root, sidecars, per_source, pack, entries)
                table = pack.tell()
                for record, in entries.execute("SELECT payload FROM record ORDER BY digest,flags,decoded"):
                    pack.write(record)
            pack.seek(0)
            pack.write(HEADER.pack(b"FX1GDF01", table, count))
            pack.flush()
            os.fsync(pack.fileno())
        os.replace(path, out)
        print(f"{count} distinct verified streams published in {out}")
    finally:
        path.unlink(missing_ok=True)


def write_sources(root, sidecars, per_source, pack, entries):
    count = 0
    for sidecar in sorted(sidecars.rglob("*.sqlite")):
        db = sqlite3.connect(sidecar.as_uri() + "?mode=ro", uri=True)
        try:
            signature = db.execute("SELECT name,size,mtime FROM source").fetchone()
            if signature is None:
                continue
            name, size, modified = signature
            src = (root / name).resolve()
            src.relative_to(root)
            stat = src.stat()
            if (stat.st_size, stat.st_mtime_ns) != (size, modified):
                raise ValueError(f"changed source: {src}")
            with src.open("rb") as original:
                for off, stored, decoded, flags, digest, blob in db.execute(
                    "SELECT * FROM block ORDER BY off LIMIT ?", (per_source,)
                ):
                    original.seek(off)
                    raw = original.read(stored)
                    if len(raw) != stored:
                        raise ValueError("truncated source stream")
                    key = hashlib.sha256(raw).digest(), flags, decoded
                    location = pack.tell()
                    record = RECORD.pack(key[0], digest, location, len(blob), stored, decoded, flags)
                    inserted = entries.execute("INSERT OR IGNORE INTO record VALUES (?,?,?,?)", (*key, record))
                    if not inserted.rowcount:
                        continue
                    pack.write(blob)
                    count += 1
        finally:
            db.close()
    return count


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("root", "sidecars", "out"):
        parser.add_argument("--" + name, required=True, type=Path)
    parser.add_argument("--per-source", type=int, default=-1, help="optional cap for fast trials")
    args = parser.parse_args()
    publish(args.root.resolve(), args.sidecars.resolve(), args.out.resolve(), args.per_source)
