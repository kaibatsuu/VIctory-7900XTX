"""Offline D3D12 marker order, source attribution, bounded output and LRU regression."""
import argparse
from collections import OrderedDict
import csv
import ctypes as C
import hashlib
import json
import os
from pathlib import Path
import random
import sqlite3
import struct
import subprocess
import sys
import tempfile
import uuid
import zlib

from asset_markers import inspect_marker, summary
from test_rolling import SIZES, TILE, make_pack, payload


class Source(C.Structure):
    _fields_ = [("source_id", C.c_uint64), ("name_id", C.c_uint64), ("offset", C.c_uint64),
                ("file", C.c_uint32), ("reserved", C.c_uint32)]


def operations(mode):
    if mode == "lru":
        rng = random.Random(641)
        return [0, 1, 2, 0, 3, 0, 1, 2] + [rng.randrange(4) for _ in range(192)]
    return [0, 1, 2, 0, 3, 1, 0, 2, 1, 1]


def probe(pack, sdk, mode):
    with os.add_dll_directory(str(sdk)):
        core = C.WinDLL(str(sdk / "dstoragecore.dll"))
        api = C.CDLL(str(sdk / "fx1-storage.dll"))
    ptr, uint = C.c_void_p, C.c_uint32
    device = ptr()
    iid = uuid.UUID("189819f1-1db6-4b57-be54-1821339b85f7").bytes_le
    create = C.WinDLL("d3d12.dll").D3D12CreateDevice
    create.argtypes = [ptr, uint, ptr, C.POINTER(ptr)]
    assert create(None, 0xc000, iid, C.byref(device)) == 0
    read = api.fx1_read_gdeflate_profiled
    read.argtypes = [ptr, C.c_wchar_p, ptr, uint, uint, uint, ptr, C.POINTER(uint), ptr, C.POINTER(Source)]
    finish = api.fx1_finish_gdeflate_delivery
    finish.argtypes = [C.c_int]
    finish.restype = None
    prepare_calls = []
    prepare = C.CFUNCTYPE(None, ptr, uint)(lambda p, n: prepare_calls.append(n))
    raws = [payload(i) for i in range(len(SIZES))]
    sources = [zlib.compress(raw) for raw in raws]
    sequence = operations(mode)
    # First a lookup miss, and then real requests. Last valid block simulates
    # failure writing the paged destination after successful verified readback.
    status = uint()
    for request, index in enumerate(sequence + [4], 1):
        raw, source = raws[index], sources[index]
        destination = C.create_string_buffer(b"_" * len(raw), len(raw))
        origin = Source(0 if index == 1 else 0xabc0 + index, 0x9000 + request,
                        1000 + index * 65536, int(index != 1), 0)
        if request == 1:
            assert read(device, str(pack), b"?", 1, len(raw), 8, destination, C.byref(status),
                        prepare, C.byref(origin)) == 0
            finish(1)
        before = len(prepare_calls)
        direct = request % 2 != 0 or index == 4
        rc = read(device, str(pack), source, len(source), len(raw), 8, destination, C.byref(status),
                  prepare if direct else None, C.byref(origin))
        if index == 4:
            assert rc < 0 and len(prepare_calls) == before and destination.raw == b"_" * len(raw)
            finish(1)
            break
        assert rc == 1 and destination.raw == raw and status.value & 3
        assert len(prepare_calls) - before == int(direct)
        finish(int(request != len(sequence)))
        finish(1)  # Completion is single-use, even if called twice.
    # A corrupt digest permanently disables this decoder lane.
    destination = C.create_string_buffer(SIZES[0])
    assert read(device, str(pack), sources[0], len(sources[0]), SIZES[0], 8, destination,
                C.byref(status), None, C.byref(origin)) == 0


def expected(mode):
    sequence = operations(mode)
    # Independent occupancy oracle with scalar tile owners and ordered recency.
    tiles = [None] * 64
    residents = OrderedDict()
    next_tile = 0
    events = []
    for req, index in enumerate(sequence + [4], 1):
        def evict(victim):
            original, start = residents.pop(victim)
            events.append((original, "eviction", victim, start))
            tiles[:] = [None if value == victim else value for value in tiles]

        count = (SIZES[index] + TILE - 1) // TILE
        if index in residents:
            _, start = residents[index]
            residents.move_to_end(index)
            events.append((req, "reuse", index, start))
        else:
            if mode == "lru":
                while True:
                    start = next((i for i in range(65-count) if tiles[i:i+count] == [None] * count), None)
                    if start is not None:
                        break
                    evict(next(iter(residents)))
            else:
                start = next_tile if next_tile + count <= 64 else 0
                for victim in dict.fromkeys(tiles[start:start+count]):
                    if victim is not None:
                        evict(victim)
            events.append((req, "gpu_submit", index, start))
            if index == 4:
                break
            residents[index] = (req, start)
            tiles[start:start+count] = [index] * count
            next_tile = start + count
        events.append((req, "delivery_failed" if req == len(sequence) else "verified_delivery", index, start))
    return events


def verify(rows, mode, pack):
    events = expected(mode)
    assert len(rows) == len(events), (len(rows), len(events))
    with pack.open("rb") as f:
        _, table, size = struct.unpack("<8sQQ", f.read(24))
        f.seek(table)
        records = {r[0]: r for r in struct.iter_unpack("<32s32sQIIII", f.read(size * 88))}
    for seq, (row, (req, event, index, start)) in enumerate(zip(rows, events, strict=True), 1):
        record = records[hashlib.sha256(zlib.compress(payload(index))).digest()]
        assert (int(row["seq"]), int(row["request"]), row["event"], int(row["tile"])) == (seq, req, event, start)
        assert int(row["utc_us"]) > 1_600_000_000_000_000
        assert row["asset_id"] == record[0].hex()[:16]
        assert tuple(int(row[k]) for k in ("pack_offset", "compressed_bytes", "stored_bytes", "decoded_bytes")) == record[2:6]
        assert int(row["name_id"], 16) == 0x9000 + req
        assert row["source_kind"] == ("memory" if index == 1 else "file")
        assert int(row["source_id"], 16) == (0 if index == 1 else 0xabc0 + index)
        assert row["file_offset"] == ("" if index == 1 else str(1000 + index * 65536))
    return summary(rows)


def check_lookup(row, folder):
    index = folder / "texture.sqlite"
    with sqlite3.connect(index) as db:
        db.execute("""CREATE TABLE texture(hash,store,width,height,dxgi,offset_in_decoded_chunk,
                      source_offset INTEGER,stored INTEGER,decoded INTEGER)""")
        for store in ("data/store.ttpf", "other/store.ttpf"):
            for i in range(2):
                db.execute("INSERT INTO texture VALUES(?,?,?,?,?,?,?,?,?)",
                           (str(i), store, 64, 64, 0, i * 128, row["file_offset"], row["stored_bytes"], row["decoded_bytes"]))
    names = {row["source_id"]: r"G:\data\store.ttpf", row["name_id"]: "request"}
    detail = inspect_marker(row, names, index)
    assert detail["request_name"] == "request"
    assert [c["store"] for c in detail["indexed_texture_start_candidates"]] == ["data/store.ttpf"] * 2
    names[row["source_id"]] = "store.ttpf"
    assert not inspect_marker(row, names, index)["indexed_texture_start_candidates"]


def check_hle(binary, sdk, pack, folder):
    markers = folder / "hle-markers.csv"
    names = folder / "hle-names.csv"
    data = folder / "compressed.bin"
    data.write_bytes(zlib.compress(payload(1)))
    subprocess.run([binary.resolve(strict=True), "--exact",
                    "modules::dstorage::tests::generated_gpu_marker_requests", "--ignored"], check=True,
                   env={**os.environ, "VICTORY_GDEFLATE_ROLLING": "1", "VICTORY_GDEFLATE_LRU": "0",
                        "VICTORY_TEST_GDEFLATE_INPUT": str(data), "VICTORY_GDEFLATE_PACK": str(pack),
                        "VICTORY_GDEFLATE_BRIDGE_DIR": str(sdk), "VICTORY_DSTORAGE_RETRIES": "0",
                        "VICTORY_GDEFLATE_MARKERS": str(markers), "VICTORY_GDEFLATE_MARKER_NAMES": str(names)})
    with markers.open(newline="") as f:
        rows = list(csv.DictReader(f))
    assert [r["event"] for r in rows] == ["gpu_submit", "verified_delivery", "reuse", "verified_delivery", "reuse", "delivery_failed"]
    assert [r["source_kind"] for r in rows] == ["file"] * 2 + ["memory"] * 4 and rows[0]["file_offset"] == "19"
    detail = json.loads(subprocess.check_output([sys.executable, "-B", Path(__file__).with_name("asset_markers.py"),
                                                markers, "--seq", "1", "--names", names]))
    assert detail["request_name"] == "fixture,chunk" and Path(detail["container"]).name == "data.bin"
    print("HLE file + memory requests, private names dictionary and paged destination failure passed")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sdk", type=Path, required=True)
    parser.add_argument("--bc", type=Path, required=True)
    parser.add_argument("--probe", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--mode", default="rolling", choices=["rolling", "lru"])
    parser.add_argument("--hle-test-exe", type=Path, help="run generated requests through the Rust HLE test binary")
    args = parser.parse_args()
    sdk = args.sdk.resolve(strict=True)
    if args.probe:
        probe(args.probe, sdk, args.mode)
    else:
        with tempfile.TemporaryDirectory() as temp:
            folder = Path(temp)
            pack = folder / "generated.pack"
            make_pack(pack, sdk, args.bc)
            for test in ("rolling", "lru", "cap", "existing"):
                mode = "lru" if test == "lru" else "rolling"
                markers = folder / (test + ".csv")
                if test == "existing":
                    markers.write_bytes(b"previous run")
                subprocess.run([sys.executable, "-B", __file__, "--sdk", str(sdk), "--bc", str(args.bc),
                                "--probe", str(pack), "--mode", mode], check=True,
                               env={**os.environ, "VICTORY_GDEFLATE_ROLLING": "1",
                                    "VICTORY_GDEFLATE_LRU": str(int(mode == "lru")),
                                    "VICTORY_GDEFLATE_MARKERS": str(markers),
                                    "VICTORY_GDEFLATE_MARKER_LIMIT": "3" if test == "cap" else "65536"})
                if test == "existing":
                    assert markers.read_bytes() == b"previous run"
                else:
                    with markers.open(newline="") as f:
                        rows = list(csv.DictReader(f))
                    if test == "cap":
                        assert len(rows) == 4 and rows[-1]["event"] == "limit" and None not in rows[-1]
                    else:
                        report = verify(rows, mode, pack)
                        print(f"{test}: {len(rows)} marker rows, {report['events']}", flush=True)
                        if test == "rolling":
                            check_lookup(rows[0], folder)
            print("cap, exclusive creation, name/texture lookup, digest and paged-destination negative controls passed")
            if args.hle_test_exe:
                check_hle(args.hle_test_exe, sdk, pack, folder)
