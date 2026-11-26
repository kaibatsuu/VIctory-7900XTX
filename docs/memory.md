# Where the memory goes (2026-11-26)

Another PC than the one of [findings.md](findings.md): Ryzen 7 9800X3D, 64 GB (61.6 GB
for Windows), Radeon RX 9070 XT 16 GB (driver 32.0.32029.2010), Windows 11 build 26300,
Series X profile, a diagnostics build with `scripts\play.ps1`'s settings
(`VICTORY_THIN=1`). The card's budget was 15.4 GB at the start and 13.0 GB at the end, all
of it in use; 3,803 passes made no texture for lack of room.

Seven runs of 2 to 9 minutes, each ending at 32-34 GB private. Run 5 (9 minutes): 10.7 GB
in the menus, from 11.5 to 28.6 GB in the 17 seconds the world loads, 34.4 GB at the end.
The figures below are of run 5's last memory lines unless said otherwise.

## Private memory is not all RAM

On this driver whatever lies in the card's own memory is charged to the process's private
bytes in full, and shows in its address space as committed write-combined memory, but
takes no RAM. Objects made on their own, before and after (private bytes, working set,
the PC's free RAM):

| Made | Private | Working set | RAM |
|---|---|---|---|
| 16 heaps of 64 MB in the card's memory (the texture pool's blocks) | +1,026 MB | 0 | 0 |
| 64 textures BC7 1024x1024 with mips, committed | +88 MB | 0 | 0 |
| 64 buffers of 4 MB in the card's memory the CPU maps (`mapped_card_buffer`) | +256 MB | 0 | 0 |
| A shader-visible view heap of 2^22 slots (32 bytes a slot) | +128 MB | 0 | 0 |
| 64 buffers of 4 MB in the PC's memory, write-back (`shared_buffer`) | +257 MB | +256 MB | +256 MB |
| 512 command allocators, each with a list closed at once (as a prelude) | +561 MB | +549 MB | +545 MB |

So of run 5's 34.4 GB private about 11 GB is the card's memory: the texture pool's 80
blocks of 64 MB, the textures the host renders to (2.9 GB; likely the odd sizes of 11.5 to
28.75 MB in the map), buffers in the card's memory the CPU maps, and three blocks of 128 MB,
the view heaps. The memory map's "write-combined (buffers shared with the card)" is mostly
that: 18,669 MB committed, 7,160 MB of it in memory.

It still counts against the commit limit: 70.6 GB here (RAM and a page file of 9 GB). About
35 GB of it was in use with the title closed (looked at afterwards), and run 5 ended 3.4 GB
from the limit. A page file large enough for that costs no RAM.

## What the RAM holds

| Kind in the memory map | Committed | In memory | What it is |
|---|---|---|---|
| the title's heaps | 4,818 MB | 2,179 MB | console memory the title asked for |
| views of the title's pages | 8,704 MB | 5,576 MB | console memory, mapped |
| the title's page pool | 6,236 MB | 0 MB | |
| process heap segments | 3,543 MB | 3,524 MB | our own allocations 2,730 MB in 2.38 million blocks; the rest the driver's |
| windows of title memory, 4 MiB | 2,794 MB | 2,758 MB | see Windows below |
| write-combined | 18,669 MB | 7,160 MB | command memory about 4.5 GB, upload buffers (staging 832 MB, list scratch 632 MB, others); the rest the card's |
| other private, 1 MB and more | 4,132 MB | 3,964 MB | host copies of title memory (4,933 MB alive in all); one block of 401 MB of ours made at the start, owner not found |
| other private, under 1 MB | 271 MB | 261 MB | |
| files and mappings at host addresses | 8,447 MB | 4,288 MB | among them the driver's shader cache file and the pipeline library's |

## Command lists: 1.1 MB each, used or not

An allocator with its list takes 1,124 KB of private memory when it is made, 1 MiB of it
write-combined, cut from blocks of 32 MB. About 0.7 MB of that is in RAM at once for a
list left open; all of it for one closed at once (a prelude) or recorded into. Recording
200 copies into a list adds 16 KB, 20,000 copies 17 KB; a reset gives nothing back; a
release gives it all back. (Only copies were recorded; draws with tables and constants were not tried.)

In the runs the 32 MB write-combined blocks go with the host lists alive:

| Run | Host lists alive | Preludes made | Blocks of 32 MB |
|---|---|---|---|
| 4 | 2,048 | 1,552 | 233 (7.4 GB) |
| 1 | 2,142 | 1,674 | 252 (8.0 GB) |
| 3 | 2,151 | 1,760 | 249 (8.0 GB) |
| 2 | 2,182 | 1,782 | 258 (8.2 GB) |
| latewait | 2,207 | 2,073 | 273 (8.7 GB) |
| 5 | 2,232 | 2,049 | 287 (9.1 GB) |

Run 5's 2,232 lists and 2,049 preludes are up to 4,281 allocators (the count of preludes
is of those made, not of those alive), about 4.5 GB of RAM. The rest
of those blocks is the card's memory (buffers it maps for the CPU, small committed
textures and buffers), which the driver cuts from the same 32 MB blocks.

Why there are so many: with `VICTORY_THIN` a recording takes a list of ours from the pool
(`thin.rs` `back_list`) and holds it until the title begins that list again or releases
it, so there are about as many host lists as title lists. A list's prelude used to be made the first time the list needed one (the earlier lazy-prelude switch, since replaced by pooled preludes) and kept as long as the list,
though it is recorded again at every submission (`HostList::prelude`) and has to live only
while that submission is on the card.

The memory line's "private bytes grew N MB while lists were made" (`LISTS[2]`) is not what
lists cost: other threads' allocations meanwhile go into it. It said 7.3 GB for the 831
lists alive in the menus.

## The table of placed views' shapes grows

`Views::shape` (`descriptor.rs`) was meant to hold as many shapes as the title has kinds of
textures and buffers. It held 572 in the menus and 911,564 after 9 minutes, and its bytes
went from 3 MB to 270 MB ("distinct views kept" on the memory line), 22 MB a minute in
play. The 116 MB block in "our own blocks of 32 MB and more" is its list (1,048,576 shapes
of 116 bytes). So some word of a view's description other than the address (words 10 and
11) differs from view to view; which one is not known yet. The table is full at 2^22
shapes (about 1.2 GB); views are then table entries, which have no end either.

Our own allocations as a whole grew from 2,217 to 2,730 MB in the last 7 minutes of run 5,
75 MB a minute.

## Windows

927 windows of title memory outside buffers, 3,872 MB, over 3,572 MB of title memory. Of
that, 1,997 MB lies between the lowest and highest address ever asked for in each window,
and 2,523 MB was ever copied. `VICTORY_WINDOW_KB` (a finer grid) and
`VICTORY_WINDOW_TAILS` were not tried in these runs.

## What to do, by RAM saved

1. **Preludes from a pool**, given back when their submission is done: about 2 GB of RAM
   and commit.
2. **A finer window grid** (`VICTORY_WINDOW_KB=1024`, `VICTORY_WINDOW_TAILS=1`): a run to
   measure it, up to the 1.4-1.9 GB between what the windows hold and what was asked for.
3. **The shapes table**: find the words that differ and leave them out of the shape; 22 MB
   a minute, more in long sessions.
4. **A recording's list given back once the card has run it**, not when the title resets
   its own list: up to about 2 GB more, but the title may submit a closed list again.
5. **A page file for the commit**: no code, no RAM.

## How it was measured

The memory lines of the logs (`copystat.rs`; the map, `VICTORY_MEMORY_MAP`, is on by
default in diagnostics builds), the logs of 2026-11-26 joined line by line since
PowerShell wrapped them. The costs of single objects come from a program of its own that
made them on the same card and compared private bytes, the working set, the PC's free RAM
and a walk of the address space before and after.
