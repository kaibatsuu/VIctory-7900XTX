# Unified memory and async streaming: the spec against the layer

What of the "Console Unified Memory & Async Streaming" architecture note
applies to this layer, what was already here under other names, what this
change adds, and where the note's prescriptions do not fit this layer's
concurrency model. Everything below is about `crates/victory-gpu` and was
checked against this tree on 2026-11-26.

## What the layer does today

The console's one pool is emulated by *mirroring* it: every title resource
has a host resource, and every buffer and window of title memory has a copy
on the CPU's side and, where it pays, one in the card's memory. A title's
`D3D12_GPU_VIRTUAL_ADDRESS` is the title's own CPU address (`memory.rs`,
`shadow.rs`), and the host GPU addresses are ours, handed to shaders through
descriptors the layer rewrites (`descriptor`). No title address is ever
given to the host GPU.

| The note's item | Where it lives | State |
|---|---|---|
| Capability probe (`ARCHITECTURE`, `OPTIONS16`) | `hardware.rs` (new) | **added**: the host device is asked once, at creation; the title is still told the console's one pool (`device.rs:296`) |
| Tier 1: one pool, custom L0 write-back | `hardware.rs` `mapped_properties`; `host.rs` `shared_buffer` already made such buffers | **added** as the tier's heap; the shader-read copies still live in the PC's memory |
| Tier 2: `GPU_UPLOAD` (ReBAR) | `host.rs` `mapped_card_buffer`; used by `VICTORY_WINDOWS_BAR_MB` and `VICTORY_SEND_BAR` | existed, opt-in; now also reachable on Tier 1 |
| Tier 3: `UPLOAD` then copy | `host.rs` `upload_buffer`, `pool.rs` staging, `host/upq.rs` | existed |
| Guest address translation table | `shadow.rs` (buffer copies), `memory.rs` (64 KiB page directory by address), `upload.rs` (arenas in `[4 GiB, 2^37)` so 40-bit addresses reach them) | existed |
| Fenced ring-buffer allocator | `host.rs` `scratch_at` (per-list chunk linear allocator), `PreludePool` (fence-keyed free list) | **not** the note's design; see below |
| Async DMA on copy queues | `host/upq.rs` (up), `texture/ahead.rs` (first texture uploads), `relay` fetch (`host.rs:5030`) | existed, all opt-in |
| Cross-queue fences (`Queue::Wait`) | `upq.rs:194` (drawing queue waits for the up lane) | existed in one place only |
| Resource state decay | modelled per list (`host.rs:7163`); ahead textures stay in `COMMON` and rely on promotion (`ahead.rs:11-13`) | existed |
| Write-combined safety | `mapped_card_buffer`'s contract, `window_mapped_for` writes only, shadows hold the CPU's side | existed as a convention, not enforced |

So the note's sections 2 and 5 mostly describe machinery this layer already
has, in a form shaped by its own constraint: the title must keep seeing one
pool, so the layer cannot simply hand the card a mapped console pointer the
way a straight DX12 title could.

## What this change adds

`hardware.rs` asks the host device the two questions the note's section 3
opens with:

* `D3D12_FEATURE_ARCHITECTURE` → `UMA`, `CacheCoherentUMA`;
* `D3D12_FEATURE_DATA_D3D12_OPTIONS16` → `GPUUploadHeapSupported`.

It then picks the note's tiers (`Unified` when both architecture bits are
set, `MappedCard` when the upload heap is there, `Discrete` otherwise) and
keeps the answer for the process. `VICTORY_MEMORY_TIER=unified|mapped|discrete`
overrules it, for an A/B between two runs on one machine (the tier is read
once, at creation).

The tier is used where the layer already asked "can the CPU map memory the
card reads?":

* `Host::mapped_card_memory` (`host.rs`) no longer asks `OPTIONS16` itself;
  it asks the tier. On an APU (`Unified`) the answer is yes even though the
  card has no `GPU_UPLOAD` heap, which is exactly the note's Tier 1: the
  PC's write-back pages are the card's memory.
* `Host::mapped_card_buffer` creates its buffer in the heap the tier names
  (`hardware::mapped_properties`): custom write-back L0 on `Unified`,
  `GPU_UPLOAD` on `MappedCard`. The two opt-in users
  (`VICTORY_WINDOWS_BAR_MB`, `VICTORY_SEND_BAR`) therefore work on an APU,
  where `GPU_UPLOAD` does not exist, and the log line says which heap was
  picked.
* The device creation logs the answers and the tier once
  (`the PC's memory under the card`), so a run's hardware is in the log.

Tests: the tier table, the switch's spelling, and the two queries against
the software adapter (`cargo test -p victory-gpu hardware::`).

## Why the note's ring allocator is not imported as written

The note's `GpuRingBuffer` is correct for one producer and one submission
stream: allocate, record, signal, watermark the head, reclaim everything
below the completed value. This layer records lists on several worker
threads, and a submission's lists are recorded *before* the queue is told to
execute them; submissions can overlap under `VICTORY_LATE_WAIT`. An
allocation for submission N+1 can therefore precede the signal of N, and a
watermark taken at N's signal would free bytes that N+1's lists still name.
That is the same hazard the note warns about for the *title's* writes,
turned on the allocator itself.

The two places that need a ring already have its guarantees by other means:

* per-list scratch and upload memory (`host.rs` `scratch_at`, `upload`) is a
  linear allocator whose tail is the list's reset, and the layer only
  recycles a list once its submission has run (`list.rs:280`), so the
  reclamation point is the submission fence without a separate watermark;
* preludes are a fence-keyed free list (`host.rs` `PreludePool`), reclaimed
  from `GetCompletedValue()` — the note's watermark, with the fence value
  recorded at submission rather than at allocation.

A per-submission ring would fit where the layer *does* have one owner and
one fence: the `up` lane (`upq.rs`) and the ahead-upload lane
(`texture/ahead.rs`), both of which already keep one bounce/staging buffer
per submission rather than a ring. Porting the note's allocator there would
remove allocation churn, not correctness risk; it is listed under remaining
work below rather than done here, because the churn it saves is already
pooled (`pool.rs` staging blocks, `Spare` chunk reuse).

## Hazards found while checking the note's synchronization rules

These are pre-existing; they are recorded here because the note's checklist
is exactly the audit that finds them.

1. **Ahead upload vs. a direct re-upload, no queue wait.**
   `texture/ahead.rs:151` (`listed`) waits at most 1000 ms for the copy
   queue's job to land; then `texture.rs:878` records `ensure_state(COPY_DEST)`
   and `texture.rs:941` copies into the same subresources on the drawing
   queue. If the job is still in flight, two queues write one texture with
   no `Queue::Wait` between them. The note's rule — the consuming queue
   waits on the producer's fence before `ExecuteCommandLists` — is missing
   here. Proposed fix: `listed` returns whether the job landed; when it did
   not, the submission path records `drawing.Wait(ahead_fence, value)`
   before executing (the ahead lane already has a fence per worker,
   `ahead.rs:212`), or the re-upload is deferred a frame. Gate on
   `VICTORY_UPLOAD_AHEAD`, which is off by default.
2. **Copy-queue fetch of a buffer in a shader-resource state.**
   `shadow.buffer` is transitioned to `NON_PIXEL_SHADER_RESOURCE` on the
   drawing queue before the compare (`host.rs:4665`), and the landing fetch
   copies out of it on the copy queue (`host.rs:5064`) without a
   `COPY_SOURCE` transition. Buffers are promoted from `COMMON` for copies,
   which is what the layer relies on elsewhere (the same convention as the
   old `queue2` lanes); a buffer that is explicitly in another state is the
   case the note's decay rules call out. It is latent: the copy-queue fetch
   is `VICTORY_RELAY_COPY=1`, off by default. Proposed fix: transition the
   shadow buffer back to `COMMON` on the drawing queue before the fetch (or
   keep the fetch on the drawing queue), and let the debug layer prove it.

Both are gated behind switches that are off by default, so a default run is
unaffected.

## What was not done, and why

* **Teardown of the shadow/relay on `Unified`.** On an APU the CPU's side
  and the card's side are one pool; the correct long-run shape is the
  `inplace` heap (`inplace.rs`, `OpenExistingHeapFromAddress`) or direct
  mapped memory, with no relay. That changes resource lifetime, page
  protection and the title's ability to free memory; it needs the title to
  run, which this checkout cannot (no assets). The probe makes the decision
  available; the teardown is the next step, not a safe blind port.
* **Defaulting the BAR windows on Tier 2.** `VICTORY_WINDOWS_BAR_MB` is 0 by
  default because the win it showed in a capture (29 → 26 ms of card time)
  was not measured against the memory it takes from textures. The probe does
  not change the default; `VICTORY_MEMORY_TIER` + the switch is the A/B.
* **Explicit `COMMON` → `PSR` transition on the drawing queue for ahead
  uploads.** The layer relies on implicit promotion from `COMMON` (and never
  records shader-resource transitions on a copy list, which is what the note
  requires). Adding the barrier would be a no-op on the host and is not
  needed; the note's rule is satisfied by never recording it on the copy
  list.

## Validation plan

Build and unit tests:

```
cargo test -p victory-gpu hardware::
cargo check --workspace
cargo build --profile diag -p victory --features diagnostics
```

The probe's own line, on any run (`VICTORY_LOG=info`):

```
the PC's memory under the card: uma=... cache_coherent=... gpu_upload=... tier=...
```

On an RX 7900 XTX (this machine's card) expect `uma=false`, `gpu_upload=true`,
`tier=mapped card memory (BAR)`; on an APU expect `uma=true`,
`cache_coherent=true`, `tier=unified`. A wrong tier can be forced with
`VICTORY_MEMORY_TIER=discrete` (or `unified`) and compared; the overrule is
logged.

Behaviour under each tier, with the existing opt-in paths:

```
set VICTORY_MEMORY_TIER=unified
set VICTORY_WINDOWS_BAR_MB=256
target\diag\victory.exe warm
```

Expect `memory the CPU maps and the card reads: supported=true tier=unified`
and the window path's counters (`shadow.rs` `WINDOW_MAPPED_BYTES`) to move;
on a discrete card the same run must create the same buffers in `GPU_UPLOAD`
(tier `mapped`) or fall back to copies (`VICTORY_MEMORY_TIER=discrete`,
`supported=false`). `VICTORY_D3D12_DRED=1` and a debug layer run
(`VICTORY_D3D12_DEBUG=1`) should show no new messages: the heap properties
are the only change on this path.

For the two hazards above, the switches that reach them:

```
set VICTORY_UPLOAD_AHEAD=1
set VICTORY_RELAY=1
set VICTORY_RELAY_COPY=1
set VICTORY_D3D12_DEBUG=1
```

The debug layer is expected to report the fetch's state mismatch (hazard 2)
on a build with it on; a fix is in scope once the title can be run here.

## Remaining work, by value

1. Fix hazard 1 (ahead timeout) with a queue wait or a deferred re-upload.
2. Fix hazard 2 (fetch state) or move the fetch back to the drawing queue.
3. Measure `VICTORY_WINDOWS_BAR_MB` on Tier 2 with the probe's log and pick a
   default; measure `VICTORY_MEMORY_TIER=unified` on an APU.
4. Per-submission ring for the up and ahead lanes, if profiling still shows
   allocation or zeroing time there (`pool.rs` staging is 64 MiB blocks;
   `upq` sizes bounce buffers by doublings).
5. Teardown of the relay on `Unified`, behind its own switch, once a run can
   be made here.
