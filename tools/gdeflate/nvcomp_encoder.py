"""Offline CUDA encoder. Each worker owns its nvCOMP stream and DirectStorage verifier."""
import hashlib
import struct
import threading
import zlib
from collections import deque
from concurrent.futures import ThreadPoolExecutor

TILE = 65536


def tile_stream(pages, size):
    # Microsoft's TileStream stores the final compressed length in offset slot zero.
    offsets = []
    total = 0
    for page in pages:
        offsets.append(total)
        total += len(page)
    offsets[0] = len(pages[-1])
    header = struct.pack("<BBHI", 4, 251, len(pages), 1 | (size % TILE) << 2)
    return header + struct.pack(f"<{len(pages)}I", *offsets) + b"".join(pages)


class Encoder:
    def __init__(self, make_verifier, workers):
        import numpy
        from nvidia import nvcomp
        self.np = numpy
        self.nvcomp = nvcomp
        self.make_verifier = make_verifier
        self.workers = workers
        self.local = threading.local()
        self.pool = ThreadPoolExecutor(max_workers=workers, thread_name_prefix="gdeflate")

    def close(self):
        self.pool.shutdown(wait=True, cancel_futures=True)

    def transform_batch(self, batch):
        if not hasattr(self.local, "codec"):
            self.local.codec = self.nvcomp.Codec(algorithm="GDeflate", algorithm_type=1,
                                                 bitstream_kind=self.nvcomp.BitstreamKind.RAW)
            self.local.verifier = self.make_verifier()
        verifier = self.local.verifier
        results, valid, inputs = [], [], []
        for key, source in batch:
            if isinstance(source, str):
                results.append((key, None, source))
                continue
            try:
                raw = verifier.decode_source(source, key[3], key[2])
            except (ValueError, zlib.error) as error:
                results.append((key, None, str(error)))
                continue
            first = len(inputs)
            for start in range(0, len(raw), TILE):
                inputs.append(self.np.frombuffer(raw, dtype=self.np.uint8,
                                                  count=min(TILE, len(raw)-start), offset=start))
            valid.append((key, raw, first, len(inputs)))
        if inputs:
            gpu_input = self.nvcomp.as_arrays(inputs).cuda()
            gpu_output = self.local.codec.encode(gpu_input)
            output = gpu_output.cpu()
            pages = [bytes(tile) for tile in output]
            if len(pages) != len(inputs):
                raise RuntimeError("Incomplete nvCOMP batch")
            for key, raw, start, end in valid:
                encoded = tile_stream(pages[start:end], len(raw))
                verifier.verify(encoded, raw)
                results.append((key, encoded, hashlib.sha256(raw).digest()))
        return results

    def run(self, jobs):
        # Cap both outstanding work and decoded bytes. Each source block is independently
        # bounded to 64 MiB; normal batches stop at 4 MiB or 128 records.
        pending = deque()
        batch = []
        size = 0
        try:
            for key, source in jobs:
                cost = key[2] + (len(source) if isinstance(source, bytes) else 0)
                if batch and (size + cost > 4 << 20 or len(batch) >= 128):
                    pending.append(self.pool.submit(self.transform_batch, batch))
                    batch, size = [], 0
                    if len(pending) >= self.workers:
                        yield from pending.popleft().result()
                batch.append((key, source))
                size += cost
            if batch:
                pending.append(self.pool.submit(self.transform_batch, batch))
            while pending:
                yield from pending.popleft().result()
        finally:
            for future in pending:
                future.cancel()
