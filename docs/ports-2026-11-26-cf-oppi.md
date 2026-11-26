# Ports from cool_fox 30e54b4 and Op-lite/victory-scarlett (2026-11-26)

Two other forks were read on 2026-11-26 and what applied here was ported:
the source drop of [codec-d/cool_fox](https://github.com/codec-d/cool_fox)
build 30e54b4 (in `victory-cool_fox-willy/cool_fox/source`, whose base is
`f4b48ad`) and `C:\Dev\oppi`, the `Op-lite/victory-scarlett` fork at
`c45b43e` (base `f4b48ad`). Neither repository contains game files.

The trees descend from avatarmanz `0a43566` by different routes, so the code
differs a lot: our fork is avatarmanz plus `another-victory-fork`'s work and
the renderer ports, theirs are the cool_fox line (Op-lite's six commits) plus
Scarlett's. Everything below was adapted by hand to our code, not
cherry-picked.

The branch is `impl-cf-oppi`, one commit per item. Switches that could change
the picture or the frame follow the rule here: **the default is the behaviour
every figure in the README was measured with**, and the new behaviour is an
environment variable until a run says otherwise. The one exception is early
depth, which is on like the console bit it follows and can be turned off with
`VICTORY_EARLY_DEPTH=0`.

## Ported

### 1. `VICTORY_E5_TARGETS` (cool_fox, Neptune-VE)

The title renders to R9G9B9E5, which no PC GPU does; R16G16B16A16_FLOAT has
stood in. `VICTORY_E5_TARGETS=r11g11b10` makes R11G11B10_FLOAT the stand-in:
32 bits a texel as on the console, unsigned, no alpha, a negative channel
stored as 0 (the 16-bit stand-in keeps it; magenta on screen is what a
missing green looks like, and a history carries the error for seconds).

Their run cf-col1: with it and their NaN guard, walls and skin were no
longer blotched. Ours defaults to `r16g16b16a16`; try `r11g11b10` on the
sticky purple patches and look for banding (6/6/5 bits of mantissa against
R9G9B9E5's 9). It changes target formats, so pipelines are rebuilt in that
run.

`crates/victory-gpu/src/resource.rs`: `e5_stand_in`, `e5_stand_in_for`,
`target_format`; unit test `r9g9b9e5_targets_stand_in_as_the_switch_says`.

### 2. `VICTORY_AMBIENT_LEFTOVER` (cool_fox, Neptune-VE)

With `VICTORY_AMBIENT` on and rays off, the stand-in kept the larger of the
title's leftover light and its estimate per channel, with the leftover's
brightness bounded but its hue kept. Without rays the leftover is whatever
the probes' memory held: their run cf-skin0 had a sheet of paper on a wall
glowing magenta. `=hue` keeps only its brightness in the estimate's colour;
`=off` writes the estimate alone (their default, which made the sheet
white). Ours defaults to `keep`.

`crates/victory-gpu/src/ambient.rs`: `leftover`, `leftover_mode`; one more
root constant (`uint leftover`) in the stand-in's passes; unit test
`settings`.

### 3. `VICTORY_WINDOW_CARD_PERCENT` (cool_fox, Neptune-VE)

A new window outside buffers gets a copy in the card's memory while a fixed
fifth of the card's budget is free (the `budget / 10 * 8` line). The switch
moves that line between 50 and 95 %, default 80 (the same line to the byte;
a unit test pins that). Their run nep-chk1: the card passed 80 %, textures
took the room the windows let go, and the windows' copies in PC memory grew
4 MB -> 2,764 MB in three minutes, private memory 29 -> 35 GB. A card over
its budget wants the windows on the bus; a card under it wants them in card
memory, so try `90` once the card is under budget.

`crates/victory-gpu/src/shadow.rs`: `window_card_percent`,
`window_card_percent_of`, `window_card_line`; two call sites; tests
`window_card_tests`.

### 4. `VICTORY_EARLY_DEPTH` (oppi, on by default; `=0` off)

A pixel program whose `DB_SHADER_CONTROL` has bit 12 runs its depth and
stencil tests before the shader on the console (HLSL's
`[earlydepthstencil]`). The host was never told, so it tested after the
shader: a program that writes unordered views wrote them for pixels the
test then rejected, and a later pass copied that over the character (their
glass prepass wrote depth and motion for rejected pixels). The flag is the
entry's shader flags bit 3 (`ForceEarlyDepthStencil`); `dxc` writes it as
`!{i32 0, i64 8}` with no SFI0 bit, checked here by compiling the same
shader with and without `[earlydepthstencil]` with the 10.0.26100 dxc and
diffing `-dumpbin`. Not declared when the program exports depth, stencil or
coverage (`DB_SHADER_CONTROL` bits 0-2 and 8, or `SV_Depth`,
`SV_DepthLessEqual`, `SV_DepthGreaterEqual`, `SV_Coverage`,
`SV_StencilRef`).

`crates/victory-shader/src/emit/lower.rs`: `early_depth`, `early_depth_on`,
set in `Lower::new`; `emit/shader.rs` `flags::EARLY_DEPTH`; test
`a_pixel_program_tested_before_it_runs_declares_early_depth`. Cheap and
picture-safe (the console bit is the console compiler's own condition);
worth an A/B on a scene with glass or a prepass.

### 5. `VICTORY_STAGING_REST_MS`, `VICTORY_STAGING_PACK` (oppi, Fox F)

Staging buffers are pooled in 64 MiB blocks; a piece given back rests 2.5 s
and empty blocks are kept up to `STAGING_KEPT` (8, 512 MiB). Their run-121
held 0.7-3.4 GB of staging for 60-240 MB live. `VICTORY_STAGING_REST_MS`
moves the rest (a piece comes back only when the last list that recorded its
copy is reset, so the rest is a second guard; try 250, 0);
`VICTORY_STAGING_PACK=1` places a piece in the fullest block it fits (as
`VICTORY_HEAP_PACK` does for textures) and keeps two empty blocks, so sparse
blocks empty and go. Defaults are today's behaviour. Watch the memory
line's staging MiB and the blocks-made count not rising.

`crates/victory-gpu/src/pool.rs`: `staging_rest`, `staging_pack`, `Pool::rest`;
test `a_pool_may_rest_shorter_than_the_default`.

### 6. `VICTORY_SEND_SPARE_MB` (oppi, Fox F)

A submission's changed bytes are copied through a mapped upload buffer kept
spare after the submission ran, up to 768 MiB (256 with `VICTORY_SEND_BAR`);
their meter found 0.6 GB kept. The switch sets the cap, unset keeps the old
one. On 32 GB: 128 MiB still covers a frame's 73-93 MB of sends (run
card-3); a bigger send makes its buffer anew.

`crates/victory-gpu/src/host/send.rs`: `kept`.

## Read and left out, with the reason

- **Textures keeping only the levels their views show** (their largest single
  gain: 41 -> 27 GB, re-uploads 6.4 GB -> 14 MB). It is not code in the drop:
  their launcher stopped setting `VICTORY_LEVELS_ON_DEMAND=1`, which on their
  base switched held levels off and gave every placed texture its whole
  chain. **Our tree already has it**: `scripts/play.ps1` does not set the
  switch, `tiles::wanted()` is off by default and `resource::held_only()` is
  on (`VICTORY_HELD_LEVELS=0` turns it off). Nothing to port; do not set that
  switch in a launcher.
- **PR #927 half views** (`VICTORY_HALF_VIEWS`: 16-bit channels moved through
  unsigned-integer views, fixes frozen faces and a blurred disc in
  cutscenes). Absent here entirely: no `halves.rs`, no `HostShader::half_views`,
  no host twin view or fresh table per draw. Their own note scopes the
  visible failure to NVIDIA float views reading zeros; on AMD most of the
  same cases are d16 reads, not half-view table places. It is a dedicated
  session (~2 files in the translator, ~3 in the GPU crate, a cache-key bump,
  a new per-draw table path), and only worth it if faces or cloth misbehave
  on this card.
- **`VICTORY_D16_BITS`** (keep the low 16 bits of a d16 read through an
  integer view; ~45 lines, 1,450 mesh programs change). The strongest shader
  accuracy candidate after early depth, but it has no test in their tree and
  touches typed-image semantics; take it with a picture A/B, not blind.
- **`VICTORY_TABLE_MEMO`** (their biggest descriptor win: 81 % of ~54,000
  table entries a frame are already ready earlier in the list). Needs an
  entry memo keyed on the list's epoch plus `readable` hooks in `draw.rs`, and
  the aliased-view guard from their `memo_copy` or it reintroduces a stale
  read; ~350-450 lines. Its own session.
- **`VICTORY_LIST_ALIAS_EXACT`** (a table entry known ready is made readable
  again when its view is an aliased texture). Their fix assumes
  `texture::alias` copies again after a write that does not change the
  texture's state; **our `texture::alias` skips exactly that case**
  (`first_use` and `transition` both unchanged), so the switch would add a
  lookup and change nothing. The real fix is fxceless `19a45d1`'s
  `alias_source_changed`, a separate port.
- **`VICTORY_PAGE_POOL`** (textures on 64 KiB pages: ~1.2 GB less texture
  memory, no frame cost). ~1,800 lines with a page mover their own notes say
  loses the device on the test card. Not for one sitting.
- **`VICTORY_POOL_CLASSES` / `_CAP` / `_DRAIN`** (2.0-2.9 GB of holes in
  texture blocks): ~500-600 lines of pool surgery.
- **`VICTORY_DROP_DXIL`** (let a translated shader's host code go after its
  pipelines are made): at most ~0.1 GB of 30, plus a disk read per later
  pipeline.
- **Store-side NaN guard** (`VICTORY_NAN_GUARD_STORES`): their image-store
  guard has no format check and our own measurement turned a packed colour
  `0xFF800000` into -65504. Skip; colour-export NaN is the safe half and
  would come with the d16/NaN pass if ever taken.
- **FSR DLL swap** (`VICTORY_FSR_DLL`): loads a PC FidelityFX DLL the user
  supplies, replaces the title's upscaler, and bilinearly scales any frame
  the DLL refuses. Not a port (and needs a downloaded binary).
- **Keyboard and mouse, auto-input, window icon**: input and tooling, not
  speed; this fork has its own mouse controls and scripted input.
- **`VICTORY_BAKE_DELTA`** (a bake's source already in its host copy is not
  uploaded again): our bake pass has no pending-slot promote path to hook;
  the texture-side half alone would be a half fix.
- **Early texture budget defaults 12 -> 40 ms and 1,500 -> 10,000 ms**:
  their pop-in fix for the first cutscene's faces. The pass runs on the
  thread that records draws, which is near our wall; try
  `VICTORY_EARLY_TEXTURES_MS=40` as an environment test, no code.
- **`VICTORY_FRAME_INTERVAL_MS` / `VICTORY_TITLE_FPS`** (their frame-cap
  lift): already here (`scripts/play.ps1 -Uncap`).
- **`VICTORY_HOLDERS`, `VICTORY_CLEAN_STAMP`, `VICTORY_ASK_KEPT_EARLY`**:
  diagnostics or "built, exact, no gain shown" in their own notes.

## What a run should look at

| Switch | Run | Look at |
|---|---|---|
| `VICTORY_E5_TARGETS=r11g11b10` | purple spots, walls, skin | the patches gone; no banding in skies or gradients |
| `VICTORY_AMBIENT_LEFTOVER=off` | the cell block, translucent surfaces | paper and glass white, not magenta; no new flatness |
| `VICTORY_WINDOW_CARD_PERCENT=90` | long walk | card in use against budget, the windows' PC-memory copies in the memory line |
| `VICTORY_EARLY_DEPTH=0` against on | glass, prepass scene | the character/glass overlay; card time (`VICTORY_GPU_TIMING`) |
| `VICTORY_STAGING_REST_MS=250` + `VICTORY_STAGING_PACK=1` | any heavy streaming stretch | staging MiB in the memory line, blocks made, frame times |
| `VICTORY_SEND_SPARE_MB=128` | any | commit, the send buffers in the holders/memory line |

No run was made for this branch: every gain above is from their measurements
or from code reading. The diagnostics build is
`cargo build --profile diag -p victory --features diagnostics`, and the
existing `docs/findings.md` and `docs/replay.md` say how to read the frame
log.
