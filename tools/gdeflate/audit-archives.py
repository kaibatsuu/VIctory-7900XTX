"""Independently scan the on-disk bytes outside indexed zlib extents, including stored raw chunks."""
import argparse
import json
import os
from pathlib import Path
import struct
import subprocess
import tempfile


def extents(index, output, root):
    data = json.loads(index.read_bytes())
    path = Path(data["path"]).resolve()
    path.relative_to(root)
    size = path.stat().st_size
    if size != data["size"] or int(path.stat().st_mtime) != data["mtime"]:
        raise ValueError(f"Stale index for {path}")
    intervals = sorted((off, off + length) for entry in data["entries"]
                       for off, length, decoded in entry.get("streams", [])
                       if length != decoded)
    end = start = 0
    with output.open("w") as dst:
        for lo, hi in intervals:
            if not 0 <= lo <= hi <= size:
                raise ValueError(f"Invalid stream in {index}")
            if lo > end:
                if start < end:
                    print(start, end, file=dst)
                start = lo
            end = max(end, hi)
        if start < end:
            print(start, end, file=dst)
    return path


def inspect_rds(file, base, size):
    file.seek(base)
    header = file.read(24)
    if len(header) != 24:
        return None
    magic, count, _, fmt, _, _ = struct.unpack("<4sIIIII", header)
    if magic != b"RDS5" or fmt not in (0x16000000, 0x16000001) or not 2 <= count <= 2**20:
        return None
    metadata = (16 + count * 8 + 255) & ~255
    file.seek(base + metadata)
    if file.read(4) != b"ETPF":
        return None
    file.seek(base + 24)
    table = file.read((count - 1) * 8)
    if len(table) != (count - 1) * 8:
        return None
    chunks = []
    previous = None
    for a, b in struct.iter_unpack("<II", table):
        if not (a or b):
            break
        at = base + ((a & 0x3ffffff) << 8)
        virt = 4096 + ((b & 0xfffff) << 14) + ((a >> 30) << 12)
        if at < base + metadata or at > size:
            return None
        if previous:
            pos, vpos, pad, flags = previous
            if at <= pos + pad or virt <= vpos or flags not in (0,1,3,4,5,6,7,8):
                return None
            chunks.append((pos, at-pos-pad, virt-vpos, flags))
        previous = (at, virt, b >> 20, (a >> 26) & 15)
    return chunks or None


def audit(root, indexes, scanner, dest):
    dest.mkdir(parents=True, exist_ok=True)
    found = set()
    for source in sorted(indexes.glob("*.json")):
        with tempfile.TemporaryDirectory(dir=dest) as directory:
            ranges = Path(directory)/"ranges"
            path = extents(source, ranges, root)
            found.add(path)
            extra = dest/(source.stem+".streams")
            temp = Path(directory)/"candidates"
            with temp.open("wb") as report:
                run = subprocess.run([scanner, path, ranges], stdout=report, check=True,
                                     stderr=subprocess.PIPE, text=True)
            rds = []
            with temp.open() as report, path.open("rb") as archive:
                for line in report:
                    if line.startswith("rds5 "):
                        base = int(line.split()[1])
                        chunks = inspect_rds(archive, base, path.stat().st_size)
                        if chunks:
                            rds.append({"base":base,"chunks":chunks})
            (dest/(source.stem+".stores.json")).write_text(json.dumps(rds))
            os.replace(temp, extra)
            print(f"{path.relative_to(root)} {run.stderr.strip()} embedded_stores={len(rds)}",flush=True)
    missing = set(root.rglob("*.rpf")) - found
    if missing:
        raise ValueError(f"Archives absent from source indexes: {missing}")


if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    for name in ("root","indexes","scanner","out"):
        p.add_argument("--"+name, type=Path, required=True)
    a = p.parse_args()
    audit(a.root.resolve(), a.indexes.resolve(), a.scanner.resolve(), a.out.resolve())
