"""Run the opt-in source-offset path through the real HLE queue with generated files."""
import argparse
import csv
import hashlib
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile
import zlib

from test_rolling import make_pack, payload


def check(binary, sdk, pack, index, source, output, stored, offset, hits, delivered):
    env = dict(os.environ, VICTORY_GDEFLATE_PACK=str(pack), VICTORY_GDEFLATE_OFFSET_INDEX=str(index),
               VICTORY_GDEFLATE_BRIDGE_DIR=str(sdk), VICTORY_TEST_OFFSET_SOURCE=str(source),
               VICTORY_TEST_OFFSET_OUTPUT=str(output), VICTORY_TEST_OFFSET_STORED=str(stored),
               VICTORY_TEST_OFFSET_LOCATION=str(offset), VICTORY_TEST_OFFSET_HITS=str(hits),
               VICTORY_TEST_OFFSET_DELIVERED=str(delivered))
    subprocess.run([binary,"--exact","modules::dstorage::tests::generated_gpu_offset_request","--ignored"], env=env, check=True)


def fixtures(binary, sdk, bc):
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        pack, index, source, output = (root / n for n in ("fixture.pack","offsets.bin","fixture.rpf","expected.bin"))
        make_pack(pack, sdk, bc)
        decoded = payload(1)
        compressed = zlib.compress(decoded)
        source.write_bytes(bytes(19) + compressed + compressed)
        output.write_bytes(decoded)
        with pack.open("rb") as file:
            _, table, n = struct.unpack("<8sQQ",file.read(24))
            file.seek(table)
            record = next(r for r in struct.iter_unpack("<32s32sQIIII",file.read())
                          if r[0] == hashlib.sha256(compressed).digest())
        with (root / "gdeflate-markers.csv").open("w", newline="") as file:
            w = csv.writer(file)
            w.writerow(("event","source_kind","source_id","file_offset","stored_bytes","decoded_bytes","pack_offset"))
            w.writerow(("verified_delivery","file","123",19,len(compressed),len(decoded),record[2]))
        (root / "gdeflate-names.csv").write_text("id,name_hex\n123," + br"G:\fixture.rpf".hex() + "\n")

        def build():
            subprocess.run([sys.executable, "-B", Path(__file__).with_name("source-offset-index.py"),
                           "--root",root,"--capture",root,"--pack",pack,"--output",index], check=True)

        def run(hits, delivered, file=source, start=19, packed=pack):
            check(binary, sdk, packed, index, file, output, len(compressed), start, hits, delivered)

        build()
        run(1,1)
        run(0,0, start=19+len(compressed))
        check(binary,sdk,pack,index,source,output,len(compressed)+1,19,0,0)
        output.write_bytes(decoded + b"\0")
        run(0,0)
        output.write_bytes(decoded)
        # A different codec tuple must not use the captured digest for this request.
        valid = index.read_bytes()
        wrong_flags = bytearray(valid)
        struct.pack_into("<I", wrong_flags, len(wrong_flags) - 40, 9)
        wrong_flags[-4:] = struct.pack("<I",zlib.crc32(wrong_flags[:-4]))
        index.write_bytes(wrong_flags)
        run(0,0)
        index.write_bytes(valid)
        copy = root / "replacement.rpf"
        shutil.copy2(source, copy)
        run(0,0, file=copy)
        other_pack = root / "other.pack"
        shutil.copy2(pack, other_pack)
        run(0,0, packed=other_pack)
        with source.open("r+b"):
            run(0,0)
        old = source.stat()
        with source.open("r+b") as writable:
            writable.write(b"changed")
        os.utime(source, ns=(old.st_atime_ns,old.st_mtime_ns))
        run(0,0)
        build()
        data = bytearray(index.read_bytes())
        data[-5] ^= 1
        index.write_bytes(data)
        run(0,0)  # broken sidecar checksum
        data[-4:] = struct.pack("<I",zlib.crc32(data[:-4]))
        index.write_bytes(data)
        run(1,0)  # no pack record for altered digest; ordinary source path delivers correct bytes
    print("Verified GPU delivery, offset/size/flags misses, file/pack replacement, writer, restored-mtime edit, checksum failure and bridge miss passed.")


if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    for key in ("hle-test-exe","sdk","bc"):
        p.add_argument("--"+key, type=Path, required=True)
    a = p.parse_args()
    fixtures(a.hle_test_exe.resolve(strict=True), a.sdk.resolve(strict=True), a.bc.resolve(strict=True))
