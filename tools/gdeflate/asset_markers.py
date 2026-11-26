"""Summarize private bridge markers or inspect a single request with a local texture index."""
import argparse
from collections import Counter
from contextlib import closing
import csv
import json
import hashlib
import os
from pathlib import Path
import sqlite3

from publish import HEADER, RECORD


def summary(rows):
    counts = Counter()
    byte_counts = Counter()
    for row in rows:
        event = row["event"]
        if event not in ("gpu_submit", "verified_delivery", "reuse", "eviction", "delivery_failed", "limit"):
            raise ValueError("unrecognized marker event")
        counts[event] += 1
        if event != "limit":
            byte_counts[event] += int(row["decoded_bytes"])
    return {"events": dict(counts), "decoded_bytes_by_event": dict(byte_counts),
            "capture_capped": counts["limit"] != 0}


def path_key(value):
    return value.replace("\\", "/").casefold().rstrip("/")


def inspect_marker(row, names, index):
    if row["event"] == "limit":
        return {"event": "limit"}
    result = {"block": row, "request_name": names.get(row["name_id"]),
              "container": names.get(row["source_id"]), "indexed_texture_start_candidates": []}
    if not index or row["source_kind"] != "file" or not result["container"]:
        return result
    source = path_key(result["container"])
    with closing(sqlite3.connect(index.resolve(strict=True).as_uri() + "?mode=ro", uri=True)) as db:
        # Require the full indexed relative path as a suffix of the guest path.
        # A basename shared by different stores must not identify either store.
        stores = [store for store, in db.execute("SELECT DISTINCT store FROM texture")
                  if source == path_key(store) or source.endswith("/" + path_key(store))]
        if len(stores) != 1:
            result["matching_store_count"] = len(stores)
            return result
        db.row_factory = sqlite3.Row
        query = """SELECT hash,store,width,height,dxgi,offset_in_decoded_chunk FROM texture
                   WHERE store=? AND source_offset=? AND stored=? AND decoded=?
                   ORDER BY offset_in_decoded_chunk,hash"""
        params = (stores[0], int(row["file_offset"]), int(row["stored_bytes"]), int(row["decoded_bytes"]))
        result["indexed_texture_start_candidates"] = [dict(record) for record in db.execute(query, params)]
    return result


def pack_identity(row, path):
    """Resolve a private marker against the immutable pack index without scanning the pack."""
    prefix = bytes.fromhex(row["asset_id"])
    if len(prefix) != 8:
        raise ValueError("expected a 64-bit source hash prefix")
    with path.open("rb") as pack:
        magic, table, count = HEADER.unpack(pack.read(HEADER.size))
        if magic != b"FX1GDF01" or table < HEADER.size or table + count * RECORD.size != os.fstat(pack.fileno()).st_size:
            raise ValueError("invalid GDeflate pack header")
        def at(index):
            pack.seek(table + index * RECORD.size)
            return RECORD.unpack(pack.read(RECORD.size))
        lo, hi = 0, count
        while lo < hi:
            mid = (lo + hi) // 2
            if at(mid)[0][:8] < prefix:
                lo = mid + 1
            else:
                hi = mid
        expected = tuple(int(row[k]) for k in ("pack_offset", "compressed_bytes", "stored_bytes", "decoded_bytes"))
        for i in range(lo, count):
            source, decoded, pos, size, stored, expanded, _ = at(i)
            if source[:8] != prefix:
                break
            if (pos, size, stored, expanded) != expected:
                continue
            if pos < HEADER.size or size <= 0 or pos + size > table:
                raise ValueError("invalid GDeflate block bounds")
            pack.seek(pos)
            h = hashlib.sha256()
            remaining = size
            while remaining:
                part = pack.read(min(remaining, 65536))
                if not part:
                    raise ValueError("truncated GDeflate block")
                h.update(part)
                remaining -= len(part)
            return {"file": path.name, "offset": pos, "bytes": size,
                    "gdeflate_sha256": h.hexdigest(), "source_sha256": source.hex(),
                    "expected_decoded_sha256": decoded.hex()}
    raise ValueError("marker and pack index do not match")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("markers", type=Path)
    parser.add_argument("--seq", type=int, help="inspect this private event instead of emitting aggregate JSON")
    parser.add_argument("--names", type=Path, help="private VICTORY_GDEFLATE_MARKER_NAMES dictionary")
    parser.add_argument("--pack", type=Path, help="resolve one event to its packed bytes and full SHA-256 hashes offline")
    parser.add_argument("--texture-index", type=Path, help="optional local ETPF texture-index SQLite file")
    args = parser.parse_args()
    if args.pack and args.seq is None:
        parser.error("--pack requires --seq")
    with args.markers.open(newline="", encoding="ascii") as file:
        rows = csv.DictReader(file)
        if args.seq is None:
            result = summary(rows)
        else:
            row = next((row for row in rows if int(row["seq"]) == args.seq), None)
            if row is None:
                parser.error("requested sequence number is absent")
            names = {}
            if args.names:
                with args.names.open(newline="", encoding="ascii") as dictionary:
                    names = {entry["id"]: bytes.fromhex(entry["name_hex"]).decode("utf-8", "backslashreplace")
                             for entry in csv.DictReader(dictionary)}
            result = inspect_marker(row, names, args.texture_index)
            if args.pack and row["event"] != "limit":
                result["gdeflate_block"] = pack_identity(row, args.pack)
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
