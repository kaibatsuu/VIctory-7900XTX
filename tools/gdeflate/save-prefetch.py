"""Build and replay bounded buffered read-ahead data from verified pack deliveries."""
import argparse
import csv
import json
from pathlib import Path
from time import monotonic

MAX_BYTES = 64 * 1024 * 1024
MAX_TRACE = 32 * 1024 * 1024
MAX_RANGES = 4096


def identity(pack):
    stat = pack.stat()
    return [str(pack.resolve()), stat.st_size, stat.st_mtime_ns, stat.st_ino]


def collect(marker, limit):
    if not marker.is_file() or marker.stat().st_size > MAX_TRACE:
        return []
    seen = set()
    ranges = []
    spent = 0
    started = None
    with marker.open(encoding="ascii", errors="replace", newline="") as file:
        for row in csv.DictReader(file):
            if row.get("event") != "verified_delivery":
                continue
            try:
                now = int(row["utc_us"])
                offset = int(row["pack_offset"])
                size = int(row["compressed_bytes"])
            except (ValueError, KeyError, TypeError):
                continue
            if started is None:
                started = now
            if now - started > 60_000_000:
                break
            if offset < 0 or size <= 0 or offset + size > limit or (offset, size) in seen:
                continue
            if len(ranges) >= MAX_RANGES or spent + size > MAX_BYTES:
                break
            seen.add((offset, size))
            ranges.append([offset, size])
            spent += size
    return ranges


def replay(pack, ranges):
    # Merge overlapping or adjacent ranges. This is buffered read-ahead for
    # the filesystem; DirectStorage decides whether it uses those pages.
    combined = []
    for offset, size in sorted(ranges):
        end = offset + size
        if combined and offset <= combined[-1][1]:
            combined[-1][1] = max(combined[-1][1], end)
        else:
            combined.append([offset, end])
    started = monotonic()
    read_bytes = 0
    with pack.open("rb") as file:
        buffer = bytearray(1024 * 1024)
        for offset, end in combined:
            file.seek(offset)
            while offset < end:
                count = file.readinto(memoryview(buffer)[: min(len(buffer), end - offset)])
                if not count:
                    raise OSError("pack changed during read-ahead")
                offset += count
                read_bytes += count
    print(f"Neptune prefetch: {read_bytes} bytes in {len(combined)} ranges, {monotonic()-started:.3f} s")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("pack", type=Path)
    parser.add_argument("profile", type=Path)
    parser.add_argument("--record-only", action="store_true")
    args = parser.parse_args()
    meta = args.profile / "manifest.json"
    pack_id = identity(args.pack)
    try:
        if meta.stat().st_size > 1024 * 1024:
            raise ValueError("oversized prefetch manifest")
        previous = json.loads(meta.read_text(encoding="utf-8-sig"))
    except (OSError, ValueError):
        previous = {}
    if not isinstance(previous, dict) or previous.get("pack") != pack_id:
        previous = {}
    # Marker filenames are fixed inside this save's profile.
    marker = args.profile / "last-load.csv"
    ranges = collect(marker, pack_id[1]) if previous else []
    if not ranges:
        previous_ranges = previous.get("ranges", [])
        if isinstance(previous_ranges, list) and len(previous_ranges) <= MAX_RANGES:
            used = 0
            for row in previous_ranges:
                if not isinstance(row, list) or len(row) != 2 or any(type(v) is not int for v in row):
                    continue
                offset, size = row
                if offset >= 0 and size > 0 and offset + size <= pack_id[1] and used + size <= MAX_BYTES:
                    ranges.append(row)
                    used += size
    # Retain the previous marker until its validated ranges have been saved.
    temporary = meta.with_suffix(".tmp")
    temporary.write_text(json.dumps({"pack": pack_id, "ranges": ranges}), encoding="utf-8")
    temporary.replace(meta)
    if marker.exists():
        marker.unlink()
    if args.record_only:
        print("Neptune prefetch: recording only")
    elif ranges:
        replay(args.pack, ranges)
    else:
        print("Neptune prefetch: first launch, recording verified block ranges")


if __name__ == "__main__":
    main()
