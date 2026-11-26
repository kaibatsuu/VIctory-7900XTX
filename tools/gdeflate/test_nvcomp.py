"""Offline converter regression. Run with the private converter's Python and SDK paths."""
import argparse
import hashlib
from pathlib import Path
import sqlite3
import tempfile
import zlib

from nvcomp_encoder import Encoder
from repack import Codec, convert


def test(sdk, bc):
    make = lambda: Codec(sdk, bc)
    verifier = make()
    gpu = Encoder(make, 2)
    try:
        with tempfile.TemporaryDirectory() as temp:
            source = Path(temp) / "synthetic.bin"
            db_path = Path(temp) / "sidecar.sqlite"
            keys, expected = [], {}
            with source.open("wb") as stream:
                # Partial, full and multi-tile blocks, plus two batches in flight.
                for i in range(300):
                    size = (127, 65536, 65537, 196885)[i % 4]
                    data = (bytes([i % 251]) * 257 + bytes(range(255))) * (size // 512 + 1)
                    data = data[:size]
                    payload = zlib.compress(data)
                    key = (stream.tell(), len(payload), len(data), 8)
                    stream.write(payload)
                    keys.append(key)
                    expected[key] = data
                bad = (stream.tell(), 4, 256, 8)
                stream.write(b"oops")
            def run(keys, samples=False):
                return convert(source, iter(keys), source.name, db_path, verifier, samples, 0, gpu)

            assert run([bad, keys[0], keys[1]], samples=True) == 1
            with sqlite3.connect(db_path) as check:
                assert check.execute("select count(*) from block").fetchone() == (1,)
            check.close()
            assert run([*keys, keys[30], bad]) == 1
            assert run(keys) == 0
            db = sqlite3.connect(db_path)
            try:
                rows = list(db.execute("select * from block"))
                assert len(rows) == len(expected)
                for off, stored, size, flags, digest, blob in rows:
                    original = expected[(off, stored, size, flags)]
                    verifier.verify(blob, original)
                    assert hashlib.sha256(original).digest() == digest
                assert db.execute("select off,stored,decoded,flags from failure").fetchall() == [bad]
            finally:
                db.close()
            print("300 round trips, repeated key, resume, sample and malformed-input checks passed")
    finally:
        gpu.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--sdk", required=True, type=Path)
    parser.add_argument("--bc", required=True, type=Path)
    args = parser.parse_args()
    test(args.sdk, args.bc)
