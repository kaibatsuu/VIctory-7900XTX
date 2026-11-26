"""Resume offline conversion, publish the indexed pack, then validate it on the adapter."""
import argparse
import subprocess
import sys
from pathlib import Path


def run(script, *args):
    subprocess.run([sys.executable, "-u", str(Path(__file__).with_name(script)), *map(str, args)], check=True)


if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    for arg in ("root", "indexes", "sdk", "bc", "sidecars", "pack"):
        p.add_argument("--" + arg, type=Path, required=True)
    p.add_argument("--encoder", choices=("directstorage", "nvcomp"), default="directstorage")
    p.add_argument("--compression-level", choices=("fastest", "default", "best-ratio"))
    p.add_argument("--workers", type=int, choices=range(1, 9), default=2)
    a = p.parse_args()
    level = ["--compression-level", a.compression_level] if a.compression_level else []
    run("repack.py", "--root", a.root, "--indexes", a.indexes, "--sdk", a.sdk,
        "--bc", a.bc, "--output", a.sidecars, "--encoder", a.encoder, "--workers", a.workers, *level)
    run("publish.py", "--root", a.root, "--sidecars", a.sidecars, "--out", a.pack)
    run("verify-pack.py", "--root", a.root, "--sidecars", a.sidecars, "--pack", a.pack)
