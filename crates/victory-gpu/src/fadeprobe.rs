//! Diagnostic (`VICTORY_FADE_LOG=<file>`): the title's per-entity alpha, as it
//! travels to the card.
//!
//! The fade the props show is the title's own: in `capture-1` (list 96) the
//! ten faded draws differ from the solid ones in one float of their per-draw
//! constant block, whose first twelve floats are
//! `[alpha, 0, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0]` with `alpha = k / 255` for a `k`
//! the title's CPU computed (`TASK-02-findings-round2.md`, "Proven" 1).
//!
//! Nothing is scanned on the submitting thread. Round 3 read back the *upload*
//! memory a fill went to and cost 56 ms a frame (D3D12 upload memory is
//! write-combined: sixteen uncached reads for 48 bytes, 156,000 fills a
//! second). Round 4's first run scanned in this process's memory on the
//! submitting thread as well, and stood at 10 fps. Here a fill, a copy or a
//! refresh of title memory only *names* its range ([`ask`]: a push into a list
//! of 262,144, no lock a caller waits on, no allocation), and one thread of the
//! probe does all the reading and all the writing to the log.
//!
//! The thread takes the whole list every two milliseconds, sorts it in chunks
//! (thousands of fills of a frame's constant ring stand next to one another),
//! reads every run of them from title memory (through the kernel: a range the
//! title unmapped must not fault), and looks at the ranges asked about. It is
//! told what asked, so a `A` line says which way the bytes were travelling.
//!
//! The matcher is the capture's block, `[alpha, 0, 1 × 6, …]` with `alpha` a
//! `k / 255`:
//!
//! - `A` when a block's alpha is strictly between 0 and 1, first seen or
//!   changed (a fade step);
//! - `A` with 0 when a block that was fading is fully hidden;
//! - `R` when a block that faded is solid (1.0) again.
//!
//! Round 4's first run also had a "any `k / 255` float" discovery matcher: it
//! wrote 866,000 lines of colour noise in a hundred seconds and starved the
//! thread that feeds it, which cost the run its fills. It is gone.
//!
//! Lines (`ask` kinds: 0 fill of upload memory on the CPU, 1 buffer copy,
//! 2 copy at its place in a list, 3 fill or copy done on the card, 4 refresh
//! of a host copy, 5 a graphics draw's own constant buffer):
//!
//! - `A <ms> <address> <bits> <alpha> <k> <kind>`
//! - `R <ms> <address> <1.0> <kind>`
//! - `E <ms> <address> <bits> <alpha> <k> <kind>`: `CB0[2].y` of a graphics
//!   draw's constant buffer (kind 5), a `k / 255` between 0 and 1, changed;
//!   `1.0` when a field that was fading is solid again. The address is the
//!   draw's own constant buffer, so it names the entity as well as its alpha.
//! - `C <ms> <ranges> <bytes> <A, R and E lines> <zero-alpha blocks> <ns a
//!   range sampled at> <ranges dropped> <µs the thread was at its work>
//!   <ranges asked about>`: every five seconds. The probe's own cost is here
//!   so it cannot hide again.
//!
//! Off unless `VICTORY_FADE_LOG` names a file; a diagnostics build only.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// `1.0f32`: six of a block's eight leading floats are this.
const ONE: u32 = 0x3f80_0000;

/// The most of one asked range looked at, at every four bytes: a fill of a
/// per-draw constant buffer is a few hundred bytes, the title's ring copies
/// 20 KiB a frame.
const MOST: usize = 16 << 10;

/// The most ranges left for the thread (262,144 of 16 bytes, 4 MB).
const RING: usize = 1 << 18;

struct Probe {
    file: Mutex<std::io::BufWriter<std::fs::File>>,
    /// The alpha last seen at each block (title address), by bits.
    seen: Mutex<HashMap<usize, u32>>,
    /// Blocks that showed an alpha between 0 and 1.
    faded: Mutex<HashSet<usize>>,
    /// Ranges whose bytes the probe was not given ([`ask`]): title address,
    /// length, and what asked.
    asked: Mutex<Vec<(usize, usize, u8)>>,
    /// Ranges looked at, bytes scanned, lines written, zero-alpha blocks, the
    /// probe's own sampled nanoseconds, samples, ranges dropped (the list was
    /// full or taken), microseconds the thread was at its work, and ranges
    /// asked about.
    counts: [AtomicU64; 14],
}

impl Probe {
    fn new(file: std::fs::File) -> Probe {
        Probe {
            file: Mutex::new(std::io::BufWriter::with_capacity(1 << 16, file)),
            seen: Mutex::new(HashMap::new()),
            faded: Mutex::new(HashSet::new()),
            asked: Mutex::new(Vec::with_capacity(RING)),
            counts: [const { AtomicU64::new(0) }; 14],
        }
    }
}

fn probe() -> Option<&'static Probe> {
    static PROBE: OnceLock<Option<Probe>> = OnceLock::new();
    PROBE
        .get_or_init(|| {
            let path = std::env::var_os("VICTORY_FADE_LOG")?;
            let file = std::fs::File::create(path).ok()?;
            start_thread();
            Some(Probe::new(file))
        })
        .as_ref()
}

/// Whether the probe is asked for.
pub(crate) fn on() -> bool {
    probe().is_some()
}

/// Milliseconds since the Unix epoch, as the streaming log's clock.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |t| t.as_millis() as u64)
}

/// A range of title memory the probe should look at, `len` bytes at `title`:
/// `kind` is what asked (the `A` and `R` lines say it back). Nothing is read
/// here: the thread [`start_thread`] starts does that.
pub(crate) fn ask(title: usize, len: usize, kind: u8) {
    let Some(probe) = probe() else {
        return;
    };
    probe.counts[8].fetch_add(1, Ordering::Relaxed);
    probe.counts[9 + (kind as usize).min(4)].fetch_add(1, Ordering::Relaxed);
    let Ok(mut asked) = probe.asked.try_lock() else {
        probe.counts[6].fetch_add(1, Ordering::Relaxed);
        return;
    };
    // Draws of one instance stand one after the other with one constant
    // buffer: the same range again is nothing new.
    if asked.last().is_some_and(|&(t, l, k)| t == title && l == len && k == kind) {
        return;
    }
    if asked.len() >= RING {
        probe.counts[6].fetch_add(1, Ordering::Relaxed);
        return;
    }
    asked.push((title, len, kind));
}

/// The thread that looks at what [`ask`] left: it takes the whole list at
/// once (a swap under the lock, so nobody waits for more than that), sorts it
/// in chunks (the fills of a frame's constant ring stand next to one another,
/// so a run of them is one read), reads every range from title memory through
/// the kernel, and scans it.
fn start_thread() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let _ = std::thread::Builder::new()
            .name("fade probe".into())
            .spawn(|| {
                /// Ranges this close are read as one run.
                const GAP: usize = 4096;
                /// The most of one read.
                const RUN: usize = 64 << 10;
                /// Ranges sorted at once (a sort of the whole list was the
                /// thread's own cost in round 4's first run).
                const CHUNK: usize = 2048;
                let mut taken: Vec<(usize, usize, u8)> = Vec::with_capacity(RING);
                let mut bytes = vec![0u8; RUN];
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    let Some(probe) = probe() else {
                        return;
                    };
                    {
                        let mut asked = probe.asked.lock().unwrap_or_else(|e| e.into_inner());
                        if asked.is_empty() {
                            summary(probe);
                            continue;
                        }
                        std::mem::swap(&mut *asked, &mut taken);
                    }
                    let started = std::time::Instant::now();
                    // A panic in here (a range that goes nowhere, a slice
                    // that does not fit) must not take the probe down: the
                    // rest of the list is looked at, and the `C` line's share
                    // of lost ranges says how often one happened.
                    let looked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        for chunk in taken.chunks_mut(CHUNK) {
                            chunk.sort_unstable();
                            let mut i = 0;
                            while i < chunk.len() {
                                let start = chunk[i].0;
                                let mut end = (start + chunk[i].1).min(start + RUN);
                                let mut j = i + 1;
                                while j < chunk.len() {
                                    let (at, len, _) = chunk[j];
                                    let to = (at + len).min(start + RUN);
                                    // Sorted: nothing later can reach this run.
                                    if at > end + GAP || to <= start {
                                        break;
                                    }
                                    end = end.max(to);
                                    j += 1;
                                }
                                let n = (end - start).min(bytes.len());
                                let got = if crate::host::read_into(start as *const u8, &mut bytes[..n]) {
                                    n
                                } else {
                                    // A page of it went: what is left is read.
                                    crate::host::read_there(start as *const u8, &mut bytes[..n])
                                };
                                if got > 0 {
                                    for &(at, len, kind) in &chunk[i..j] {
                                        let off = at - start;
                                        if off >= got {
                                            continue;
                                        }
                                        let len = len.min(MOST).min(got - off);
                                        copy(probe, at, &bytes[off..off + len], kind);
                                    }
                                }
                                i = j;
                            }
                        }
                    }));
                    if looked.is_err() {
                        probe.counts[6].fetch_add(1, Ordering::Relaxed);
                    }
                    probe.counts[7].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
                    taken.clear();
                    summary(probe);
                    let _ = probe.file.lock().map(|mut f| f.flush());
                }
            });
    });
}

/// `bytes` is title memory at `title`, whose bytes are at hand. Only the
/// probe's thread calls this.
fn copy(probe: &Probe, title: usize, bytes: &[u8], kind: u8) {
    let n = bytes.len().min(MOST);
    let count = probe.counts[0].fetch_add(1, Ordering::Relaxed);
    probe.counts[1].fetch_add(n as u64, Ordering::Relaxed);
    let sample = count.is_multiple_of(512);
    let started = sample.then(std::time::Instant::now);
    let mut zero = 0u64;
    let mut at = 0;
    while at + 48 <= n {
        let w0 = u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
        let w1 = u32::from_le_bytes([bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]]);
        let alpha = f32::from_bits(w0);
        // The tell that survives one moved field: an alpha, then a zero.
        if w1 == 0 && (0.0..=1.0).contains(&alpha) && is_byte_over_255(alpha) && shape(&bytes[at..at + 48]) {
            if alpha >= 1.0 {
                back(probe, title + at, kind);
            } else if alpha > 0.0 {
                note(probe, title + at, alpha, kind);
            } else {
                zero += 1;
                gone(probe, title + at, kind);
            }
        }
        at += 4;
    }
    // A graphics draw's constant buffer (kind 5): the field the alpha-clip and
    // foliage pixel shaders read, `CB0[2].y`, is the ninth float of the block
    // (`PS_DeferredTextured_AlphaClip`, round 1). A constant buffer whose
    // ninth float is a whole byte over 255 is no accident: matrix and scale
    // floats are not n/255.
    if kind == 5 && n >= 40 {
        let bits = u32::from_le_bytes([bytes[36], bytes[37], bytes[38], bytes[39]]);
        let alpha = f32::from_bits(bits);
        match byte_alpha(alpha) {
            Some(k) => field(probe, title + 36, alpha, k, kind),
            None if alpha >= 1.0 => field_back(probe, title + 36, kind),
            None => {}
        }
    }
    probe.counts[3].fetch_add(zero, Ordering::Relaxed);
    if let Some(started) = started {
        probe.counts[4].fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        probe.counts[5].fetch_add(1, Ordering::Relaxed);
    }
}

/// Whether `alpha` is a whole byte over 255 (`k / 255`, 0 and 255 included).
fn is_byte_over_255(alpha: f32) -> bool {
    if !(0.0..=1.0).contains(&alpha) {
        return false;
    }
    let k = alpha * 255.0;
    (k - k.round()).abs() <= 0.01
}

/// Whether 48 bytes are a per-draw constant block: `[alpha, 0, 1 × 6, …]`.
fn shape(b: &[u8]) -> bool {
    (2..8).all(|k| u32::from_le_bytes([b[k * 4], b[k * 4 + 1], b[k * 4 + 2], b[k * 4 + 3]]) == ONE)
}

/// `k` when `alpha` is exactly `k / 255` for a byte `k` strictly between 0 and
/// 255: a fade value.
fn byte_alpha(alpha: f32) -> Option<u32> {
    if !(alpha > 0.0 && alpha < 1.0) || !is_byte_over_255(alpha) {
        return None;
    }
    Some((alpha * 255.0).round() as u32)
}

/// A draw's constant buffer: `CB0[2].y` (the ninth float), an alpha between 0
/// and 1, logged when it changes. The address is the draw's own constant
/// buffer, so it names the entity as well as its alpha.
fn field(probe: &Probe, at: usize, alpha: f32, k: u32, kind: u8) {
    let bits = alpha.to_bits();
    let mut seen = probe.seen.lock().unwrap_or_else(|e| e.into_inner());
    if seen.len() > 1 << 20 {
        seen.clear();
    }
    if seen.insert(at, bits) == Some(bits) {
        return;
    }
    drop(seen);
    let ms = now();
    line(probe, format_args!("E\t{ms}\t{at:x}\t{bits:08x}\t{alpha:.6}\t{k}\t{kind}"));
}

/// The same field solid again: only worth a line if it was fading.
fn field_back(probe: &Probe, at: usize, kind: u8) {
    let mut seen = probe.seen.lock().unwrap_or_else(|e| e.into_inner());
    let Some(bits) = seen.get(&at).copied() else {
        return;
    };
    if byte_alpha(f32::from_bits(bits)).is_none() {
        return;
    }
    seen.remove(&at);
    drop(seen);
    let ms = now();
    line(probe, format_args!("E\t{ms}\t{at:x}\t3f800000\t1.000000\t255\t{kind}"));
}

/// A per-draw block whose alpha is strictly between 0 and 1: a line when the
/// address's alpha changed.
fn note(probe: &Probe, at: usize, alpha: f32, kind: u8) {
    let bits = alpha.to_bits();
    let mut seen = probe.seen.lock().unwrap_or_else(|e| e.into_inner());
    if seen.len() > 1 << 20 {
        seen.clear();
    }
    if seen.insert(at, bits) == Some(bits) {
        return;
    }
    drop(seen);
    if let Ok(mut faded) = probe.faded.lock() {
        faded.insert(at);
    }
    let k = (alpha * 255.0).round() as i64;
    line(probe, format_args!("A\t{}\t{at:x}\t{bits:08x}\t{alpha:.6}\t{k}\t{kind}", now()));
}

/// A block seen solid (1.0) again: only worth a line if it faded first.
fn back(probe: &Probe, at: usize, kind: u8) {
    let mut faded = probe.faded.lock().unwrap_or_else(|e| e.into_inner());
    if !faded.remove(&at) {
        return;
    }
    drop(faded);
    line(probe, format_args!("R\t{}\t{at:x}\t1.000000\t{kind}", now()));
}

/// A block that was fading and is now 0: fully hidden. Only a block that had
/// an alpha between 0 and 1 is worth this line (an address seen at 0 for the
/// first time is one of the thousands of hidden entities a frame).
fn gone(probe: &Probe, at: usize, kind: u8) {
    let mut seen = probe.seen.lock().unwrap_or_else(|e| e.into_inner());
    let last = seen.get(&at).copied().unwrap_or(0);
    if last == 0 || last == ONE {
        return;
    }
    seen.insert(at, 0);
    drop(seen);
    line(probe, format_args!("A\t{}\t{at:x}\t00000000\t0.000000\t0\t{kind}", now()));
}

fn line(probe: &Probe, fields: std::fmt::Arguments) {
    probe.counts[2].fetch_add(1, Ordering::Relaxed);
    let mut file = probe.file.lock().unwrap_or_else(|e| e.into_inner());
    let _ = writeln!(file, "{fields}");
}

fn summary(probe: &Probe) {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let ms = now();
    let last = LAST.load(Ordering::Relaxed);
    if ms < last.saturating_add(5_000) {
        return;
    }
    if LAST.compare_exchange(last, ms, Ordering::Relaxed, Ordering::Relaxed).is_err() {
        return;
    }
    let c = probe.counts.each_ref().map(|n| n.swap(0, Ordering::Relaxed));
    let per = c[4].checked_div(c[5]).unwrap_or(0);
    let mut file = probe.file.lock().unwrap_or_else(|e| e.into_inner());
    let _ = writeln!(
        file,
        "C\t{ms}\t{}\t{}\t{}\t{}\t{per}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        c[0], c[1], c[2], c[3], c[6], c[7], c[8], c[9], c[10], c[11], c[12], c[13]
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_bytes(alpha: f32) -> [u8; 48] {
        let mut bytes = [0u8; 48];
        for (k, word) in [alpha.to_bits(), 0, ONE, ONE, ONE, ONE, ONE, ONE, 0, 0, 0, 0].into_iter().enumerate() {
            bytes[k * 4..k * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn a_per_draw_constant_block_is_recognised() {
        assert!(shape(&block_bytes(124.0 / 255.0)));
        assert!(shape(&block_bytes(1.0)));
    }

    #[test]
    fn an_alpha_on_a_zero_that_is_not_a_block_is_not_a_shape() {
        let mut bytes = block_bytes(124.0 / 255.0);
        bytes[8..12].copy_from_slice(&2.0f32.to_le_bytes());
        assert!(!shape(&bytes));
    }

    #[test]
    fn only_a_byte_over_255_is_a_fade_value() {
        assert!(is_byte_over_255(124.0 / 255.0));
        assert!(!is_byte_over_255(0.5));
        assert!(is_byte_over_255(1.0));
        assert!(is_byte_over_255(0.0));
        assert!(!is_byte_over_255(2.0));
    }
}
