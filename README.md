# another-victory-fork

A fork of [avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX),
which is itself a fork of [VIctory](https://github.com/Marvisak/VIctory) by way of
[fxceless/victory-fork](https://github.com/fxceless/victory-fork): a compatibility
layer that runs an Xbox Series title natively on Windows through Direct3D 12.

**This repository contains no game files, code or assets.** You need your own copy
of the game. It is not affiliated with or endorsed by the game's developer or
publisher.

The default branch, `combined`, is avatarmanz's work with three feature branches
merged into it and the fixes below. Everything from
[VIctory on an RX 7900 XTX](#victory-on-an-rx-7900-xtx) down is avatarmanz's README
as it was, with one correction (XCL, under its Credits).

## What this fork adds

- **Branches merged**, each on top of the self-healing pipelines (a driver
  crash in a pipeline compile is bisected to the instruction to disable, and the
  fallback is cached per shader and driver):
  - crash fallback at run time: a shader that crashes the driver is made with
    its culprit instruction disabled instead of being left out;
  - the submission-preparing thread split in two, opt-in (`VICTORY_SUBMIT_SPLIT=1`).
- **Pipeline isolation that works** (`VICTORY_PIPELINE_ISOLATE=1`: each new
  pipeline is first compiled in a child process, so a driver crash costs the child,
  not the run):
  - only a child that crashes leaves a pipeline out. Before, any failed exit counted
    as a crash, and the child refuses some pipelines the run makes fine: about a
    quarter of the pipelines were left out and the world was not drawn;
  - pipelines a child has made without a crash are remembered in the cache's
    `warm-done.txt` (the list `victory warm` keeps, per translator and driver), so
    later launches skip the test for them;
  - a shader's cached fallback now reaches the child, and is also used without
    isolation;
  - at most `VICTORY_CANARY_JOBS` children at once (4 by default), and each child
    that dies is logged with its exit code.
- **Measurements** (diagnostics builds): `VICTORY_SLEEP_COUNT=1` counts and times
  the title's `Sleep(0)`, `SleepEx` and `SwitchToThread` and logs them per frame;
  each `monitorx`/`mwaitx` replaced in the title's code is logged with its place,
  so `VICTORY_PROFILE` samples can be matched to the spin loops around them.
- **Memory over time**: `VICTORY_MEMLOG=<file>` (diagnostics builds) writes the
  numbers of the five-second memory line to a CSV, and `tools\memlog.ps1` reads
  it back as what grew, a minute and a column at a time; `tools\memwatch.ps1`
  samples a running build's RAM and VRAM from outside, so a release build can be
  watched too ([docs/memory.md](docs/memory.md)).
- **Mouse controls** (on by default): left-clicking the title window captures
  the cursor and turns raw mouse motion into the pad's right stick; left/right
  mouse buttons pull the right/left triggers and middle-click presses the right
  stick. `Alt` or losing focus releases the cursor. `VICTORY_MOUSE=0` turns it
  off; `VICTORY_MOUSE_FULL`, `VICTORY_MOUSE_SMOOTH_MS`,
  `VICTORY_MOUSE_DEADZONE` and `VICTORY_MOUSE_INVERT=1` tune it.

## Ported from other forks (2026-11-26)

Read on 2026-11-26: the source drop of codec-d/cool_fox build 30e54b4 (its base
`f4b48ad`) and Op-lite/victory-scarlett `c45b43e` (`C:\Dev\oppi`). Six things
were adapted here; what was read and left out, and why, is in
[docs/ports-2026-11-26-cf-oppi.md](docs/ports-2026-11-26-cf-oppi.md).

| Switch | Default | What it changes |
|---|---|---|
| `VICTORY_EARLY_DEPTH=0` | on | the console's `DEPTH_BEFORE_SHADER` (`[earlydepthstencil]`) declared to the host; off is the old late testing |
| `VICTORY_E5_TARGETS=r11g11b10` | `r16g16b16a16` | R9G9B9E5 targets stand in as R11G11B10_FLOAT (no alpha, negatives stored as 0) instead of R16G16B16A16 |
| `VICTORY_AMBIENT_LEFTOVER=hue` / `off` | `keep` | what the ambient stand-in does with the light the title's own resolve left (kept as before / its brightness only / dropped) |
| `VICTORY_WINDOW_CARD_PERCENT=50..95` | `80` | the share of the card's budget up to which new windows outside buffers get a copy in card memory |
| `VICTORY_STAGING_REST_MS`, `VICTORY_STAGING_PACK=1` | `2500`, off | how long a staging piece rests and whether pieces pack into the fullest block; both trims of staging memory |
| `VICTORY_SEND_SPARE_MB=<n>` | `768` (`256` with `VICTORY_SEND_BAR`) | the most send-buffer memory kept spare |

No run of this branch was made: the defaults are the picture every figure here
was measured with, and each line in the table is an experiment with what to look
at recorded in the doc.

### Ported from zeex64/victory-7900xtx (2026-11-27)

Audited on 2026-11-27: [zeex64/victory-7900xtx](https://github.com/zeex64/victory-7900xtx)
at `e2b05f4` (8 commits past our shared ancestor `b0cfe2b`). Applicable
improvements were ported across recording/submission parallelism, write-watch
fast paths and safety fixes, pipeline/pool/resource caches, crash diagnostics,
and optional shader/ambient switches; what was ported, adapted, or skipped is
in [docs/ports-2026-11-27-zeex64.md](docs/ports-2026-11-27-zeex64.md).

- **Recording & submission fast paths (on by default, each switchable `=0`)**:
  `VICTORY_ASK_BESIDE`, `VICTORY_REFRESH_BESIDE`, `VICTORY_SNAPSHOTS_BESIDE`
  (`VICTORY_BESIDE_SPIN`), `VICTORY_GAPS_TOGETHER`, `VICTORY_DYNAMIC_CHUNK`,
  `VICTORY_READY_FINE`, `VICTORY_ENTRY_READY`, `VICTORY_TARGETS_ONCE`,
  `VICTORY_RETIRE_BATCH`, `VICTORY_RECORD_BATCH`, `VICTORY_PLACED_QUICK`,
  `VICTORY_FRESH_QUICK`, `VICTORY_UPLOAD_ASKED`, `VICTORY_ASK_KNOWN`,
  `VICTORY_REMAP_LEAN`, `VICTORY_SWEEP_FOLLOWS_ROOM`,
  `VICTORY_SWEEP_GONE_FIRST`, `VICTORY_LIFT_SELDOM`, `VICTORY_UNMAP_RUNS`.
- **Opt-in / Wine-default switches (off by default on Windows)**:
  `VICTORY_AMBIENT_REFLECT=0.5`, `VICTORY_NAN_GUARD=1|stores`,
  `VICTORY_SINGLE_LATCHES=1`, `VICTORY_CARD_FINE=1`, `VICTORY_WIDE_MORE=1`,
  `VICTORY_TAKE_FINE=1`, `VICTORY_POOLS_MORE=1`, `VICTORY_LEAVE_OUT=<hash>`,
  `VICTORY_LARGE_STACK=1`, `VICTORY_GUARD_HOT_FRAMES=4`,
  `VICTORY_HOLES_TOLD=1`, `VICTORY_TEXTURE_ASK_ONCE=1`, `VICTORY_MAP_SAME=1`,
  `VICTORY_NO_WATCHDOG=1`, `VICTORY_SKIP_BINK=1|all`,
  `VICTORY_SKIP_INTRO=<sec>`, `VICTORY_SAMPLE=<ms>`.

### Ported from hle-d3d12-runtime and foxvic-scarlett (2026-11-27)

Two more forks were read the same day and every candidate was vetted by an
independent skeptic pass before anything was merged: the
[LibsMQG/hle-d3d12-runtime](https://github.com/LibsMQG/hle-d3d12-runtime) fork
(unrelated history, built on RYUKI) and
[randomkidmakesthis/foxvic-scarlett](https://github.com/randomkidmakesthis/foxvic-scarlett)
(a foxhound × scarlett merge). What was taken, adapted, or refused and why is in
[docs/ports-2026-11-27-hle-foxvic.md](docs/ports-2026-11-27-hle-foxvic.md).

- **Correctness fixes, on by default**: use-after-free in command-queue release
  and fence waits/signals; a dropped view heap's cached descriptor run; a null
  constant-buffer write now replacing the old descriptor; a placement claiming
  its pages at once and giving back exactly the pages it locked, with title
  buffers refused in place under late waits; texture allocation-info alignment
  and an empty query heap; two frame waits taking two ticks; a snapshot
  allocation retried after idle buffers are let go; XGameSave validation and
  quota arithmetic, DirectStorage and gamepad out-parameter contracts, the XMA
  decoder's APU heap lifetime; `v_div_fixup_f32` turning `0.0/x` and `x/Inf`
  into `±Inf`, `_g16` gradient packing, and `annotateHandle` kept block-local so
  AMD's DXIL compiler no longer crashes.
- **Opt-in, off by default**: `VICTORY_PORT_BUFFERS=1` (title buffers in place
  above 4 GiB, big ones kept on the card), `VICTORY_STAGED_ONLY_TITLE=1` (the
  texture pass asks only textures at a title address), and
  `VICTORY_EXACT_NO_WAIT=1` (a bake's exact refresh reads through the kernel
  before waiting for a remap).

## How to run it

You need Windows 10/11 x64, a CPU with AVX2, a Direct3D 12 GPU with shader model
6.6 and mesh shaders, Rust (the toolchain is pinned), the MSVC build tools, and your
own dump of the title. This fork's changes were tested on one PC: Windows 11, an AMD
Ryzen 7 9800X3D, a Radeon RX 9070 XT (driver 32.0.32029.2010) and 64 GB of RAM.

```powershell
git clone https://github.com/kaibatsuu/another-victory-fork
cd another-victory-fork
copy victory.example.toml victory.toml      # then set your paths, outside this folder
cargo build --locked --profile diag -p victory --features diagnostics
```

(Instead of `victory.toml`, `VICTORY_CACHE_DIR` and `VICTORY_DATA_DIR` can name the
cache and data folders.) Once per new build or driver, before the first run (both
can be stopped and go on later):

```powershell
target\diag\victory.exe shaders <your copy's folder>\shaders.rpf   # every shader translated, about a minute
target\diag\victory.exe warm --used                                # only after a run with VICTORY_PIPELINE_BOOK=1
```

Then:

```powershell
scripts\play.ps1 -Exe <your copy's executable> -More @{VICTORY_PIPELINE_ISOLATE='1'}
```

With isolation the first launch of a new build stutters for a few minutes while
each new pipeline is tested; later launches skip those already tested. A child that
crashes in the driver appears in Windows' event log as an application error of
`victory.exe`: that is the test doing its job, and the game goes on. Without
`-More` it runs as avatarmanz's README below describes; `-Safe` and `-RayTracing`
are as there.

Where it stands (2026-11-26, one PC): the world draws; once loaded, about 53 ms a
frame (about 19 fps) in the scenes tried, with single frames up to about 200 ms. As
below, trees and far buildings flicker in places, and the ray-traced resolve is
left out without `-RayTracing`.

## Where everything comes from

- **[VIctory](https://github.com/Marvisak/VIctory)** by Marvisak and its
  contributors, through **[fxceless/victory-fork](https://github.com/fxceless/victory-fork)**
  and **[avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX)**:
  nearly all of the code; avatarmanz's README below lists who else it carries work
  from (DexNom, Belobug and romainhedouin, FQY55kai, myokex with Spexivus, joroyt,
  tightfall and Joshua Odom, Geckomacabre, cxy0072019-sketch). The history is kept,
  and commits taken from elsewhere name the commit and its author.
- The DirectStorage worker count by physical cores follows a measurement by
  soldsawn (`0bba762`, carried in Op-lite/victory-scarlett `f748fb5`).
- This fork's own commits are the 17 after avatarmanz's `main` (`85d5641`): the
  self-healing pipelines, the three merged branches and the fixes above. The last
  three were made with [Claude Code](https://claude.com/claude-code).
- Code from outside the VIctory family, under its own licence, as its files say:
  - AMD's addrlib swizzle tables (MIT, Advanced Micro Devices):
    `crates/victory-formats/src/texture.rs`;
  - GFX10 opcode names generated from Mesa's ACO (MIT, Valve Corporation):
    `crates/victory-shader/src/gfx10/opcodes.rs`;
  - tables and the frame decoder's structure from FFmpeg's WMA Pro decoder
    (LGPL-2.1-or-later, used under GPL-3.0-or-later as it allows):
    `crates/victory-formats/src/xma/tables.rs`, `crates/victory-formats/src/xma.rs`;
  - the console-instruction decoder and emulation from
    [XCL](https://github.com/ivall/xcl) (GPL-2.0-only):
    `crates/victory-loader/src/console_insn.rs` and `console_insn_decode.rs`.

**A licensing problem this fork inherits and has not resolved:** XCL is
GPL-2.0-only, and code under GPL-2.0-only cannot be relicensed under
GPL-3.0-or-later, the licence of this project. Until that is settled (XCL's
authors' permission, or those two files written anew), treat them as GPL-2.0-only.

## License

GPL-3.0-or-later, as VIctory (see [LICENSE](LICENSE)), except the files named
above under their own licences, and the one file named in
[docs/asset-policy.md](docs/asset-policy.md).

---

# VIctory on an RX 7900 XTX

A fork of [VIctory](https://github.com/Marvisak/VIctory), a compatibility layer
that runs an Xbox Series title natively on Windows from files the user supplies.

## The goal

**A stable 30 fps or more without ray tracing, then as high a frame rate and as
low frame times as the PC allows**, on an AMD RX 7900 XTX first and on PCs with
32 GB (later 16 GB) of RAM.

The way there is **porting, not emulating**: patching the title's renderer so it
behaves the way a PC renderer does, rather than having the layer watch and copy
the console's shared memory every frame. Shaders are translated ahead of time
into a versioned store that is kept between builds. Ray tracing stays optional
and off by default. The picture comes after the frame rate: texture pop-in and
lighting that is too bright or too dark when walking indoors are next in line.

**This repository contains no game files, code or assets.** You need your own
copy of the game. It is not affiliated with or endorsed by the game's developer
or publisher.

## Where it stands (2026-11-26)

Tested on one machine only: Windows 11, Intel i9-12900K, 64 GB RAM, Radeon
RX 7900 XTX, ray tracing off.

| | |
|---|---|
| City street, Series S profile, renderer patches 1, 2, 5, 6 and 7 on | about 45 ms a frame (about 22 fps, 44.9 ms in run merge-5) |
| Same, patches 1, 2, 5 and 6 | 45.6 ms, p95 47 ms |
| Same, patches 1, 2 and 5 | 47.3 ms (about 21 fps), p95 49 ms |
| Series X profile, patches 1+2 | about 53 ms (about 19 fps) |
| Same scene before the patches | 57-75 ms (13-17 fps) |
| Shaders the title uses that translate | all of them (25,790 of 25,985; the rest are unused geometry shaders) |
| Memory | about 30 GB of private memory, 10.5 GB of VRAM: **64 GB of RAM recommended** for now |

It started at 280 ms a frame in the same scenes three days earlier. What sets
the frame now is the one thread that submits the title's work to the card
(about 40 ms); the card itself needs about 36 ms and now waits about 7 ms a frame with nothing queued.

### Renderer patches (off by default while they are tested)

| Switch | What it does |
|---|---|
| `VICTORY_PORT_CONSTANTS=1` | The title's 174 MB constants heap is read by the card where the title writes it: no uploads, no fills |
| `VICTORY_PORT_DESCRIPTORS=1` | The title's descriptor-table stores are patched (byte-checked) to report the slots they write, instead of a scan of ~600,000 slots a frame |
| `VICTORY_PORT_CBV=1` | Translated shaders read constant buffers in place as raw bytes (~22,000 fills a frame to none; -4.7 ms) |
| `VICTORY_PORT_GUARD=1` | The write guards' lock is shared by faults and refreshes, with a claim per 64 KiB block; the submission's planning runs beside it (title threads' waits 29 ms to ~2 ms a frame; -2 ms) |
| `VICTORY_PORT_JOURNAL=1` | The heaps' records are asked for at the title's submit, and the journal's notes are split into 64 parts with locks of their own (the submit thread's waits ~5 ms + 1 ms to ~0.3 ms; -1 to -3 ms) |

### Other switches

| Switch | What it does |
|---|---|
| `--console series-s` | The Series S profile: 1080p output, about 5 ms faster, a softer picture |
| `VICTORY_OUTPUT=1920x1080` | The title renders to that output size itself (Series X profile; about 16 ms less card time) |
| `VICTORY_FRAME_INTERVAL_MS=16.667` | Lets the title go above 30 fps (menus and videos reach 60; the game's clock stays right) |
| `VICTORY_AMBIENT_SETTLE` | How fast light settles after walking indoors (on by default, `250,3`; `0` turns it off) |
| `VICTORY_TEXTURE_STORE=1` | Converted textures kept between runs (about 8 ms a frame for now: off) |

What is known to be wrong:

- Trees, signs and far objects flicker in the distance in some places.
- Lighting is too bright or too dark after walking indoors, until the title has
  run for a while; rooms with little direct light are too dark without `-RayTracing`.
- Textures of what just came into view appear late while the title streams.
- The first run of a new build stutters while the driver makes pipelines
  (`victory warm` makes them ahead).
- Recording a frame in the world makes the title's own watchdog end it.

## How to run it

You need Windows 10/11 x64, a CPU with AVX2, a Direct3D 12 GPU with shader
model 6.6 and mesh shaders, Rust (the toolchain is pinned), the MSVC build
tools, and your own dump of the title.

```powershell
git clone https://github.com/avatarmanz/VIctory-7900XTX
cd VIctory-7900XTX
copy victory.example.toml victory.toml      # then set your paths, outside this folder
cargo build --locked --profile diag -p victory --features diagnostics
scripts\play.ps1 -Exe <path of your copy's executable>
```

`scripts\play.ps1 -Safe` leaves out the newest, faster ways; try it first when
something is wrong. `-RayTracing` turns the title's ray-traced indirect light on.

On a hybrid-graphics PC, the runtime and offline pipeline checks prefer a
high-performance adapter with Shader Model 6.6. The runtime logs its adapter
name, PCI vendor and device IDs, and dedicated VRAM. Set
`VICTORY_D3D12_ADAPTER` to a zero-based DXGI index or part of the adapter's
name to select it explicitly. With the launcher, pass
`-More @{VICTORY_D3D12_ADAPTER='NVIDIA'}`.

Before the first run, ahead of time (both can be stopped and go on later):

```powershell
target\diag\victory.exe shaders <the shader archives>   # every shader translated, minutes
target\diag\victory.exe warm --used                     # pipelines earlier runs drew with, for a new driver or build
```

Commands, debugging switches and upstream's own description:
[docs/README-fxceless.md](docs/README-fxceless.md). Every `VICTORY_...` switch is
described in a comment where it is read.

## What is different here

In short; the reasons and the measurements are in [docs/findings.md](docs/findings.md).

- **List calls are written down, not carried out, on the title's threads**
  (`VICTORY_THIN`): eight threads of ours translate them.
- **Submissions are prepared by a thread of ours and joined**
  (`VICTORY_ASYNC_SUBMIT`, `VICTORY_JOIN_PARTS`), with what can be spread over
  threads spread: refreshes, compares (AVX2), descriptor records, landings.
- **What shaders wrote comes back beside the write guards**, by the dwords that
  changed, on a thread of its own (`VICTORY_RELAY`, `VICTORY_LAND`).
- **Buffer fills that need bytes still on the card are done on the card**
  (`VICTORY_CARD_STEPS`) instead of waiting for it.
- **The title's writes are caught more cheaply**: one view opened at a fault,
  blocks that rest, places in the title's code that look before they write.
- **Textures go up on a copy queue of their own** and a slot shows one only after
  its upload ran, which also ended a device loss that came every 15 minutes or so.
- **Boats float**: textures that shaders write and the title's CPU reads go back
  to title memory (from fxceless and Geckomacabre, on by default here).
- **Frame record and replay** with per-pass times and shader variants compared
  turn by turn ([docs/replay.md](docs/replay.md)).

## Credits

This is a tree of other people's work with more on top. The history is kept, and
commits taken from elsewhere name the commit and its author.

- **[VIctory](https://github.com/Marvisak/VIctory)** by Marvisak and its
  contributors: the loader, the runtime services, the Direct3D 12 back end and
  the shader recompiler.
- **[fxceless/victory-fork](https://github.com/fxceless/victory-fork)**: the fork
  this one started from, and the people whose pull requests it carries.
- **[DexNom/VIctory](https://github.com/DexNom/VIctory)**: ray tracing fixes,
  cube gradients, sparse textures.
- **[Belobug/VIctory-Belobug-AMD](https://github.com/Belobug/VIctory-Belobug-AMD)**:
  the ambient light idea, pipeline workers, and work by romainhedouin.
- **[FQY55kai/ZJOU-VICTORY](https://github.com/FQY55kai/ZJOU-VICTORY)**.
- **[myokex/victory-amd](https://github.com/myokex/victory-amd)**: the guard over
  every view of a page (with Spexivus, joroyt and tightfall), and Joshua Odom's
  reads into title memory.
- **Geckomacabre** and **cxy0072019-sketch**: the texture read-back.
- **[XCL](https://github.com/ivall/xcl)** (GPL-2.0-only) was read for how the same
  title behaves under another layer. *(Correction by another-victory-fork: this
  said "no code of it is here", but `crates/victory-loader/src/console_insn.rs`
  says it and `console_insn_decode.rs` are ported from XCL; see the licensing note
  at the top.)*
- This fork's changes were made with [Claude Code](https://claude.com/claude-code).

## License

GPL-3.0-or-later, as VIctory. See [LICENSE](LICENSE); the one file that is not
covered is named in [docs/asset-policy.md](docs/asset-policy.md).
