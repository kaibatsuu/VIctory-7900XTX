//! Host-memory admission for speculative texture work and reusable upload
//! storage (`VICTORY_RESIDENCY=1`; off by default, and then nothing here
//! changes what the layer does).
//!
//! Ported from Op-lite/victory-scarlett: codec_d's ab75ac3 ("Back off
//! refused GPU pool growth and defer uploads under low commit") and 18bdf79
//! ("... bound speculative staging"), with the part of their residency module
//! (merge e05d463) those two need. On their 32 GB PC Windows returned
//! E_OUTOFMEMORY for a series of 64 MiB pool blocks and uploads as small as
//! 24,080 bytes, and Rust then aborted on a 786,432-byte allocation; with
//! these the confirmation run had none of either at 34.4 GiB of process
//! commit (their `docs/bug-journal.md`, 2026-11-25).
//!
//! What it does, with the switch on:
//! - An out-of-memory return from a host upload buffer, a block of texture
//!   memory or a host texture ([`allocation_error`]) stops the texture pass
//!   for 500 ms ([`upload_refused`]); the slots it did not get to stay due.
//! - The texture pass that uploads textures picked by index runs only while
//!   Windows has commit to spare ([`admit_texture_pass`]): the bake's before
//!   the lookahead's.
//! - Under host-memory pressure ([`refresh`], [`pressured`]) the pass takes
//!   16 MiB a frame ([`prefetch_limit`]) and the pools keep fewer empty
//!   blocks (`crate::pool::room`).
//!
//! Thresholds. Theirs were tuned on a 32 GB PC. On ours (64 GB, 61.6 GB for
//! Windows, a page file Windows manages, commit limit 80.7 GB) commit is what
//! runs short, not physical memory: about 78 GB committed in play with
//! 11-18 GB of physical memory free, so 2.5-6 GiB of commit left. With
//! theirs (lookahead below 4 GiB of commit, pressure from 6 GiB) the pass
//! would stand still or crawl at 16 MiB a frame for most of play. So the
//! commit ladder keeps their order and proportions (bake floor : lookahead
//! floor : pressure entry : pressure exit = 1 : 2 : 3 : 4, theirs 2, 4, 6,
//! 8 GiB) at a quarter of their bytes: 512 MiB, 1 GiB, 1.5 GiB, 2 GiB. A
//! commit floor guards against a burst of allocations between two samples,
//! whose size is the workload's and not the PC's (one catch-up frame of the
//! pass is 3 x 256 MiB on a 16 GB card, and the pass samples commit every
//! 32 ms). The physical-memory thresholds stay theirs (pressure from 4 GiB
//! free, cleared at 6 GiB): absolute amounts, which a 64 GB PC reaches only
//! when it is short indeed. Their own later tree (scarlett-fox 411a745)
//! lowered the floors too, to 512 and 256 MiB, because 4 GiB starved world
//! textures with 1-4 GiB of steady headroom on 32 GB PCs. [`limited_host_ram`]
//! (32 GiB or less) is false here, so 18bdf79's 32 MiB cap for such PCs does
//! not apply on ours.
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// Commit the texture pass leaves free before a bake ([`admit_texture_pass`]):
/// a bake samples its inputs at once and cannot retry that draw (theirs 2 GiB).
const BAKE_RESERVE: u64 = 512 * MIB;
/// The same for the lookahead, every pass that is no bake's (theirs 4 GiB).
const LOOKAHEAD_RESERVE: u64 = GIB;
/// Host-memory pressure begins at this much commit left or less, or at
/// [`PHYSICAL_ENTER`] of physical memory free or less (theirs 6 GiB).
const COMMIT_ENTER: u64 = 3 * GIB / 2;
/// ... and ends with at least this much commit and [`PHYSICAL_EXIT`] free
/// (theirs 8 GiB).
const COMMIT_EXIT: u64 = 2 * GIB;
const PHYSICAL_ENTER: u64 = 4 * GIB;
const PHYSICAL_EXIT: u64 = 6 * GIB;

// The ladder keeps ab75ac3's order: pressure (16 MiB a frame) comes before
// the lookahead stops, and that before the bake does.
const _: () = assert!(
    BAKE_RESERVE < LOOKAHEAD_RESERVE
        && LOOKAHEAD_RESERVE < COMMIT_ENTER
        && COMMIT_ENTER < COMMIT_EXIT
        && PHYSICAL_ENTER < PHYSICAL_EXIT
);

/// Whether the admission is on (`VICTORY_RESIDENCY=1`; also one of the
/// switches `VICTORY_AB` turns on and off in diagnostics builds). Off, every
/// function here says what lets the layer go on as before: no pressure, the
/// pass admitted, no upload refused.
pub(crate) fn on() -> bool {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_RESIDENCY") {
        return on;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_RESIDENCY").is_some_and(|v| v == "1");
        if on {
            tracing::info!(
                bake_reserve_mib = BAKE_RESERVE >> 20,
                lookahead_reserve_mib = LOOKAHEAD_RESERVE >> 20,
                pressure_commit_mib = format_args!("{}-{}", COMMIT_ENTER >> 20, COMMIT_EXIT >> 20),
                pressure_physical_mib = format_args!("{}-{}", PHYSICAL_ENTER >> 20, PHYSICAL_EXIT >> 20),
                limited_host_ram = limited_host_ram(),
                "host-memory admission of texture uploads and pool blocks on (VICTORY_RESIDENCY=1)"
            );
        }
        on
    })
}

fn memory_status() -> Option<MEMORYSTATUSEX> {
    // SAFETY: the API initializes the fields of a correctly sized structure.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: as above.
    (unsafe { GlobalMemoryStatusEx(&mut status) } != 0).then_some(status)
}

static PRESSURED: AtomicBool = AtomicBool::new(false);

// The texture sweep may run for seconds inside a single submitted frame.
static NEXT_COMMIT_SAMPLE: AtomicU64 = AtomicU64::new(0);
static AVAILABLE_COMMIT: AtomicU64 = AtomicU64::new(u64::MAX);
static RETRY_UPLOAD_AT: AtomicU64 = AtomicU64::new(0);

/// A host allocation failed with `error`: an out-of-memory return refuses
/// the texture pass's uploads for 500 ms ([`upload_refused`]). (Noted with
/// the switch off too, so that a phase of `VICTORY_AB` that turns it on
/// finds it; nothing reads it then.)
pub(crate) fn allocation_error(error: &windows::core::Error) {
    if error.code() == windows::Win32::Foundation::E_OUTOFMEMORY {
        RETRY_UPLOAD_AT.fetch_max(crate::host::coarse_ms().saturating_add(500), Ordering::Relaxed);
    }
}

/// Whether an allocation failed for want of memory in the last 500 ms.
pub(crate) fn upload_refused() -> bool {
    on() && crate::host::coarse_ms() < RETRY_UPLOAD_AT.load(Ordering::Relaxed)
}

fn has_upload_reserve(commit: u64, bake: bool) -> bool {
    commit >= if bake { BAKE_RESERVE } else { LOOKAHEAD_RESERVE }
}

/// Texture passes [`admit_texture_pass`] turned away, since the start: the
/// bake's and the lookahead's.
static DEFERRED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Leave commit for game allocations and submissions already in flight. Direct
/// texture consumers bypass this gate. Bake prefetch gets priority over lookahead.
pub(crate) fn admit_texture_pass(bake: bool) -> bool {
    if !on() {
        return true;
    }
    let now = crate::host::coarse_ms();
    let admitted = now >= RETRY_UPLOAD_AT.load(Ordering::Relaxed) && {
        let next = NEXT_COMMIT_SAMPLE.load(Ordering::Relaxed);
        if now >= next
            && NEXT_COMMIT_SAMPLE
                .compare_exchange(next, now.saturating_add(32), Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            && let Some(status) = memory_status()
        {
            AVAILABLE_COMMIT.store(status.ullAvailPageFile, Ordering::Relaxed);
        }
        has_upload_reserve(AVAILABLE_COMMIT.load(Ordering::Relaxed), bake)
    };
    if !admitted {
        let n = DEFERRED[usize::from(!bake)].fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_power_of_two() {
            tracing::info!(
                bake,
                times = n,
                available_commit_mib = AVAILABLE_COMMIT.load(Ordering::Relaxed) >> 20,
                refused = now < RETRY_UPLOAD_AT.load(Ordering::Relaxed),
                "texture pass deferred: little commit left, or an allocation failed for want of memory (VICTORY_RESIDENCY)"
            );
        }
    }
    admitted
}

/// Whether the PC has 32 GiB of physical memory or less.
pub(crate) fn limited_host_ram() -> bool {
    static LIMITED: OnceLock<bool> = OnceLock::new();
    *LIMITED.get_or_init(|| memory_status().is_some_and(|s| s.ullTotalPhys <= 32 * GIB))
}

fn next_pressure(was: bool, physical: u64, commit: u64) -> bool {
    if was {
        physical < PHYSICAL_EXIT || commit < COMMIT_EXIT
    } else {
        physical <= PHYSICAL_ENTER || commit <= COMMIT_ENTER
    }
}

/// Called by the once-per-second submission sweep. A failed sample keeps the last good state.
pub(crate) fn refresh() {
    if !on() {
        return;
    }
    let Some(status) = memory_status() else { return };
    let (physical, commit) = (status.ullAvailPhys, status.ullAvailPageFile);
    let was = PRESSURED.load(Ordering::Relaxed);
    let pressure = next_pressure(was, physical, commit);
    if pressure != was {
        PRESSURED.store(pressure, Ordering::Relaxed);
        if pressure {
            tracing::warn!(
                pid = std::process::id(),
                frame = crate::frame::current(),
                physical_free_mib = physical >> 20,
                available_commit_mib = commit >> 20,
                texture_staging_mib = crate::texture::live_staging_bytes() >> 20,
                spare_scratch_mib = crate::host::ALIVE[4].load(Ordering::Relaxed) >> 20,
                title_filled_texture_mib = crate::resource::KINDS[1].load(Ordering::Relaxed) >> 20,
                "host memory pressure: limiting texture prefetch and idle upload storage"
            );
        } else {
            tracing::info!(physical_free_mib = physical >> 20, available_commit_mib = commit >> 20, "host memory pressure cleared");
        }
    }
}

/// Whether the host is short of memory ([`refresh`]; never with the switch off).
pub(crate) fn pressured() -> bool {
    on() && PRESSURED.load(Ordering::Relaxed)
}

/// A soft per-frame admission limit. A single texture can cross it. The
/// queue retains unserved slots so they can be retried on later frames.
pub(crate) fn prefetch_limit(bytes: u64, pressure: bool, limited: bool, staging: u64) -> u64 {
    if pressure {
        bytes.min(16 << 20)
    } else if limited && staging >= GIB {
        bytes.min(32 << 20)
    } else {
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bake_and_lookahead_leave_room_for_live_game_allocations() {
        for (commit, bake, lookahead) in [
            (277 << 20, false, false),
            (BAKE_RESERVE - 1, false, false),
            (BAKE_RESERVE, true, false),
            (LOOKAHEAD_RESERVE - 1, true, false),
            (LOOKAHEAD_RESERVE, true, true),
        ] {
            assert_eq!(has_upload_reserve(commit, true), bake);
            assert_eq!(has_upload_reserve(commit, false), lookahead);
        }
    }

    #[test]
    fn either_constraint_enters_pressure_and_both_must_recover() {
        assert!(next_pressure(false, 12 * GIB, COMMIT_ENTER));
        assert!(next_pressure(false, PHYSICAL_ENTER, 16 * GIB));
        assert!(next_pressure(true, PHYSICAL_EXIT - GIB, 9 * GIB));
        assert!(next_pressure(true, 7 * GIB, COMMIT_EXIT - 1));
        assert!(!next_pressure(true, PHYSICAL_EXIT, COMMIT_EXIT));
        assert!(!next_pressure(false, PHYSICAL_ENTER + 1, COMMIT_ENTER + 1));
    }

    /// Our PC in play (11 GiB free, about 2.5 GiB of commit left: 78 of
    /// 80.7 GB) is neither pressured nor deferred.
    #[test]
    fn thresholds_leave_play_on_a_64_gb_pc_alone() {
        let (physical, commit) = (11 * GIB, 5 * GIB / 2);
        assert!(!next_pressure(false, physical, commit));
        assert!(!next_pressure(true, physical, commit));
        assert!(has_upload_reserve(commit, false));
    }

    #[test]
    fn prefetch_respects_existing_budgets_and_resumes_after_recovery() {
        assert_eq!(prefetch_limit(512 << 20, false, true, GIB), 32 << 20);
        assert_eq!(prefetch_limit(512 << 20, false, true, GIB - 1), 512 << 20);
        assert_eq!(prefetch_limit(512 << 20, false, false, 2 * GIB), 512 << 20);
        assert_eq!(prefetch_limit(0, true, false, 0), 0);
        assert_eq!(prefetch_limit(8 << 20, true, false, 0), 8 << 20);
        assert_eq!(prefetch_limit(512 << 20, true, false, 0), 16 << 20);
        assert_eq!(prefetch_limit(192 << 20, false, false, 0), 192 << 20);
    }

    /// Off (the default), nothing is pressured, deferred or refused.
    #[test]
    fn off_changes_nothing() {
        if on() {
            return;
        }
        RETRY_UPLOAD_AT.store(u64::MAX, Ordering::Relaxed);
        assert!(!upload_refused());
        assert!(admit_texture_pass(false));
        assert!(!pressured());
        RETRY_UPLOAD_AT.store(0, Ordering::Relaxed);
    }
}
