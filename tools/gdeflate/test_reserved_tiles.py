"""Generate distinct synthetic GDeflate tiles and verify single and batched reserved-texture mapping."""
import argparse
import hashlib
from pathlib import Path
import subprocess
import tempfile

from repack import Codec


def verify(exe, pack, manifest, shader, *options, markers=()):
    result = subprocess.run([exe, pack, manifest, shader, *options], check=True, timeout=30,
                            capture_output=True, text=True)
    for marker in markers:
        assert marker in result.stdout, (marker, result.stdout)
    print(result.stdout.strip())


def main():
    args = argparse.ArgumentParser(description=__doc__)
    args.add_argument("--sdk", type=Path, required=True,
                      help="Folder containing the fixture, DLLs and four compiled shaders")
    args.add_argument("--bc", type=Path, required=True)
    options = args.parse_args()
    sdk = options.sdk.resolve()
    codec = Codec(sdk, options.bc)
    with tempfile.TemporaryDirectory() as temp:
        folder = Path(temp)
        pack, manifest = folder / "fixture.pack", folder / "fixture.tiles"
        digests = []
        with pack.open("wb") as out, manifest.open("w", encoding="ascii") as index:
            for tile in range(4):
                raw = bytes((x * 13 + (x // 256) * 7 + tile * 53) & 255 for x in range(65536))
                encoded, digest = codec.transform(raw, 0, len(raw))
                assert digest == hashlib.sha256(raw).digest()
                digests.append(digest.hex())
                index.write(f"{out.tell()} {len(encoded)} {digest.hex()}\n")
                out.write(encoded)
                if tile == 1:
                    (folder / "single.gdeflate").write_bytes(encoded)
        exe = sdk / "streaming-test.exe"
        verify(exe, pack, manifest, sdk / "feedback.dxil", "--observe",
               markers=("Observation only: 8x8 regions, 64x64 resolve, 4 bytes read back, mask 2",))
        verify(exe, folder / "single.gdeflate", digests[1], sdk / "feedback.dxil",
               markers=("GPU feedback mask 2 ", digests[1]))
        verify(exe, pack, manifest, sdk / "feedback.dxil", "1", "rgba8", sdk / "sample.dxil",
               markers=("GPU feedback mask 2 ", "Sampler returned all 16384 RGBA texels", digests[1]))
        verify(exe, pack, manifest, sdk / "diagonal.dxil", "--diagonal",
               markers=("GPU feedback mask 9 ", f"(0,0) verified 65536 bytes SHA256 {digests[0]}",
                        f"(1,1) verified 65536 bytes SHA256 {digests[3]}"))
        verify(exe, pack, manifest, sdk / "diagonal.dxil", "--diagonal-memory",
               markers=("Compressed CPU memory to device-local tiles verified",
                        f"(0,0) verified 65536 bytes SHA256 {digests[0]}",
                        f"(1,1) verified 65536 bytes SHA256 {digests[3]}"))
        # Swap the two manifest hashes only: the selected tile must be compared
        # against its own block and not accepted by a collective digest check.
        bad = folder / "bad.tiles"
        rows = manifest.read_text().splitlines()
        rows[0] = rows[0].replace(digests[0], digests[3])
        rows[3] = rows[3].replace(digests[3], digests[0])
        bad.write_text("\n".join(rows) + "\n", encoding="ascii")
        rejected = subprocess.run([exe, pack, bad, sdk / "diagonal.dxil", "--diagonal"],
                                  capture_output=True, text=True, timeout=30)
        assert rejected.returncode == 1 and "tile digest mismatch" in rejected.stderr, rejected
        print("Swapped digests rejected")


if __name__ == "__main__":
    main()
