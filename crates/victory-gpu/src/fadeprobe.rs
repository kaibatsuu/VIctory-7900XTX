//! Diagnostic (`VICTORY_FADE_LOG=<file>`): the title's per-entity alpha, as it
//! travels to the card.
//!
//! The fade the props show is the title's own: in `capture-1` (list 96) the
//! ten faded draws differ from the solid ones in one float of their per-draw
//! constant block, whose first twelve floats are
//! `[alpha, 0, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0]` with `alpha = k / 255` for a `k`
//! the title's CPU computed (`TASK-02-findings-round2.md`, "Proven" 1). The
//! per-frame blocks were not found in copies into the card
//! (`crate::list`'s `fade_probe` ran and saw none): they are written by the
//! title's CPU into title memory, and reach the card the way every other
//! title write does. This watches the two places such bytes pass through:
//!
//! - the fill of a submission's upload memory from title memory
//!   (`crate::host`, `Step::Fill`): a constant buffer a table names, which is
//!   how the draws' constants are given to the card (6,400-6,600 a frame,
//!   some 480 bytes each);
//! - the refresh of a host copy (`crate::shadow::Shadow`): a constant buffer
//!   a root descriptor names.
//!
//! Only the head of a copy is read ([`HEAD`] bytes, at every four bytes of
//! it): a draw's constants start at its view's address, which is where a fill
//! or a refresh of them starts. Reading whole fills cost 400+ ms a frame in
//! the loading phase of 2026-11-27 19:26 (the clean run of the same build:
//! 19-20 fps, copies 19-22 ms); the `C` line carries the probe's own cost so
//! it cannot hide again.
//!
//! Two matchers, because where the block starts is not settled:
//!
//! - a per-draw block by its shape, `[alpha, 0, 1 × 6]` with `alpha` a
//!   `k / 255` in `[0, 1]`: what capture-1 showed;
//! - any `k / 255` float strictly between 0 and 1, with the twelve floats
//!   around it, logged once a second per distinct twelve-float shape: a value
//!   like that is rare outside a fade (positions, scales and colours are not
//!   n/255, though some colours are, which is why the shapes are told apart).
//!
//! Lines:
//!
//! - `A <ms> <address> <bits> <alpha> <k>`: a block shaped as a per-draw
//!   constant block whose alpha is below 1.0, first seen or changed.
//! - `R <ms> <address> <1.0>`: such a block is solid again.
//! - `F <ms> <address> <off in range> <bits> <alpha> <k> <12 floats hex>`: a
//!   `k / 255` float strictly between 0 and 1, first seen at that address
//!   with that value and in that neighbourhood.
//! - `C <ms> <copies seen> <bytes scanned> <matched> <weak> <ns per copy
//!   sampled>`: every five seconds.
//!
//! The `C` line carries the probe's own cost so it cannot hide again.
//!
//! Off unless `VICTORY_FADE_LOG` names a file; a diagnostics build only.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// `1.0f32`: six of a block's eight leading floats are this.
const ONE: u32 = 0x3f80_0000;

/// The bytes at the start of a copy looked at: a draw's constant block is
/// there (alpha at 0, or at CB0[2].y, byte 36).
const HEAD: usize = 64;

struct Probe {
    file: Mutex<std::io::BufWriter<std::fs::File>>,
    /// The alpha last seen at each block (title address), by bits.
    seen: Mutex<HashMap<usize, u32>>,
    /// Blocks that showed an alpha below 1: a solid block is checked against
    /// this with a `try_read` (a lock a fill must not wait on; the cost of
    /// locking per fill was 3 µs a copy and 7 ms a frame at 156,000 fills a
    /// second).
    faded: std::sync::RwLock<std::collections::HashSet<usize>>,
    /// Each twelve-float shape logged, and how many lines it has had.
    shapes: Mutex<HashMap<[u32; 12], u32>>,
    /// Copies looked at, bytes scanned, blocks matched, weak matches, the
    /// probe's own sampled nanoseconds, and how many samples.
    counts: [AtomicU64; 6],
}

impl Probe {
    fn new(file: std::fs::File) -> Probe {
        Probe {
            file: Mutex::new(std::io::BufWriter::with_capacity(1 << 16, file)),
            seen: Mutex::new(HashMap::new()),
            faded: std::sync::RwLock::new(std::collections::HashSet::new()),
            shapes: Mutex::new(HashMap::new()),
            counts: [const { AtomicU64::new(0) }; 6],
        }
    }
}

fn probe() -> Option<&'static Probe> {
    static PROBE: OnceLock<Option<Probe>> = OnceLock::new();
    PROBE
        .get_or_init(|| {
            let path = std::env::var_os("VICTORY_FADE_LOG")?;
            let file = std::fs::File::create(path).ok()?;
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

/// What 48 bytes at some offset of a copy are.
enum Match {
    /// A per-draw constant block: its alpha.
    Block(f32),
    /// An alpha on a zero, but not the six 1.0s after it: the block's shape
    /// is not what capture-1 showed, and the probe may be looking for the
    /// wrong thing.
    Weak,
    No,
}

/// `bytes` is title memory at `title`, copied to the card just now.
pub(crate) fn copy(title: usize, bytes: &[u8]) {
    let Some(probe) = probe() else {
        return;
    };
    let n = bytes.len().min(HEAD);
    let count = probe.counts[0].fetch_add(1, Ordering::Relaxed);
    probe.counts[1].fetch_add(n as u64, Ordering::Relaxed);
    // The head at every four bytes: a block can start at any of them, and
    // this is cheap once the solid blocks no longer take a lock (`faded`).
    let mut weak = 0u64;
    let sample = count.is_multiple_of(256);
    let started = sample.then(std::time::Instant::now);
    let mut at = 0;
    while at + 4 <= n {
        let word = u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
        if let Some(k) = byte_alpha(f32::from_bits(word)) {
            // A colour is three or four equal components: `249/255` three
            // times then 1.0 is everywhere in the loading phase, and taking a
            // lock for each of 500,000 a second cost 45 ms a frame. A fade's
            // alpha is followed by something else (0, usually).
            let repeated = at + 8 <= n && u32::from_le_bytes([bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]]) == word;
            if k > 0 && !repeated {
                if let Some(floats) = floats(bytes, at) {
                    fresh(probe, title + at, at, f32::from_bits(word), k, floats);
                }
            }
        }
        if at + 48 <= n {
            match block(&bytes[at..at + 48]) {
                Match::Block(alpha) => note(probe, title + at, alpha),
                Match::Weak => weak += 1,
                Match::No => {}
            }
        }
        at += 4;
    }
    probe.counts[3].fetch_add(weak, Ordering::Relaxed);
    if let Some(started) = started {
        probe.counts[4].fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        probe.counts[5].fetch_add(1, Ordering::Relaxed);
    }
    if count.is_multiple_of(1024) {
        summary(probe);
    }
}

/// `k` when `alpha` is exactly `k / 255` for a byte `k`; 0 and 255 are not a
/// fade.
fn byte_alpha(alpha: f32) -> Option<u32> {
    if !(alpha > 0.0 && alpha < 1.0) {
        return None;
    }
    let k = alpha * 255.0;
    let r = k.round();
    ((k - r).abs() <= 0.01).then_some(r as u32)
}

/// The twelve floats at `at` of `bytes`, as words, when the range holds them.
fn floats(bytes: &[u8], at: usize) -> Option<[u32; 12]> {
    if at + 48 > bytes.len() {
        return None;
    }
    let mut out = [0u32; 12];
    for (k, word) in out.iter_mut().enumerate() {
        let p = at + k * 4;
        *word = u32::from_le_bytes([bytes[p], bytes[p + 1], bytes[p + 2], bytes[p + 3]]);
    }
    Some(out)
}

/// What the 48 bytes `b` are: a per-draw constant block if they start
/// `[alpha, 0, 1, 1, 1, 1, 1, 1, …]` with `alpha` a byte's worth of 255ths.
fn block(b: &[u8]) -> Match {
    let word = |k: usize| u32::from_le_bytes([b[k * 4], b[k * 4 + 1], b[k * 4 + 2], b[k * 4 + 3]]);
    // The tell that survives one moved field: an alpha, then a zero.
    if word(1) != 0 {
        return Match::No;
    }
    let alpha = f32::from_bits(word(0));
    if !(0.0..=1.0).contains(&alpha) {
        return Match::No;
    }
    let k = alpha * 255.0;
    if (k - k.round()).abs() > 0.01 {
        return Match::No;
    }
    if word(2) != ONE {
        return Match::Weak;
    }
    for j in 3..8 {
        if word(j) != ONE {
            return Match::Weak;
        }
    }
    Match::Block(alpha)
}

/// A `k / 255` float strictly between 0 and 1: its first sighting at an
/// address in a given twelve-float neighbourhood.
fn fresh(probe: &Probe, at: usize, offset: usize, alpha: f32, k: u32, floats: [u32; 12]) {
    let bits = alpha.to_bits();
    let mut seen = probe.seen.lock().unwrap_or_else(|e| e.into_inner());
    if seen.len() > 1 << 20 {
        seen.clear();
    }
    if seen.insert(at, bits) == Some(bits) {
        return;
    }
    drop(seen);
    // A shape gets at most a few lines: colours (249/255, 194/255) are
    // everywhere, a fade's changing alpha is its own shape every step.
    let mut shapes = probe.shapes.lock().unwrap_or_else(|e| e.into_inner());
    if shapes.len() > 1 << 16 {
        shapes.clear();
    }
    let n = shapes.entry(floats).or_insert(0);
    if *n >= 8 {
        return;
    }
    *n += 1;
    drop(shapes);
    let dwords: Vec<String> = floats.iter().map(|w| format!("{w:08x}")).collect();
    let ms = now();
    probe.counts[2].fetch_add(1, Ordering::Relaxed);
    let mut file = probe.file.lock().unwrap_or_else(|e| e.into_inner());
    let _ = writeln!(file, "F\t{ms}\t{at:x}\t{offset}\t{bits:08x}\t{alpha:.6}\t{k}\t{}", dwords.join(","));
}

fn note(probe: &Probe, at: usize, alpha: f32) {
    let bits = alpha.to_bits();
    if alpha >= 1.0 {
        // A solid block is what every draw has; only a return from a fade is
        // worth a line. The check must not wait on a lock a fill holds (that
        // cost 3 µs a copy): a look that misses the lock skips this block.
        let Ok(faded) = probe.faded.try_read() else {
            return;
        };
        if !faded.contains(&at) {
            return;
        }
        drop(faded);
        if let Ok(mut faded) = probe.faded.write() {
            faded.remove(&at);
        }
        let mut file = probe.file.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(file, "R\t{}\t{at:x}\t1.000000", now());
        return;
    }
    let mut seen = probe.seen.lock().unwrap_or_else(|e| e.into_inner());
    if seen.len() > 1 << 20 {
        seen.clear();
    }
    if seen.insert(at, bits) == Some(bits) {
        return;
    }
    drop(seen);
    if let Ok(mut faded) = probe.faded.write() {
        faded.insert(at);
    }
    let k = (alpha * 255.0).round() as i64;
    let mut file = probe.file.lock().unwrap_or_else(|e| e.into_inner());
    let _ = writeln!(file, "A\t{}\t{at:x}\t{bits:08x}\t{alpha:.6}\t{k}", now());
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
    let _ = writeln!(file, "C\t{ms}\t{}\t{}\t{}\t{}\t{per}", c[0], c[1], c[2], c[3]);
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
        assert!(matches!(block(&block_bytes(124.0 / 255.0)), Match::Block(a) if a == 124.0 / 255.0));
        assert!(matches!(block(&block_bytes(1.0)), Match::Block(a) if a == 1.0));
    }

    #[test]
    fn an_alpha_on_a_zero_that_is_not_a_block_is_weak() {
        let mut bytes = block_bytes(124.0 / 255.0);
        bytes[8..12].copy_from_slice(&2.0f32.to_le_bytes());
        assert!(matches!(block(&bytes), Match::Weak));
    }

    #[test]
    fn only_a_byte_over_255_is_a_fade_value() {
        assert_eq!(byte_alpha(124.0 / 255.0), Some(124));
        assert_eq!(byte_alpha(0.5), None);
        assert_eq!(byte_alpha(1.0), None);
        assert_eq!(byte_alpha(0.0), None);
        assert_eq!(byte_alpha(2.0), None);
    }
}
