# Task-07: depth of field — the depth read, the tile reduction, the composite

## Problem

Depth of field is wrong in four shapes a player can name: the near and far
planes act inverted; foreground characters wear a hard halo and bleed into the
background; bokeh highlights show a blocky grid; or the whole screen blurs by
the same amount. One of the four has a host-side cause this audit fixes on the
target GPU: the depth read (bullet 1). The tile pass was inspected and the
target's 64-lane path needs no change (bullet 2); the composite's blend and
sRGB translation are carried as the title has them (bullet 3). The
inverted-planes shape has no host-side cause here (the sign and range of the
CoC are the title's constants and shader math, which the layer passes
through), and the DRS rect is the title's (bullet 4), so both are recorded
under Open questions instead of claimed.

1. **The circle of confusion reads the depth as zero or as the wrong number.**
   The CoC pass samples the depth buffer through a shader resource view. On the
   console the view may name the depth format the texture was made in
   (`D32_FLOAT`, `D24_UNORM_S8_UINT`, `D32_FLOAT_S8X24_UINT`), or name none at
   all ("the texture's own"). A host shader resource view may name neither a
   depth format nor a typeless one: for a depth texture it must name the colour
   format of the plane it reads. A focus distance computed from a wrong or zero
   depth is a plane at the wrong distance, or at infinity: the whole screen
   blurs, or nothing does.
2. **The tile classification loses lanes.** The CoC tile pass reduces min/max
   over each console wave of 64 lanes and hands the result to neighbouring
   pixels through LDS. The console wave64 program is lowered as a 64-lane host
   wave (the target GPU, whose maximum wave is 64) or, on a host whose waves
   are at most 32, as two 32-lane waves that exchange their halves through LDS
   and group barriers. A lane read, a DPP row mask or a missing group barrier
   taken at the host's lane instead of the console's corrupts the dilated CoC
   buffer: the blocky grid and the hard halo.
3. **The composite blends twice.** The final draw puts the half-res blurred
   colour over the sharp foreground with premultiplied alpha
   (`ONE` / `INV_SRC_ALPHA`). Rewriting those factors, or dropping the sRGB
   conversion of the target, double-multiplies the blurred colour: a bright
   fringe around characters.
4. **The DRS sub-rect is not carried.** A dynamic-resolution frame renders into
   a sub-rectangle of the target. The rect reaches the GPU as a viewport and a
   scissor; a pass that samples half-res buffers outside the rect smears the
   edge of the picture.

## What was found

- **The depth view format was wrong, and views without a description were
  dropped.** A depth texture a shader reads is made typeless on the host
  (`resource.rs`, `DEPTH_FORMATS`). A view that named `D32_FLOAT` (40) kept
  that format: the host refuses a depth format as a shader resource view. A
  view that named nothing was refused outright (`fits` returns false for a
  view without a format of a typeless resource), so the CoC shader read
  nothing. Both are fixed: `resource::depth_read_format` maps a plane's depth
  format to the colour format the host reads it in (`D32_FLOAT` → `R32_FLOAT`,
  `D24_UNORM_S8_UINT` → `R24_UNORM_X8_TYPELESS`, `D32_FLOAT_S8X24_UINT` →
  `R32_FLOAT_X8X24_TYPELESS`, `D16_UNORM` → `R16_UNORM`, the stencil plane →
  `X24_TYPELESS_G8_UINT` / `X32_TYPELESS_G8X24_UINT`); `descriptor::host`
  applies it to a described view and spells out a whole-texture view for one
  with no description (`whole_depth_view`). A view in a stencil format
  (`X24_TYPELESS_G8_UINT`, `X32_TYPELESS_G8X24_UINT`, or the console's 16-bit
  stencil format that maps to the first) reads plane 1, so the layer sets
  `PlaneSlice` 1 as well as the format; a plain 8-bit view of a stencil plane
  (`R8_UINT`/`R8_UNORM`) is left refused, because making it a host stencil view
  would also need the component mapping that a guessed view cannot have. The
  no-description view is only spelled out when the title made the texture in a
  *typed* depth format: a texture the title made typeless may be a colour
  texture, and the layer does not guess which typed view it meant. A
  multisampled depth texture's null view is spelled out in its multisampled
  dimension (`TEXTURE2DMS`/`TEXTURE2DMSARRAY`) in the same colour format; the
  end-to-end test shows the host takes it. This is the D3D12 rule the SDL
  D3D12 backend and the DirectX samples hit too: the resource is `_TYPELESS`,
  the DSV is the depth format, the SRV is the colour format
  (<https://git.eden-emu.dev/submodules-archive/SDL/commit/c81b62293a5e7d6e46535c22fad77808cbc33e7f.patch>,
  <https://stackoverflow.com/questions/38933565/which-format-to-use-for-a-shader-resource-view-into-depth-stencil-buffer-resourc>).
- **The depth read is already ordered.** A shader read of a depth target asks
  the list for both shader-read states (`SHADER_RESOURCE`, `draw.rs`), and
  `list::ensure_state` records the transition from the state the list tracked
  (`DEPTH_WRITE` → `0xC0`) before the draw or dispatch that reads it
  (`list.rs`). A resource the same draw also tests read-only keeps
  `DEPTH_READ | SHADER_RESOURCE` (`draw.rs`). No change was needed; no path
  binds a depth SRV without those bits.
- **The wave64 lifter is correct for the tile pattern, and one lane-mask bug
  was fixed.** The whole-wave section model simulates the console's 64 lanes
  and lowers a reduction to two host-wave reductions plus one combine
  (`emit/wave.rs`, `emit/lower.rs`); cross-half values travel through two LDS
  dwords with group barriers, and `s_barrier` becomes `barrier(9)` (or `11`
  with UAV writes), which is sync + group-shared-memory fence (+ device fence).
  `ds_swizzle_b32` and `v_permlane16`/`v_permlanex16` only touch lane bits
  [4:0], so each host wave serves its own half with the console's arithmetic.
  The title's shader set translates with zero refusals in the run logs, so its
  DoF passes are among the translated programs, not dropped.
  One real defect: a DPP write mask's row index was taken from the host lane
  (`(lane >> 4) & 3`), so a DPP with a row mask other than `0xF` in console
  lanes 32-63 tested rows 0/1 instead of 2/3 (`emit/lower.rs`). It now uses
  the console lane, as the whole-wave simulation already did; and a DPP with
  a row mask other than `0xF` now counts as crossing the console wave's
  halves, so a 32-lane host splits the wave with the exchange relabelling
  rather than evaluating the mask at the host lane; where it cannot split,
  the shader is refused (its `WaveSize(64)` is not one the host takes), not
  mis-lowered. `row_mask:0xF`, which the compiler's reduction sequences
  use, is unaffected. Scope: on the target GPU (maximum wave 64) a console
  wave64 program keeps 64-lane waves, where `lane >> 4` already gives the
  console's rows; this fix matters on a 32-lane host and for programs the
  layer splits, not for the tile pass on the target itself.
- **The composite blend and its sRGB target are carried.** `host_blend` only
  clears `BlendEnable` for integer render targets; `ONE` / `INV_SRC_ALPHA`,
  both alpha factors, the blend op and the write mask are the title's. `pc_format`
  and `target_format` leave `_SRGB` formats alone, so the RTV names the sRGB
  format and the host converts once, on the blend's linear values.
- **The layer has no DRS clamp to get wrong.** `RSSetViewports` and
  `RSSetScissorRects` are forwarded unchanged (`draw.rs`), so an offset
  sub-rect survives; the half-res gather's UV clamp is the title's shader, and
  the layer keeps no rect of its own to inject (the only dispatch-time
  injection point is the structure builder's thread count). `VICTORY_SCISSOR_TEST=full`
  is the one experiment that rewrites a scissor, and it is off.

## Non-goals

- Replacing the title's DoF with a FidelityFX pass, FSR 4 or any upscaler. The
  title's own passes are the ones under test; a replacement changes what the
  game draws.
- Rewriting shader source or adding a shader pass. The fixes are in the
  translation, not in the title's HLSL.
- A new default-on switch. The format mapping and the DPP row fix are default
  behaviour because they make a host-legal view and the console's lane
  semantics; neither is an experiment.
- A DRS rect the layer invents. The title owns its sub-rect; the layer only
  must not rewrite it.

## Reuse

- `crates/victory-gpu/src/resource.rs`: `DEPTH_FORMATS`, `family`, `pc_format`,
  `target_format`, `view_format` — the existing typeless-depth and stand-in
  machinery the mapping extends; the new `depth_read_format`,
  `depth_read_view`, `stencil_format`, `title_format`.
- `crates/victory-gpu/src/descriptor/host.rs`: `fits`, `srv_desc`, `make_twinned`
  — where a title view becomes a host descriptor (and where a view it cannot
  make becomes null); the new `depth_srv`, `srv_plane`, `srv_set_plane`,
  `whole_depth_view`.
- `crates/victory-gpu/src/list.rs`: `ensure_state`, `host_barrier` — the
  transition that orders the CoC read.
- `crates/victory-shader/src/emit/wave.rs`, `emit/lower.rs`: the whole-wave
  section model, `exchange_halves`, `console_lane`, the barrier before each LDS
  access, `s_barrier` lowering, DPP masks.
- `crates/victory-gpu/src/pipeline/host.rs`: `host_blend`, `GraphicsState::read`.
- Tests to copy: `resource.rs` `a_depth_texture_and_its_twin_copy_into_each_other`,
  `wave64_tests.rs` `a_sum_of_halves_over_the_wave_is_the_consoles_tree_bit_for_bit`,
  `pipeline/host.rs` `integer_targets_do_not_blend`.

## Seams

- The new log line (first 64 distinct format mappings) — catches a view whose
  format had to change; misses the texture the title never reads through a
  view, and says nothing about what the shader computes from it.
- `VICTORY_D3D12_DEBUG=1` (diagnostics builds) — catches a host view the debug
  layer refuses; misses a driver that silently accepts one.
- The depth/twin counters in the periodic line — catch a depth texture that is
  also unordered access; miss a plain depth read.
- `VICTORY_WAVE_WHY=1` — catches a console wave64 program that fell to the
  approximate per-host-wave lowering; misses a correct split whose result the
  title then uses wrongly.
- The AMD DXIL compiler's own crashes (`amdxc64.dll`) are a standing risk for
  new shader shapes and are handled by the pipeline isolation/blacklist. On the
  target GPU (maximum wave 64) the DPP fix emits exactly the DXIL it emitted
  before (console lane = host lane); only a 32-lane host takes the split path
  the crossing rule now selects.
- A screenshot of the same camera position, DoF on, before and after — catches
  the visible shape; misses a difference the window's scale hides.

## Behaviour

Rows marked **(manual)** are the ones only a run of the title can settle; the
rest have a test in this change.

| # | Given | When | Then | Proof |
|---|-------|------|------|-------|
| 1 | a depth texture the title made with `D32_FLOAT`, shader-resource access allowed | its view names `D32_FLOAT` | the host view is made in `R32_FLOAT`, the device is still there, and the mapping is logged once | tests: `a_depth_view_is_read_in_the_planes_colour_format`, `a_title_depth_texture_gets_a_host_shader_view` |
| 2 | the same, view names no format | the view is made | the host view is a whole-texture `R32_FLOAT` `Texture2D`/`Texture2DArray` view, not a null slot | tests: same two, `a_depth_view_without_a_description_reads_the_whole_plane` |
| 2b | a texture the title made `R32_TYPELESS` (not a typed depth format), view names no format | the view is made | it stays refused (the layer does not guess a typed view of a typeless colour texture) | test: `a_depth_view_without_a_format_is_read_in_the_planes_colour_format` |
| 2c | a multisampled depth texture, view names no format | the view is made | a `TEXTURE2DMS`/`TEXTURE2DMSARRAY` view in the plane's colour format, and the host takes it | tests: `a_depth_view_without_a_description_reads_the_whole_plane`, `a_title_depth_texture_gets_a_host_shader_view` (a 4-sample depth texture) |
| 3 | a `D24_UNORM_S8_UINT` texture, view names `X24_TYPELESS_G8_UINT` with `PlaneSlice` 0 | the view is made | the host view is that format with `PlaneSlice` 1 | test: `a_depth_view_without_a_description_reads_the_whole_plane` (`depth_srv`) and the mapping test |
| 3b | the same texture, view names `R8_UINT` on plane 1 | the view is made | it stays refused (the host stencil view would need a component remap the layer does not guess) | code: `depth_read_format` has no 8-bit arm; no test claims one |
| 4 | the CoC compute pass reads a depth target just written **(manual)** | the dispatch is recorded | the list transitions it out of `DEPTH_WRITE` into both shader-read states before the dispatch | code: `ensure_state` at the SRV bind (`draw.rs`); no new test (needs a device capture) |
| 5 | a console wave64 tile reduction over 64 lanes | it is lowered for a 32-lane host | the DXIL is two `waveActiveOp` halves plus one combine, with a group barrier and an LDS exchange between them | existing hardware tests `wave64_tests.rs`; **gap**: no runtime test of the min/max combine (only or/sum) |
| 6 | a DPP with `row_mask:0x7` in console lanes 32-63, on a host of 32-lane waves | it is lowered | the program splits with the exchange relabelling, and the row index comes from the console lane: rows 2/3, not 0/1; where it cannot split, the shader is refused (its `WaveSize(64)` is not one the host takes), not mis-lowered | test: none yet (the fix is exercised by the 306 shader tests only as a no-change for `row_mask:0xF`) |
| 7 | a composite PSO with `ONE` / `INV_SRC_ALPHA` on an `R8G8B8A8_UNORM_SRGB` target | the pipeline stream is read | the blend factors and the `_SRGB` target format are the title's, through `GraphicsState::read` and `host_blend` | test: `a_premultiplied_composite_keeps_its_factors_on_an_srgb_target` |
| 8 | a DRS frame with an offset viewport and scissor **(manual)** | the draw is recorded | `RSSetViewports` / `RSSetScissorRects` receive the same rect | code: `draw.rs`; no test asserts an offset rect |

## Must not cut

- Title rendering: default switches draw exactly what `beta` draws, except for
  views the host refused or would have refused: a depth-format SRV (not a host
  SRV format) now names the plane's colour format, and a no-description SRV of
  a typeless depth copy is made instead of left null. A view that already named
  a correct colour format is passed through unchanged.
- Defaults: no new switch; `VICTORY_SCISSOR_TEST` stays an off experiment.
- Diagnostics: the default build compiles with no diagnostic item referenced;
  `cargo test --workspace --all-features --locked` stays green apart from the
  known list.
- Safety: no new `unsafe` without a `// SAFETY:` line; the new `unsafe` reads
  a live resource's description and the union member the view's dimension
  names.
- Asset policy: `cargo xtask policy` passes; no game data in the repo.

## Slices

1. **Depth read (done).** Format mapping, the no-description view, stencil
   planes, and an end-to-end test that the host device accepts the views it
   makes; unit tests against the documented DXGI values. One skeptic round.
2. **Lane masks (done).** The DPP row index from the console lane and the
   crossing rule; the whole shader suite as the regression check. A hardware
   test for a non-`0xF` row mask in the upper half is the follow-up if the
   artifact persists on a 32-lane host.
3. **On the title (needs the user).** A capture of the first DoF frame before
   and after: the log's mapping lines say which view changed, the screenshot
   says whether the plane is right. `VICTORY_D3D12_DEBUG=1` must be quiet on
   the new views.
4. **If the grid/halo persists:** the tile pass's `VICTORY_WAVE_WHY=1` line
   says whether it fell to the approximate lowering; only then a capturable
   shader test.
5. **If the halo persists:** the composite draw's blend state and target
   format from the pipeline log, and a scissor/viewport dump on a DRS frame.

## Open questions

- Is the depth read one of the shapes the user sees, or does the title already
  name `R32_FLOAT` itself? The new log line answers this on the first run.
- **The inverted near/far planes have no host-side cause here.** The sign and
  range of the CoC are the title's constants and shader math, passed through
  unchanged; if the planes really are inverted, the cause is in the projection
  or CoC setup (constant buffer, a sign convention), not in the depth view, and
  this task does not fix it. Confirm the shape before crediting this change.
- Does the title dispatch its tile pass in groups (normal) or in threads
  (`THREAD_DIMENSIONS`)? The latter rounds up to whole groups and would run
  lanes past a DRS sub-rect.
- Is the composite target sRGB in this title, or an HDR float format?

## Decisions

- The depth view format is a correctness fix, not an experiment: a depth
  format is not a host shader resource view format, and a view without a
  format of a typeless resource is not a view. A stencil format names plane 1,
  so the plane is set with the format.
- No guessed 8-bit stencil view: mapping `R8_UINT` to a host stencil format
  without remapping the components would hand the shader the wrong channel.
- No multisampled refusal: a multisampled depth texture's null view is
  spelled out in its multisampled dimension, in the plane's colour format;
  the end-to-end test shows the host takes it.
- The DPP row index follows the console lane, and a non-`0xF` row mask counts
  as crossing the halves; the bank index is unchanged because a host wave's
  run of console lanes starts at a multiple of 32.
- No layer-side DRS rect: the title's constants and viewport are the rect.
