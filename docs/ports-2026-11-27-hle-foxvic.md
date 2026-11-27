# Ports from LibsMQG/hle-d3d12-runtime and randomkidmakesthis/foxvic-scarlett (2026-11-27)

Two forks were read on 2026-11-27, and every candidate was then put through an
independent skeptic pass that had to accept it as a real improvement before it
was ported. One commit per item, each naming the upstream commit it came from.

- [LibsMQG/hle-d3d12-runtime](https://github.com/LibsMQG/hle-d3d12-runtime) at
  `1ef9e07` (95 commits, unrelated history: a RYUKI-7900XT-based fork with the
  avatarmanz renderer ports, XaviFortes' submit changes and a set of
  correctness fixes from the community "all forks" merge).
- [randomkidmakesthis/foxvic-scarlett](https://github.com/randomkidmakesthis/foxvic-scarlett)
  at `91c0fc4` (1,352 commits; a merge of readmet3xt-lab/foxhound and
  Op-lite/victory-scarlett; shares our ancestor `0a43566`).

## What foxvic is, and is not

foxvic's tree merges foxhound (NVIDIA DLSS / Frame Generation / 1440p) into
scarlett's tree, but it keeps **foxhound's** GPU and shader crates and carries
scarlett's per-switch GPU work only as history it never compiled. Every switch
its own `docs/merge-status.md` lists as "still to port" (`TABLE_MEMO`,
`LIST_ALIAS_EXACT`, `LIST_HOLD`, `TABLE_NAMED_ONLY`, `PROBE_RAYS`,
`PLANE_NOW`, `TARGET_VARIANTS`, `HALF_VIEWS`, `POOL_CAP`, `MAP_LOCK_FINE`, and
the three we already have) has zero occurrences in its `crates/`, `apps/` and
`xtask/`: foxvic is not a source for those. Its usable work on this PC is
foxhound's CPU side; its NVIDIA content (`DLSS*`, `FRAMEGEN*`, `STREAMLINE_*`,
`FSR_PATH`, `crates/victory-gpu/src/streamline*`) cannot run on RDNA3 and was
excluded from every candidate.

## Taken from hle-d3d12-runtime

Defects proved by reading the code, not by a benchmark. Each landed with the
upstream test where one existed.

| What | Where | Source |
|---|---|---|
| a queue is unlinked from the waiting list before it is freed (`release` vs `release_held`) | `victory-gpu/src/queue.rs`, `object.rs` | `93fd103` |
| fences held in queue waits, queue signals and late signals keep a COM reference (`FenceRef`) | `queue.rs`, `fence.rs`, `host.rs`, `queue/behind.rs` | `b1db852` |
| a texture's 4 KiB alignment survives `GetResourceAllocationInfo{,2}`; a fallback size rounds to its alignment | `resource.rs` | `4e6784b` |
| a query heap with no queries is refused | `query.rs` | `01a18af` |
| two frame waits that wake together take two ticks | `frame.rs` | `94f6766` + `c85d6f1` |
| a snapshot allocation is retried after idle snapshot buffers are let go | `shadow.rs` | `4027df7` |
| a null constant-buffer write replaces the slot's old descriptor | `descriptor.rs`, `descriptor/host.rs`, `draw.rs` | `e9abefb` |
| a dropped view heap lets go of its cached descriptor run | `descriptor.rs`, `memory.rs` | `9c1751d` |
| a placement claims its pages at once, gives back exactly the pages it locked, and refuses a title buffer in place under late waits | `inplace.rs`, `port.rs` | `7b10773` |
| `VICTORY_PORT_BUFFERS`: title buffers in place above 4 GiB (off by default) | `port.rs`, `shadow.rs`, `host.rs` | `d6e6e4e` + `c322268` |
| XGameSave: blob count validated before reading; duplicate blob change refused; replaced blobs' excess counted before clamping; null out handles on failure; migration aborted when a blob read fails | `hle/modules/xgamesave.rs` | `19b5914`, `d48ca85`, `bafbbb0`, `67f78db`, `a7f8c55` |
| DirectStorage factory null-checks and clears its out pointer | `hle/modules/dstorage.rs` | `8cc1256` |
| a non-gamepad reading clears the pad state it is handed | `hle/modules/gameinput.rs` | `bbb93cf` |
| APU heap allocations are refcounted, so `ApuFree` cannot free under the XMA decoder | `hle/modules/audio.rs` | `7bc5acb` |
| a `clzero` loop's compare must be `cmp rax, reg` | `loader/src/console_insn_decode.rs` | `4ed1ea9` |
| `v_div_fixup_f32`: `0.0/x` and `x/±Inf` were turned into `±Inf` | `victory-shader/src/emit/lower.rs` | `89a6e75` |
| `_g16` image samples pack 16-bit gradients in `image_addr_dwords` independent of `a16` | `victory-shader/src/gfx10/fields.rs` | `d26b06d` |
| `annotateHandle` stays block-local; sharing one across blocks crashed AMD's DXIL compiler | `victory-shader/src/emit/bitcode/opt.rs` | `d319e9b` |

Adaptations worth naming: the fence fix keeps our `do_signals()` and routes it
through a `late_or_now()` built on `FenceRef`; the 4 KiB alignment fix keeps
our port-5 arms in `draw.rs` and lets `Constant(0)` fall through to
`make_at`; the in-place fix keeps our write-watch `pin()` bookkeeping and makes
the port's `counted()` reserve the same page-rounded bytes that `gone()`
releases.

Not taken from hle, with the reason:

- helper-lane `v_readfirstlane` and quad reads: already ours
  (`first_lane`/`helper_own`, default on, and `helpers_read`/`quad_pick` at both
  sites), so the upstream fix is a duplicate, not a defect we have.
- PSV0 names: deduplication already yields valid NUL-terminated names at valid
  offsets; no named artifact, and every shader's container would change.
- export target 16 = `pos4`: the table feeds only the disassembler's text.
- always-true/false compare lowering and wave-wide lane-read retries: no AMD
  datapoint and a silent translation change on the default path.
- slot bits, dynamic descriptor bits, the pending flag, wake-one, AVX2 compare
  in `Shadow::take`, the prelude pool: our tree already has the equivalent
  first two (`Queued::may` in both hot sites, `SPANS_QUICK` with its own A/B),
  and the rest are unmeasured or measured worse by their author (the prelude
  pool costs +1.6 % frame time; AVX2 "no difference"). Porting the slot-bits
  rewrite would have replaced proven machinery, so it stays unported.
- CPU_STATS / MEM_OWNERS diagnostics: a re-implementation on their relay
  taxonomy, not a port; DIAG-3 (the 5 s DirectStorage report, `DISK_STATS`)
  is trivial to add when the disk is next looked at.

## Taken from foxvic-scarlett

Both are foxhound CPU-side early-outs, put behind switches **off by default**:
our defaults stay the behaviour this fork's figures were measured with.

| Switch | What it does | Source |
|---|---|---|
| `VICTORY_STAGED_ONLY_TITLE=1` | the index-picked texture pass asks `QueryWorkingSetEx` only of textures made at an address the title mapped (20,000-57,000 questions per 5 s, most of them table reads, for 66 pages found away in a whole run; their run `coolfox-20261126-223059`: 18-111 us each, 400-700 ms per 5 s) | `36c4c37` |
| `VICTORY_EXACT_NO_WAIT=1` | an exact refresh before a bake reads the texture whole through the kernel and waits for a remap only if a page is away (808 of 866 ms/frame of refreshes waited in a cutscene second, diag run 2026-11-26 15:58) | `3cb7b8f` |

`VICTORY_STAGED_ONLY_TITLE` is named differently from upstream (`STAGED_ASK_ALL`)
because the default had to stay "ask every texture"; the predicate is the same
"made at a title address", and our in-place heaps are why it is written as a
resource property rather than a heap one.

## Read and not taken from foxvic, with the reason

- **Soft views (`SOFT_VIEWS`):** the largest claimed win in the fork (445,503
  maps = 20.1 s and 349,175 unmaps = 32.6 s in a 143 s run, a cutscene streaming
  thread 846 ms of 1.7 s). But it is ~2,700 lines over scarlett's
  `xmem`/`watch` architecture, and it needs `VirtualQuery`/`VirtualProtect`
  interposition in our HLE that we do not have; the numbers are scarlett's, not
  ours. Instrument our own `MapViewOfFile3`/`UnmapViewOfFile2` counts and time
  first (the timeline spans), then an in-run A/B; not a merge to make blind.
- **Tabled reread (`TABLED_REREAD`):** our `tabled_in` already takes a slot out
  at its first table read; the delta is a second take-out that changes picture
  timing (our scene-load flicker class). Needs a flicker A/B.
- **Bake sources (`BAKE_SOURCES`):** the best picture story on either side (our
  own `draw.rs` names the same specks, run n01), but 342 lines that need a
  before/after face capture on this build, and synchronous uploads add a bake
  stall under memory pressure.
- **Write-combined no-back (`WC_NO_BACK`):** needs a write-combined range table
  we lack, and the author's own run went near black with every copy left.
- **Static-sampler pipelines (`LIBRARY_STATIC_SAMPLERS`):** an NVIDIA-only
  reproducer; on AMD the guard could cost ~1.6 k pipeline compiles a run.
  Worth one A/B if the composite posterizes.
- **Ambient specular (`AMBIENT_SPECULAR`):** invented lighting for missed rays;
  the slots must be validated against our translated `CS_RayMiss` set.
- **Fresh open (`FRESH_OPEN`)** and **heaps first (`HEAP_SYNC_FIRST`):** ~1 ms
  and 2.7-5.8 ms/frame respectively, both coherence-timing changes; the second
  collides with our renderer port 7 (`port_journal`) and needs a hang-watched
  A/B of its own.
- **Native 1440p:** the frame-time gain belongs to the 1440p output change we
  already have (`VICTORY_OUTPUT`, `VICTORY_RENDER_SCALE`); what is left is a
  Settings-panel viewport layout fix.
- **3D-filter workaround (`FILTER_3D`):** superseded by the pipeline-library
  fix its own author later named.
- **Scarlett's per-switch GPU work:** not in foxvic's tree at all (see above);
  the scarlett line was audited in
  [ports-2026-11-26-cf-oppi.md](ports-2026-11-26-cf-oppi.md).

## How to prove the two foxvic switches

Both are counting-visible with our own meters, no rebuild needed to A/B:

- `VICTORY_STAGED_ONLY_TITLE=1`: run the same route and watch the texture pass's
  split line ("rest" per 5 s) and the frame-time dump (`FRAMETIME_DUMP`); the
  pass's unattributed time should fall by the ask count, and no new "not all
  there" texture should appear.
- `VICTORY_EXACT_NO_WAIT=1`: watch the exact-refresh line added to the frame
  rate (read through the kernel vs waited); a bake's sources should stop waiting
  with no new specks on a face close-up.

No build or run was made while merging. The defaults are unchanged; the single
verification pass (tests) was run once at the end, and the gameplay figures
remain the user's to produce.
