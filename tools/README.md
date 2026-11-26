# Tools

- `fps-session.py` — frame-time session capture (pre-existing).
- `gdeflate/` — the GDeflate offline toolchain, copied from
  Neptune-GDeflate (GPL-3.0-or-later).

## GDeflate — opt-in, dormant by default

The runtime's GDeflate path is **inert** unless you build a verified pack and
the native bridge yourself. None of these scripts run on their own, and none of
them touch or compress your title's files unless you invoke them by hand. The
runtime falls back to the existing zlib/BCPack CPU decoder whenever
`VICTORY_GDEFLATE_PACK` is unset or the bridge is unavailable.

### 1. Build the native bridge (manual, later)

From an x64 MSVC developer shell, with `$DirectStorageSdk` pointing at an
extracted DirectStorage SDK (contains `native/include` and `native/lib/x64`),
and `$Bridge` a writable folder outside this checkout:

```powershell
cl /nologo /LD /EHsc /std:c++20 /O2 "/I$DirectStorageSdk/native/include" `
   "/Fo$Bridge/runtime.obj" tools/gdeflate/runtime.cpp `
   "/Fe$Bridge/fx1-storage.dll" /link `
   "/LIBPATH:$DirectStorageSdk/native/lib/x64" dstorage.lib d3d12.lib dxgi.lib bcrypt.lib
cargo build --release --locked -p victory-formats
rustc --crate-name fx1_bc --edition 2024 --crate-type cdylib `
   -L dependency=target/release/deps `
   --extern victory_formats=target/release/libvictory_formats.rlib `
   tools/gdeflate/bc_decode.rs -o "$Bridge/fx1_bc.dll"
```

Place the signed `dstorage.dll` and `dstoragecore.dll` from your DirectStorage
SDK beside the bridge.

### 2. Build a verified pack (manual, later, on your own files)

```powershell
python -B tools/gdeflate/convert-indexed.py --root <archive root> `
   --indexes <index dir> --sdk <sdk dir> --bc <bridge dir> `
   --sidecars <sidecar dir> --pack <pack path>
```

This re-encodes your own archives' streams into one `FX1GDF01` pack
(`repack.py` → `publish.py` → `verify-pack.py`). BCPack decoding needs
`VICTORY_BCPACK_TABLES` to name a format-table file. The optional
`--encoder nvcomp` path needs CUDA and the nvcomp Python bindings. The pack,
tables, and bridge DLLs stay **outside** this repository.

### 3. Run

Set `VICTORY_GDEFLATE_PACK=<pack path>` (and `VICTORY_GDEFLATE_BRIDGE_DIR=<dir>`
when the bridge DLLs are not beside the pack). The runtime then decodes small
(≤ 4 MiB) file-source requests on the GPU. Without these variables the
zlib/BCPack CPU decoder runs unchanged.
