"""Match a diagnostic guest texture prefix to candidates in the offline character and interior stores."""
import argparse
from bisect import bisect_right
import re
from pathlib import Path
import sqlite3

from repack import Codec, texture_records


def signature(data, start):
    # Two slices distinguish some textures that share a padded start.
    value = 0xcbf29ce484222325
    for offset in (0, 512):
        for byte in data[start + offset:start + offset + 64]:
            value = ((value ^ byte) * 0x100000001b3) & 0xffffffffffffffff
    return f"{value:016x}"


def index_materials(db, root, codec):
    db.execute("BEGIN")
    db.execute("DROP TABLE IF EXISTS material_signature")
    db.execute("""CREATE TABLE material_signature(texture_row INTEGER, probe_offset INTEGER, signature TEXT,
        PRIMARY KEY(texture_row,probe_offset)) WITHOUT ROWID""")
    stores = db.execute("""SELECT DISTINCT store FROM texture
        WHERE store LIKE '%/peds/%' OR store LIKE '%/interiors/%'""").fetchall()
    for store, in stores:
        path = (root/store).resolve()
        path.relative_to(root)
        chunks = list(texture_records(path))
        starts = [chunk[0] for chunk in chunks]
        end = chunks[-1][0] + chunks[-1][3]
        rows = db.execute("""SELECT rowid,virtual_start FROM texture WHERE store=? ORDER BY virtual_start""",
                          (store,)).fetchall()
        boundaries = sorted({v for _, v in rows} | {end})
        reads = []
        for row, address in rows:
            next_pos = bisect_right(boundaries, address)
            if next_pos == len(boundaries):
                continue
            for probe in (0, 4096, 65536):
                at = address + probe
                i = bisect_right(starts, at) - 1
                if i < 0 or at + 576 > min(boundaries[next_pos], starts[i] + chunks[i][3]):
                    continue
                reads.append((i, at - starts[i], row, probe))
        with path.open("rb") as f:
            cached = None
            for i, start, row, probe in sorted(reads):
                if cached != i:
                    _, offset, stored, decoded, flags = chunks[i]
                    f.seek(offset)
                    data = codec.decode_source(f.read(stored), flags, decoded)
                    cached = i
                db.execute("INSERT INTO material_signature VALUES(?,?,?)", (row, probe, signature(data, start)))
    db.execute("CREATE INDEX material_by_signature ON material_signature(probe_offset,signature)")
    db.commit()
    print(db.execute("""SELECT probe_offset,COUNT(*),COUNT(DISTINCT signature)
        FROM material_signature GROUP BY probe_offset""").fetchall(), "(offset, candidates, distinct signatures)")


def review(db, log):
    probes = {}
    with log.open(encoding="utf8", errors="replace") as stream:
        for line in stream:
            if "guest texture prefix for offline asset lookup" not in line:
                continue
            fields = dict(re.findall(r"\b(frame|input|slot|offset|width|height|signature)=([0-9a-f]+)", line))
            if not {"frame", "input", "slot", "offset", "signature"} <= fields.keys():
                continue
            key = tuple(int(fields[k]) for k in ("frame", "input", "slot", "width", "height") if k in fields)
            probes.setdefault(key, {})[int(fields["offset"])] = fields["signature"]
    seen = set()
    for key, values in sorted(probes.items()):
        frame, material_input, slot, *dims = key
        signature_set = tuple(sorted(values.items()))
        identity = (material_input, slot, tuple(dims), signature_set)
        if identity in seen:
            continue
        seen.add(identity)
        query = " INTERSECT ".join(
            "SELECT texture_row FROM material_signature WHERE probe_offset=? AND signature=?" for _ in signature_set)
        params = [v for pair in signature_set for v in pair]
        where = " FROM texture WHERE rowid IN (" + query + ")"
        if len(dims) == 2:
            where += " AND width=? AND height=?"
            params.extend(dims)
        count = db.execute("SELECT COUNT(*)" + where, params).fetchone()[0]
        examples = db.execute("SELECT hash,store" + where + " LIMIT 3", params)
        print(f"frame={frame} input={material_input} slot={slot} matching records={count}",
              ", ".join(f"{asset} {store}" for asset,store in examples))
    if not probes:
        print("No material signature samples in this capture.")


if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--index", type=Path, required=True)
    p.add_argument("--root", type=Path)
    p.add_argument("--sdk", type=Path)
    p.add_argument("--bc", type=Path)
    p.add_argument("--signature")
    p.add_argument("--log", type=Path)
    p.add_argument("--offset", type=int, default=0)
    a = p.parse_args()
    mode = "ro" if a.signature or a.log else "rw"
    db = sqlite3.connect(a.index.resolve().as_uri() + "?mode=" + mode, uri=True)
    try:
        if a.log:
            review(db, a.log)
        elif a.signature:
            key = f"{int(a.signature,16):016x}"
            query = """FROM material_signature s JOIN texture t ON t.rowid=s.texture_row
                WHERE s.probe_offset=? AND s.signature=?"""
            params = a.offset, key
            print("matches:", db.execute("SELECT COUNT(*) "+query, params).fetchone()[0])
            for row in db.execute("SELECT hash,store,width,height,dxgi "+query+" LIMIT 20", params):
                print(*row)
        elif a.root and a.sdk and a.bc:
            index_materials(db, a.root.resolve(), Codec(a.sdk, a.bc))
        else:
            p.error("supply --signature to query or --root, --sdk and --bc to index")
    finally:
        db.close()
