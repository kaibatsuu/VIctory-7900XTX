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

- **Three branches merged**, each on top of the self-healing pipelines (a driver
  crash in a pipeline compile is bisected to the instruction to disable, and the
  fallback is cached per shader and driver):
  - crash fallback at run time: a shader that crashes the driver is made with
    its culprit instruction disabled instead of being left out;
  - the submission-preparing thread split in two, opt-in (`VICTORY_SUBMIT_SPLIT=1`);
  - D3D12 specialization-constant plumbing for compute pipelines (no caller uses it
    yet).
- **Pipeline isolation that works** (`VICTORY_PIPELINE_ISOLATE=1`: each new
  pipeline is first compiled in a child process, so a driver crash costs the child,
  not the run):
  - only a child that crashes leaves a pipeline out. Before, any failed exit counted
    as a crash, and the child refuses some pipelines the run makes fine: about a
    quarter of the pipelines were left out and the world was not drawn;
  - pipelines a child has made without a crash are remembered in the cache's
    `warm-done.txt` (the list `victory warm` keeps, per translator and driver), so
    later launches skip the test for them;
  - a shader's cached fallback now reaches the child (`VICTORY_PENDING_FALLBACK`),
    and is also used without isolation;
  - at most `VICTORY_CANARY_JOBS` children at once (16 by default), and each child
    that dies is logged with its exit code.
- **Measurements** (diagnostics builds): `VICTORY_SLEEP_COUNT=1` counts and times
  the title's `Sleep(0)`, `SleepEx` and `SwitchToThread` and logs them per frame;
  each `monitorx`/`mwaitx` replaced in the title's code is logged with its place,
  so `VICTORY_PROFILE` samples can be matched to the spin loops around them.

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
This one is about **speed on AMD RDNA 3**: the world at 30 frames a second on
one PC, without giving up the picture. It is not there yet.

**This repository contains no game files, code or assets.** You need your own
copy of the game. It is not affiliated with or endorsed by the game's developer
or publisher.

## Where it stands (2026-11-25)

Tested on one machine only: Windows 11, Intel i9-12900K, 64 GB RAM, Radeon
RX 7900 XTX, 1440p, the Series X profile.

| | |
|---|---|
| Menus, videos, loading screens | 30 fps |
| Prologue, indoors | 50-75 ms a frame (13-20 fps) |
| First mission with the boat | 46-53 ms (19-22 fps) |
| Street chase with many cars and people | 100-165 ms (6-10 fps), single frames far longer |
| Outdoors on foot, the camp | 60-90 ms |
| Flying | 42-57 ms |
| Longest session so far | 41 minutes, no crash |
| Memory | 40-46 GB of private memory: **64 GB of RAM recommended** |

It started at 280 ms a frame in the same scenes two days earlier.

What is known to be wrong:

- Trees, plants and far buildings flicker outdoors in some places.
- Rooms with little direct light are too dark without `-RayTracing`.
- Textures of what just came into view appear late while the title streams.
- The first run with a new driver or a new build stutters while the driver makes
  pipelines (`victory warm` makes them ahead).
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
