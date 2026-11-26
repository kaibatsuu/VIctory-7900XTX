"""Exercise concrete native bypass causes on a synthetic pack, including a held decoder lane."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import ctypes as C
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import uuid
import zlib

from test_rolling import make_pack, payload


def check(sdk, pack, backend):
    with os.add_dll_directory(str(sdk)):
        core = C.WinDLL(str(sdk / "dstoragecore.dll"))
        storage = C.WinDLL(str(sdk / "dstorage.dll"))
        api = C.CDLL(str(sdk / "fx1-storage.dll"))
    if backend == "cpu":
        class Configuration(C.Structure):
            _fields_ = [("submit_threads", C.c_uint32), ("cpu_threads", C.c_int32),
                        ("force_mapping_layer", C.c_int32), ("disable_bypass_io", C.c_int32),
                        ("disable_telemetry", C.c_int32), ("disable_metacommand", C.c_int32),
                        ("disable_gpu_decompression", C.c_int32)]

        configuration = Configuration(disable_gpu_decompression=1)
        set_config = storage.DStorageSetConfiguration
        set_config.argtypes = [C.POINTER(Configuration)]
        assert set_config(C.byref(configuration)) == 0
    os.environ["VICTORY_GDEFLATE_FORCE_SHADER"] = "1" if backend == "shader" else "0"
    ptr, uint = C.c_void_p, C.c_uint32
    device = ptr()
    iid = uuid.UUID("189819f1-1db6-4b57-be54-1821339b85f7").bytes_le
    create = C.WinDLL("d3d12.dll").D3D12CreateDevice
    create.argtypes = [ptr, uint, ptr, C.POINTER(ptr)]
    assert create(None, 0xc000, iid, C.byref(device)) == 0
    legacy = api.fx1_read_gdeflate
    legacy.argtypes = [ptr, C.c_wchar_p, ptr, uint, uint, uint, ptr, C.POINTER(uint)]
    detailed = api.fx1_read_gdeflate_detailed_v1
    detailed.argtypes = legacy.argtypes + [ptr, ptr]
    source = zlib.compress(payload(1))
    n = len(payload(1))

    def read(data=source, size=n, prepare=None, old=False):
        dest = C.create_string_buffer(n)
        support = uint()
        args = (device, str(pack), data, len(data), size, 8, dest, C.byref(support))
        rc = legacy(*args) if old else detailed(*args, prepare, None)
        return rc, support.value, dest.raw

    assert read(size=0)[0] == 6
    assert read(size=(4 << 20) + 1)[0] == 6
    assert read(data=b"")[0] == 5
    if backend == "cpu":
        @C.CFUNCTYPE(None, ptr, uint)
        def track_callback(_ptr, _size):
            callbacks.append(1)

        callbacks = []
        for _ in range(2):  # GPU capability refusal persists without allocating a lane.
            rc, support, dest = read(prepare=track_callback)
            assert rc == 7 and support & 4 and dest == bytes(n), (rc, support)
        assert read(old=True)[0] == 0
        assert not callbacks
        print("Verified CPU-only queue bypass without publishing native destination bytes.")
        return
    entered, release = threading.Event(), threading.Event()

    @C.CFUNCTYPE(None, ptr, uint)
    def hold_lane(_ptr, _size):
        entered.set()
        release.wait(5)

    with ThreadPoolExecutor(1) as workers:
        future = workers.submit(read, prepare=hold_lane)
        try:
            assert entered.wait(10), "first verified delivery did not reach the write callback"
            assert read()[:2] == (2, 0)
            assert read(old=True)[:2] == (0, 0)
        finally:
            release.set()
        rc, support, out = future.result()
        assert rc == 1 and support & 3 and out == payload(1)
        if backend == "shader":
            assert support & 7 == 2, support
    rc, support, _ = read(data=b"absent")
    assert rc == 3 and support & 3
    rc, support, _ = read(data=b"absent", old=True)
    assert rc == 0 and support & 3
    corrupt = payload(4)
    dest = C.create_string_buffer(len(corrupt))
    compressed = zlib.compress(corrupt)
    assert detailed(device, str(pack), compressed, len(compressed), len(corrupt), 8, dest,
                    C.byref(uint()), None, None) < 0
    assert read()[:2] == (4, 0)
    assert read(old=True)[:2] == (0, 0)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sdk", required=True, type=Path)
    parser.add_argument("--bc", required=True, type=Path)
    parser.add_argument("--backend", choices=("auto", "shader", "cpu"), default="auto")
    parser.add_argument("--probe", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    sdk = args.sdk.resolve(strict=True)
    if args.probe:
        check(sdk, args.probe, args.backend)
    else:
        with tempfile.TemporaryDirectory() as temp:
            pack = Path(temp) / "synthetic.pack"
            make_pack(pack, sdk, args.bc)
            subprocess.run([sys.executable, "-B", __file__, "--sdk", str(sdk), "--bc", str(args.bc),
                            "--backend", args.backend, "--probe", str(pack)], check=True)
        if args.backend != "cpu":
            print("Verified successful delivery, busy, miss, size, invalid input, bridge error, latched error and legacy returns.")
