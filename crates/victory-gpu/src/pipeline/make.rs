//! Host pipelines are made by workers, not by the title thread that first
//! records a draw with them. Translating a shader and the host driver's
//! compile take from milliseconds to seconds each (structurisation of the
//! biggest compute programs ~0.4 s, some driver compiles 3.4 s), and
//! the title's first world frame wants over 1,200 new pipelines: made
//! on its recording threads, that frame took more than 30 s and the
//! title's GPU watchdog ended the run (decision 0007, "Pipelines made off
//! the title's threads").
//!
//! A first use queues the pipeline and waits for it within a budget per
//! frame, so the cheap ones (menus, one-off passes such as a colour-grading
//! bake) are there at once as before; past the budget a draw or dispatch
//! whose pipeline is not ready is dropped, as an untranslated one is.
//!
//! The default budget is half a second per title frame. A pass the title
//! runs only once and whose pipeline takes longer is lost. Passes over the
//! screen (no vertex input, no depth) have workers and a longer budget of
//! their own ([`screen_budget`]): the title fills its lighting tables with
//! such draws once, at the world's first frame, and behind a world's worth of
//! pipelines they were dropped; that run had no lamps and no diffuse light to
//! its end (run textures-d2, the first after a new translation; Belobug
//! 18e57db8 found the same).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::canary::{self, Ended as CanaryEnd};
use super::host::{GraphicsState, HostPipeline, PreparedPipeline};
use victory_shader::emit::SpecConstants;

/// What a host pipeline is made from, and the host pipeline once made.
pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Option<Box<GraphicsState>>,
    queue_state: AtomicU8,
    /// Taken by a worker from [`Queue::slow`].
    was_slow: AtomicBool,
    made: OnceLock<Option<HostPipeline>>,
    /// What identifies it across runs (shader containers and state), and
    /// whether a draw or dispatch has requested it yet.
    key: String,
    used: AtomicBool,
    /// A pass over the screen ([`GraphicsState::screen_pass`]), rectangles and
    /// quads included.
    screen: bool,
    /// A compute dispatch.
    compute: bool,
    /// A critical pass the title waits for: over the screen, or a decoration
    /// bake ([`crate::draw::BAKE_PIXEL_SHADERS`]).
    critical: bool,
    /// Specialization constants patched into the shader when the host
    /// pipeline is made (a later step parameterizes near-duplicate shaders
    /// with them; empty: the shader's defaults).
    spec_constants: SpecConstants,
}

// SAFETY: D3D12 objects are free-threaded; the graphics state (whose input
// elements point into its own names) is only read after creation.
unsafe impl Send for Pending {}
// SAFETY: as above.
unsafe impl Sync for Pending {}

impl Pending {
    #[allow(dead_code)]
    pub(crate) fn new(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let key = pending_key(&shaders, graphics.as_deref());
        Pending::with_key(key, shaders, graphics)
    }

    fn with_key(key: String, shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let screen = graphics.as_ref().is_some_and(|g| g.screen_pass());
        let compute = matches!(shaders.as_slice(), [cs] if cs.stage == "compute");
        let critical = screen
            || shaders
                .iter()
                .any(|s| s.stage == "pixel" && crate::draw::BAKE_PIXEL_SHADERS.contains(&s.hash.as_str()));
        Pending {
            screen,
            compute,
            critical,
            shaders,
            graphics,
            queue_state: AtomicU8::new(NOT_QUEUED),
            was_slow: AtomicBool::new(false),
            made: OnceLock::new(),
            key,
            used: AtomicBool::new(false),
            spec_constants: SpecConstants::new(),
        }
    }

    /// The one live pipeline for `shaders` + `graphics`, or a new one: the
    /// boolean is whether it was made now, so the caller notes and earlies it
    /// once (the title creates ~84,000 while loading; most repeat).
    pub(crate) fn get_or_create(
        shaders: Vec<Arc<Shader>>,
        graphics: Option<Box<GraphicsState>>,
    ) -> (Arc<Pending>, bool) {
        let key = pending_key(&shaders, graphics.as_deref());
        let mut map = PENDING_BY_KEY.lock().unwrap_or_else(|e| e.into_inner());
        let map = map.get_or_insert_with(HashMap::new);
        if let Some(existing) = map.get(&key).and_then(std::sync::Weak::upgrade) {
            return (existing, false);
        }
        let pending = Arc::new(Pending::with_key(key.clone(), shaders, graphics));
        map.insert(key, Arc::downgrade(&pending));
        (pending, true)
    }

    pub(super) fn key(&self) -> &str {
        &self.key
    }


    fn make(&self) {
        self.made.get_or_init(|| {
            let _building = crate::timeline::span("pipeline: build a host pipeline");
            let began = Instant::now();
            let made = if isolate() {
                isolated_make(&self.key, &self.shaders, self.graphics.as_deref(), &self.spec_constants)
            } else if let Some((hash, mnemonic)) = fallback_for(&self.shaders) {
                // A shader the bisect found crashing this driver: made with
                // its cached fallback here too, not only behind a canary.
                tracing::info!(shaders = ?self.shaders, %mnemonic, "host pipeline made with the cached fallback (not isolated)");
                HostPipeline::new_with(
                    &self.shaders,
                    self.graphics.as_deref(),
                    &self.spec_constants,
                    Some((hash.as_str(), mnemonic)),
                )
            } else {
                HostPipeline::new(&self.shaders, self.graphics.as_deref(), &self.spec_constants)
            };
            super::library::note_time(&self.key, began.elapsed().as_millis() as u64);
            made
        });
        let (lock, done) = done_of(self);
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        done.notify_all();
    }
}

/// The key a pipeline's shaders and state give it (the same across runs).
fn pending_key(shaders: &[Arc<Shader>], graphics: Option<&GraphicsState>) -> String {
    let mut key = blake3::Hasher::new();
    for s in shaders {
        key.update(s.stage.as_bytes());
        key.update(s.hash.as_bytes());
    }
    if let Some(g) = graphics {
        g.hash_describe(&mut key);
    }
    key.finalize().to_hex()[..32].to_owned()
}

/// Live pipelines by key: a title that creates the same pipeline twice shares
/// one here, so `known`/`early`/builder work runs once per unique pipeline.
static PENDING_BY_KEY: Mutex<Option<HashMap<String, std::sync::Weak<Pending>>>> = Mutex::new(None);

impl Drop for Pending {
    fn drop(&mut self) {
        let mut map = PENDING_BY_KEY.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(map) = map.as_mut() {
            // A newer pipeline of the same key may have taken the entry.
            if map.get(&self.key).is_some_and(|weak| weak.strong_count() == 0) {
                map.remove(&self.key);
            }
        }
    }
}

/// `VICTORY_PIPELINE_ISOLATE=1`: a driver crash while a host pipeline is
/// compiled ends a child process, not the run. The child is a canary: it
/// makes the same pipeline, and the run makes its own only when the child
/// reports that the compile does not crash (the driver's crash is
/// deterministic, so a child that survived says this compile is safe).
fn isolate() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_ISOLATE").is_some_and(|v| v == "1"))
}

/// Makes the host pipeline through a child process ([`isolate`]); `None` if
/// the child died. A pipeline already in the host library is loaded without a
/// child, and a pipeline [`proven`] (or proven with its fallback) before is
/// compiled without one.
fn isolated_make(
    key: &str,
    shaders: &[Arc<Shader>],
    graphics: Option<&GraphicsState>,
    spec_constants: &SpecConstants,
) -> Option<HostPipeline> {
    // (The key does not cover specialization constants: only the plain
    // pipeline is taken as proven.)
    let plain = spec_constants.is_empty();
    // Resolve the driver-crash fallback first, as the non-isolated path does:
    // a shader the bisect found crashing this driver is made with its cached
    // fallback directly.
    let fallback = fallback_for(shaders);
    let prepared = PreparedPipeline::prepare(shaders, graphics, spec_constants, fallback.as_ref().map(|(h, m)| (h.as_str(), *m)))?;
    // A pipeline already in the host library was already compiled without a
    // crash, so it needs no canary: load it in-process and return.
    if !isolate_library_load()
        && let Some(pso) = prepared.try_load()
    {
        return Some(pso);
    }
    // Not in the library: proven pipelines compile in-process; the rest are
    // made by a canary child first.
    let fallback_tag = fallback_tag(&fallback);
    let fallback_key = fallback.as_ref().map(|(h, m)| (key.to_owned(), h.clone(), String::from(*m)));
    if plain && proven(key) {
        PROVEN_SKIPPED.fetch_add(1, Ordering::Relaxed);
        return prepared.compile();
    }
    if let Some(fk) = &fallback_key {
        if proven_fallback(fk) {
            return prepared.compile();
        }
    }
    // A hard driver crash of this pipeline (with this fallback) was recorded
    // before: do not canary it again.
    if bad_before(key, &fallback_tag) {
        return None;
    }
    let bytes = super::known::pending(shaders, graphics, spec_constants);
    match canary::make(&bytes, fallback) {
        CanaryEnd::Made | CanaryEnd::Refused => {
            if plain {
                note_proven(key);
            } else if let Some(fk) = &fallback_key {
                note_proven_fallback(fk);
            }
            prepared.compile()
        }
        CanaryEnd::Died(code) => {
            if let Some(code) = hard_crash(code) {
                note_bad(key, &fallback_tag, code);
            }
            tracing::warn!(
                ?shaders,
                "no host pipeline: the driver crashed compiling it (isolated in a child process)"
            );
            None
        }
        CanaryEnd::DeviceRemoved => {
            // The canary pool retries a removed device on a fresh worker and
            // never returns this; treat it as a death if it ever did.
            None
        }
    }
}

/// Pipelines made without a child because they were [`proven`], since the
/// start (for the log's canary line).
static PROVEN_SKIPPED: AtomicU64 = AtomicU64::new(0);

/// `VICTORY_ISOLATE_LIBRARY_LOAD=1`: a library hit still goes through a canary
/// (as before the fast path).
fn isolate_library_load() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_ISOLATE_LIBRARY_LOAD").is_some_and(|v| v == "1"))
}

/// `warm-done.txt` in the pipeline cache folder: `<fingerprint> <key>` for
/// each pipeline made in a child process without a crash, by `victory warm`
/// or by a run's canary, with the translator and driver of
/// [`super::known::fingerprint`].
fn proven_file() -> Option<std::path::PathBuf> {
    super::library::folder().map(|dir| dir.join("warm-done.txt"))
}

/// `canary-bad.txt` in the pipeline cache folder: `<fingerprint> <key>
/// <fallback-tag> <exception-code>` for each pipeline a canary child died on
/// with a hard driver crash, so it is not canaried again. Kept apart from
/// `warm-bad.txt` (`<key> <why>`), which `victory warm` writes.
fn bad_file() -> Option<std::path::PathBuf> {
    super::library::folder().map(|dir| dir.join("canary-bad.txt"))
}

/// This run's translator and driver, as `warm-done.txt` names them.
fn fingerprint() -> &'static str {
    static FINGERPRINT: OnceLock<String> = OnceLock::new();
    FINGERPRINT.get_or_init(super::known::fingerprint)
}

/// Pipelines proven in a child before (loaded from `warm-done.txt`, and added
/// to as canaries make theirs): `RwLock` so [`note_proven`] updates it in
/// place and later pipelines of this run skip the canary too.
static PROVEN: RwLock<Option<HashSet<String>>> = RwLock::new(None);

/// Whether `key` was made in a child process before, with this translator
/// and driver: its compile does not crash (the driver's crash is
/// deterministic), so it needs no canary. `victory warm` ahead of a run, and
/// each canary that made its pipeline, add to the list; a new translator or
/// driver starts it again.
fn proven(key: &str) -> bool {
    let read = PROVEN.read().unwrap_or_else(|e| e.into_inner());
    if let Some(set) = read.as_ref() {
        return set.contains(key);
    }
    drop(read);
    let mut write = PROVEN.write().unwrap_or_else(|e| e.into_inner());
    let set = write.get_or_insert_with(|| {
        let of = fingerprint();
        let text = proven_file().and_then(|f| std::fs::read_to_string(f).ok()).unwrap_or_default();
        let set: HashSet<String> = text
            .lines()
            .filter_map(|l| l.split_once(' '))
            .filter(|(f, _)| *f == of)
            .map(|(_, k)| k.trim().to_owned())
            .collect();
        tracing::info!(pipelines = set.len(), fingerprint = of, "pipelines proven in a child before: made without a canary");
        set
    });
    set.contains(key)
}

/// Adds `key` to [`proven_file`] for later runs, and to this run's in-memory
/// set, immediately. The file is kept open (and flushed per line) so a
/// `TerminateProcess` cannot lose what was written.
fn note_proven(key: &str) {
    use std::io::Write;
    static FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);
    if let Some(set) = PROVEN.write().unwrap_or_else(|e| e.into_inner()).as_mut() {
        set.insert(key.to_owned());
    }
    let Some(path) = proven_file() else { return };
    let mut file = FILE.lock().unwrap_or_else(|e| e.into_inner());
    if file.is_none() {
        *file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok();
    }
    if let Some(f) = file.as_mut() {
        let _ = writeln!(f, "{} {key}", fingerprint());
        let _ = f.flush();
    }
}

/// Pipelines proven with a driver-crash fallback in a child, this run only:
/// `(key, shader hash, mnemonic)`.
static PROVEN_FALLBACK: RwLock<Option<HashSet<(String, String, String)>>> = RwLock::new(None);

fn proven_fallback(key: &(String, String, String)) -> bool {
    PROVEN_FALLBACK
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(key))
}

fn note_proven_fallback(key: &(String, String, String)) {
    PROVEN_FALLBACK
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashSet::new)
        .insert(key.clone());
}

/// How a fallback is named in `canary-bad.txt`: `-` for none, `<hash>:<mnemonic>`
/// for a driver-crash fallback. A normal crash (`-`) never blocks a fallback
/// compile: the entry is matched with the fallback tag too.
fn fallback_tag(fallback: &Option<(String, &'static str)>) -> String {
    match fallback {
        Some((hash, mnemonic)) => format!("{hash}:{mnemonic}"),
        None => "-".to_owned(),
    }
}

/// Whether this pipeline (with `fallback_tag`) hard-crashed the driver in a
/// canary before, this translator and driver.
fn bad_before(key: &str, fallback_tag: &str) -> bool {
    let Some(path) = bad_file() else { return false };
    let Ok(text) = std::fs::read_to_string(path) else { return false };
    let of = fingerprint();
    text.lines()
        .filter_map(|l| {
            let mut words = l.split(' ');
            let (f, k, tag) = (words.next()?, words.next()?, words.next()?);
            (f == of && k == key && tag == fallback_tag).then_some(())
        })
        .next()
        .is_some()
}

/// Records a hard driver crash (`code`) of this pipeline in `canary-bad.txt`.
fn note_bad(key: &str, fallback_tag: &str, code: i32) {
    use std::io::Write;
    static FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);
    let Some(path) = bad_file() else { return };
    let mut file = FILE.lock().unwrap_or_else(|e| e.into_inner());
    if file.is_none() {
        *file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok();
    }
    if let Some(f) = file.as_mut() {
        let _ = writeln!(f, "{} {key} {fallback_tag} {code:#010x}", fingerprint());
        let _ = f.flush();
    }
}

/// A child's exit code, when it is a hard driver crash (an NT exception code).
/// A slow timeout is not one (the watchdog ends it with its own code), so it
/// is not blacklisted.
fn hard_crash(code: Option<i32>) -> Option<i32> {
    code.filter(|c| (*c as u32) & 0xC000_0000 == 0xC000_0000)
}

/// The fallback cache written by the translate path's bisect, loaded once:
/// one `<shader-hash> <driver-version> <mnemonic>` per line, keyed by
/// `(hash, driver)` so a driver update re-checks the fallback.
fn fallbacks() -> &'static HashMap<(String, i64), String> {
    static FALLBACKS: OnceLock<HashMap<(String, i64), String>> = OnceLock::new();
    FALLBACKS.get_or_init(|| {
        let mut map = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(fallbacks_file()) {
            for line in text.lines() {
                let mut words = line.split(' ');
                let (Some(hash), Some(driver), Some(mnemonic)) =
                    (words.next(), words.next(), words.next())
                else {
                    continue;
                };
                let Ok(driver) = driver.parse::<i64>() else {
                    continue;
                };
                map.insert((hash.to_owned(), driver), mnemonic.to_owned());
            }
        }
        map
    })
}

/// The cache folder's `pipeline-fallbacks.txt`, the same file the translate
/// path's bisect writes (`VICTORY_CACHE_DIR`, else the platform default).
fn fallbacks_file() -> std::path::PathBuf {
    let dir = std::env::var_os("VICTORY_CACHE_DIR")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("LOCALAPPDATA")
                .or_else(|| std::env::var_os("XDG_CACHE_HOME"))
                .map(std::path::PathBuf::from)
                .map(|base| base.join("victory").join("cache"))
        })
        .unwrap_or_else(std::env::temp_dir);
    dir.join("pipeline-fallbacks.txt")
}

/// The mnemonic cached for any of `shaders`, for this driver.
/// The fallback for any of `shaders`, for this driver: `(shader hash,
/// mnemonic)`.
fn fallback_for(shaders: &[Arc<Shader>]) -> Option<(String, &'static str)> {
    let driver = crate::host::host().map(|h| h.driver()).unwrap_or(0);
    shaders.iter().find_map(|s| {
        let key = (s.hash.clone(), driver);
        fallbacks().get(&key).map(|m| (s.hash.clone(), m.as_str()))
    })
}

/// Canary children live in [`super::canary`]: a pool of persistent
/// `victory canary-worker` processes, each making many pipelines, framed over
/// their pipes. See that module for the pool, the watchdog and the
/// `VICTORY_CANARY_*` switches.

/// Why a pipeline has no host pipeline (diagnostics): its shaders that do
/// not translate, else "refused" or "being made"; `None` if it has one.
#[cfg(feature = "diagnostics")]
pub(crate) fn missing(p: &Pending) -> Option<String> {
    match p.made.get() {
        Some(Some(_)) => None,
        None => Some("being made".into()),
        Some(None) => {
            let failed: Vec<String> = p
                .shaders
                .iter()
                .filter(|s| matches!(s.translated.get(), Some(None)))
                .map(|s| format!("{} {}", s.stage, s.hash))
                .collect();
            Some(if failed.is_empty() {
                format!("refused {:?}", p.shaders)
            } else {
                failed.join(" + ")
            })
        }
    }
}

/// The hashes of the shaders `p` is made of.
pub(crate) fn hashes(p: &Pending) -> String {
    let hashes: Vec<&str> = p.shaders.iter().map(|s| s.hash.as_str()).collect();
    hashes.join("+")
}

/// Signalled whenever a worker has made a pipeline, striped over 32 buckets
/// (indexed by the `Pending`'s address) so recording threads do not contend
/// on one condvar.
static DONE: [(Mutex<()>, Condvar); 32] = [const { (Mutex::new(()), Condvar::new()) }; 32];

/// The stripe of [`DONE`] a pipeline's waiters are signalled on.
fn done_of(p: &Pending) -> &'static (Mutex<()>, Condvar) {
    &DONE[(p as *const Pending as usize >> 6) & 31]
}

/// Waiting allowed per title frame, over all threads.
fn wait_budget() -> Duration {
    static BUDGET: OnceLock<Duration> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let millis = std::env::var("VICTORY_PIPELINE_WAIT_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(500);
        tracing::info!(millis, "host pipeline wait budget per frame");
        Duration::from_millis(millis)
    })
}

/// As [`wait_budget`], for passes over the screen (`VICTORY_SCREEN_WAIT_MS`,
/// 10 s; 0: like the others). Below the title's 30 s watchdog.
fn screen_budget() -> Duration {
    static BUDGET: OnceLock<Duration> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let millis = std::env::var("VICTORY_SCREEN_WAIT_MS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(10_000);
        Duration::from_millis(millis)
    })
}

/// Whether passes over the screen go before the others.
fn screens_first() -> bool {
    !screen_budget().is_zero()
}

/// The wait budget for regular (non-critical, non-compute) draws once the
/// world's grace window has passed (`VICTORY_WORLD_PIPELINE_WAIT_MS`, 8 ms):
/// cached hits are made in-frame, cold misses drop for a frame or two.
fn world_pipeline_wait() -> Duration {
    static D: OnceLock<Duration> = OnceLock::new();
    *D.get_or_init(|| {
        let ms = std::env::var("VICTORY_WORLD_PIPELINE_WAIT_MS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(8);
        Duration::from_millis(ms.max(1))
    })
}

/// The wait budget for compute dispatches once in-world
/// (`VICTORY_WORLD_COMPUTE_WAIT_MS`, 150 ms): protects one-shot and indirect
/// dispatches while cutting worst-case stalls.
fn world_compute_wait() -> Duration {
    static D: OnceLock<Duration> = OnceLock::new();
    *D.get_or_init(|| {
        let ms = std::env::var("VICTORY_WORLD_COMPUTE_WAIT_MS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(150);
        Duration::from_millis(ms.max(1))
    })
}

/// Frames after world entry the full loading budget is kept
/// (`VICTORY_WORLD_GRACE_FRAMES`, 60): the world's one-shot lighting, sky, GI
/// and compute passes finish cleanly before the in-world budgets apply.
fn world_grace_frames() -> u64 {
    static N: OnceLock<u64> = OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_WORLD_GRACE_FRAMES")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(60)
    })
}

/// The frame the title last entered the world (`u64::MAX`: loading).
static WORLD_ENTERED_FRAME: AtomicU64 = AtomicU64::new(u64::MAX);

/// Whether the in-world budgets apply: past the world's grace window.
fn in_world_steady() -> bool {
    if LOADING_BOOST.load(Ordering::Relaxed) {
        return false;
    }
    let entered = WORLD_ENTERED_FRAME.load(Ordering::Relaxed);
    entered != u64::MAX && crate::frame::current().saturating_sub(entered) >= world_grace_frames()
}

/// Pipelines queued and not made yet.
static WAITING: AtomicU64 = AtomicU64::new(0);
/// Pipelines queued in the current warming batch, and those completed so far.
static WARM_TOTAL: AtomicU64 = AtomicU64::new(0);
static WARM_DONE: AtomicU64 = AtomicU64::new(0);
/// Whether the title appears to be in a menu or loading screen (<500 draws/frame).
static LOADING_BOOST: AtomicBool = AtomicBool::new(true);

/// Pipelines queued and not made yet (for the frame line).
pub(crate) fn waiting() -> u64 {
    WAITING.load(Ordering::Relaxed)
}

/// Progress of the current warming batch: `(done, total, waiting)`.
pub(crate) fn progress() -> (u64, u64, u64) {
    progress_of(
        WARM_DONE.load(Ordering::Relaxed),
        WARM_TOTAL.load(Ordering::Relaxed),
        WAITING.load(Ordering::Relaxed),
    )
}

fn progress_of(done: u64, total: u64, waiting: u64) -> (u64, u64, u64) {
    (done, total.max(done + waiting), waiting)
}

/// Whether pipeline workers may boost to normal priority while loading
/// (`VICTORY_PIPELINE_BOOST`: on by default, `=0` disables).
fn boost_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_BOOST").is_none_or(|v| v != "0"))
}

/// Updates whether the title is currently in menus or loading screens.
pub(crate) fn set_loading_boost(loading: bool) {
    let was = LOADING_BOOST.swap(loading, Ordering::Relaxed);
    if was && !loading {
        // The title entered the world: begin the grace window that keeps the
        // full loading budget for the world's one-shot passes.
        WORLD_ENTERED_FRAME.store(crate::frame::current(), Ordering::Relaxed);
    } else if !was && loading {
        // Back to a menu or loading screen.
        WORLD_ENTERED_FRAME.store(u64::MAX, Ordering::Relaxed);
    }
}

/// Desired Win32 thread priority for a pipeline worker making a job of `tier`.
fn desired_priority(tier: u8, lowest_setting: bool, boost_active: bool) -> i32 {
    use windows::Win32::System::Threading::{
        THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_LOWEST, THREAD_PRIORITY_NORMAL,
    };
    match tier {
        // Screen passes and pipelines a draw is blocked on run level with the
        // title's recording threads (which are normal priority).
        TIER_SCREEN | TIER_REQUESTED => THREAD_PRIORITY_NORMAL.0,
        // Background warming: normal while loading, below-normal in-world
        // (lowest by default, level with the streaming thread otherwise).
        _ => if boost_active {
            THREAD_PRIORITY_NORMAL.0
        } else if lowest_setting {
            THREAD_PRIORITY_LOWEST.0
        } else {
            THREAD_PRIORITY_BELOW_NORMAL.0
        },
    }
}

thread_local! {
    /// The frame of the recording being translated on this thread
    /// (`VICTORY_THIN=1`); `None` otherwise. A draw recorded in frame N is
    /// charged to frame N's budget, not the frame it is translated in.
    static RECORDING_FRAME: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// The frame a wait on this thread belongs to: the recording's (thin mode) or
/// the current one.
fn wait_frame() -> u64 {
    RECORDING_FRAME.with(|f| f.get()).unwrap_or_else(crate::frame::current)
}

/// Runs `f` with `frame` as [`wait_frame`] on this thread.
pub(crate) fn with_recording_frame(frame: u64, f: impl FnOnce()) {
    RECORDING_FRAME.with(|c| {
        let old = c.replace(Some(frame));
        f();
        c.set(old);
    });
}

/// The host pipeline if it is made (or is made within the frame's budget);
/// queues it on first use.
pub(crate) fn ready(p: &Arc<Pending>) -> Option<&HostPipeline> {
    // A still-pending pipeline was requested by the title too. Prewarm it
    // on the next launch, even if this run never got to use it.
    if !p.used.load(Ordering::Relaxed) && !p.used.swap(true, Ordering::Relaxed) {
        super::library::note_used(&p.key);
    }
    if p.made.get().is_none() {
        start(p, REQUESTED);
        wait(p, wait_frame());
    }
    p.made.get()?.as_ref()
}

/// Makes `p` on this thread if it is not made yet (waiting for a worker
/// that is making it); whether it has a host pipeline.
pub(crate) fn now(p: &Arc<Pending>) -> bool {
    p.make();
    p.made.get().is_some_and(|made| made.is_some())
}

/// At the title's creation of a pipeline: starts it at once if an earlier
/// run drew with it (during loading, so the world's first frames find it).
pub(crate) fn early(p: &Arc<Pending>) {
    if !p.shaders.is_empty() && super::library::wanted_early(&p.key) {
        super::library::EARLY.fetch_add(1, Ordering::Relaxed);
        start(p, EARLY);
    }
}

/// Starts `p` at REQUESTED priority without marking it used: a pipeline bound
/// is likely drawn soon, so its compile starts at bind, not at the draw that
/// needs it.
pub(crate) fn prefetch(p: &Arc<Pending>) {
    start(p, REQUESTED);
}

/// Whether the frame's budget is one for all threads together: a thread
/// that waits takes what is left of it and gives back what it did not
/// use. Counted only when a wait had ended, the budget was one per thread:
/// each of the title's five recording threads began its first wait of a
/// frame with nothing counted yet and waited the whole half second, 2.5 s
/// of waiting in every frame in which pipelines were being made (runs
/// 34-49: "2504 ms waiting for pipelines" in each such frame).
/// On by default; `VICTORY_PIPELINE_WAIT_SHARED=0` counts as before.
fn shared_budget() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_WAIT_SHARED").is_none_or(|v| v != "0"))
}

/// The three wait-budget lanes, each with its own per-frame spent ring.
#[derive(Clone, Copy, PartialEq)]
enum Lane {
    /// Regular graphics draws.
    Graphics,
    /// Compute dispatches.
    Compute,
    /// Critical passes (screen, rectangle and quad screen, decoration bakes).
    Screen,
}

impl Lane {
    fn index(self) -> usize {
        match self {
            Lane::Graphics => 0,
            Lane::Compute => 1,
            Lane::Screen => 2,
        }
    }
}

/// A lane's spent wait budget, in an 8-slot ring so a recording translated a
/// frame late (`VICTORY_THIN=1`) does not share a slot with the frames around
/// it (`highest` is the newest frame seen).
struct FrameBudget {
    slots: [(u64, Duration); 8],
    highest: u64,
}

impl FrameBudget {
    const fn empty() -> Self {
        FrameBudget { slots: [(u64::MAX, Duration::ZERO); 8], highest: u64::MAX }
    }

    /// The spent duration of `frame`'s slot, reset if it held another frame.
    fn slot_mut(&mut self, frame: u64) -> &mut Duration {
        if frame > self.highest {
            self.highest = frame;
        }
        let slot = &mut self.slots[frame as usize & 7];
        if slot.0 != frame {
            *slot = (frame, Duration::ZERO);
        }
        &mut slot.1
    }

    /// As [`slot_mut`], but `None` if the slot was taken by another frame.
    fn slot_of(&mut self, frame: u64) -> Option<&mut Duration> {
        let slot = &mut self.slots[frame as usize & 7];
        (slot.0 == frame).then_some(&mut slot.1)
    }
}

/// The wait budget of a lane, by what the title is doing.
fn lane_budget(lane: Lane) -> Duration {
    match lane {
        Lane::Screen => screen_budget(),
        Lane::Compute if in_world_steady() => world_compute_wait(),
        Lane::Graphics if in_world_steady() => world_pipeline_wait(),
        _ => wait_budget(),
    }
}

/// Waits for `p` while frame `frame` has budget left.
fn wait(p: &Pending, frame: u64) {
    let lane = if p.critical && screens_first() {
        Lane::Screen
    } else if p.compute {
        Lane::Compute
    } else {
        Lane::Graphics
    };
    let budget = lane_budget(lane);
    if budget.is_zero() {
        return;
    }
    // The frame's budget, spent across all threads (or per thread with
    // `VICTORY_PIPELINE_WAIT_SHARED=0`), per lane.
    static SPENT: [Mutex<FrameBudget>; 3] = [const { Mutex::new(FrameBudget::empty()) }; 3];
    let spent_of = &SPENT[lane.index()];
    let shared = shared_budget();
    let left = {
        let mut spent = spent_of.lock().unwrap_or_else(|e| e.into_inner());
        let slot = spent.slot_mut(frame);
        let left = budget.saturating_sub(*slot);
        if shared {
            *slot += left;
        }
        left
    };
    if left.is_zero() {
        return;
    }
    let waiting = crate::timeline::span("pipeline: first use waits for a host pipeline");
    let started = Instant::now();
    let (lock, done) = done_of(p);
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() {
        let rest = left.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done.wait_timeout(held, rest).unwrap_or_else(|e| e.into_inner()).0;
    }
    drop(held);
    let waited = started.elapsed().min(left);
    waiting.end();
    if lane == Lane::Screen && waited > Duration::from_millis(500) {
        tracing::info!(
            ms = waited.as_millis() as u64,
            made = p.made.get().is_some(),
            shaders = ?p.shaders,
            "a pass over the screen was waited for (VICTORY_SCREEN_WAIT_MS=0: dropped past half a second a frame)"
        );
    }
    {
        let mut spent = spent_of.lock().unwrap_or_else(|e| e.into_inner());
        // A frame that began meanwhile has its own slot.
        if let Some(slot) = spent.slot_of(frame) {
            if shared {
                *slot = slot.saturating_sub(left - waited);
            } else {
                *slot += waited;
            }
        }
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        crate::copystat::C::PipelineWaitUs,
        started.elapsed().as_micros() as u64,
    );
}

const NOT_QUEUED: u8 = 0;
const EARLY: u8 = 1;
const REQUESTED: u8 = 2;
const MAKING: u8 = 3;

/// What a worker took from the queue, for its thread priority and the
/// in-world fast-lane reserve.
const TIER_SCREEN: u8 = 0;
const TIER_REQUESTED: u8 = 1;
const TIER_EARLY: u8 = 2;

/// How many workers may make pipelines at once that are known to take long
/// and that no draw has asked for: a quarter of them. On a fresh cache (a
/// change of the translator or the driver) the early list begins with
/// programs that take tens of seconds each: all 16 workers were in them
/// through the whole of the title's loading, 97 pipelines made in 20 s, and
/// the first minute and a half of the world ran at 0.5 to 1.6 s a frame,
/// dropping up to 2,900 draws a frame (run shaders-1).
/// `VICTORY_PIPELINE_SLOW=0`: in the order the title creates them, as before.
fn slow_limit() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        if std::env::var_os("VICTORY_PIPELINE_SLOW").is_some_and(|v| v == "0") {
            usize::MAX
        } else {
            (worker_total() / 4).max(1)
        }
    })
}

struct Queue {
    /// Requested passes over the screen: before all others, and the only ones
    /// [`SCREEN_WORKERS`] take.
    screen: VecDeque<Arc<Pending>>,
    early: VecDeque<Arc<Pending>>,
    /// Those of the early ones known to take long, and how many of them are
    /// being made ([`slow_limit`]).
    slow: VecDeque<Arc<Pending>>,
    slow_now: usize,
    /// Early (non-slow) jobs being made, for the in-world fast-lane reserve.
    early_now: usize,
    requested: VecDeque<Arc<Pending>>,
}

impl Queue {
    const fn new() -> Self {
        Self {
            screen: VecDeque::new(),
            early: VecDeque::new(),
            slow: VecDeque::new(),
            slow_now: 0,
            early_now: 0,
            requested: VecDeque::new(),
        }
    }

    /// Queues an unrequested pipeline or promotes one queued at creation.
    /// Returns whether it is new to the queue, or `None` if nothing changed.
    fn push(&mut self, p: &Arc<Pending>, priority: u8) -> Option<bool> {
        self.push_as(p, priority, slow_limit() != usize::MAX && super::library::slow(&p.key))
    }

    fn push_as(&mut self, p: &Arc<Pending>, priority: u8, slow: bool) -> Option<bool> {
        let prev = p.queue_state.load(Ordering::Relaxed);
        if prev >= priority {
            return None;
        }
        p.queue_state.store(priority, Ordering::Relaxed);
        if priority == REQUESTED && p.screen && screens_first() {
            self.screen.push_back(p.clone());
        } else if priority == REQUESTED {
            self.requested.push_back(p.clone());
        } else if slow {
            self.slow.push_back(p.clone());
        } else {
            self.early.push_back(p.clone());
        }
        Some(prev == NOT_QUEUED)
    }

    fn pop(&mut self, screens_only: bool, slow_limit: usize, early_limit: usize) -> Option<(Arc<Pending>, u8)> {
        // An early entry can have been promoted and claimed already.
        let unclaimed = |list: &mut VecDeque<Arc<Pending>>| {
            std::iter::from_fn(|| list.pop_front()).find(|p| p.queue_state.load(Ordering::Relaxed) == EARLY)
        };
        if let Some(p) = self.screen.pop_front() {
            p.queue_state.store(MAKING, Ordering::Relaxed);
            return Some((p, TIER_SCREEN));
        }
        if screens_only {
            return None;
        }
        if let Some(p) = self.requested.pop_front() {
            p.queue_state.store(MAKING, Ordering::Relaxed);
            return Some((p, TIER_REQUESTED));
        }
        // Early (and slow) jobs, capped in-world so two workers stay ready for
        // requested pipelines.
        if self.early_now + self.slow_now < early_limit {
            if let Some(p) = unclaimed(&mut self.early) {
                self.early_now += 1;
                p.queue_state.store(MAKING, Ordering::Relaxed);
                return Some((p, TIER_EARLY));
            }
            if self.slow_now < slow_limit {
                if let Some(p) = unclaimed(&mut self.slow) {
                    p.was_slow.store(true, Ordering::Relaxed);
                    self.slow_now += 1;
                    p.queue_state.store(MAKING, Ordering::Relaxed);
                    return Some((p, TIER_EARLY));
                }
            }
        }
        None
    }
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue::new());
static AVAILABLE: Condvar = Condvar::new();

/// Start or promote work for a draw without waiting behind the entire early
/// list. Queue changes and worker claims happen under the same lock.
fn start(p: &Arc<Pending>, priority: u8) {
    if p.queue_state.load(Ordering::Relaxed) >= priority {
        return;
    }
    if !workers() {
        if p.queue_state.swap(MAKING, Ordering::Relaxed) == NOT_QUEUED {
            p.make();
        }
        return;
    }
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(new) = queue.push(p, priority) {
        if new {
            PEAK.fetch_max(WAITING.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
            WARM_TOTAL.fetch_add(1, Ordering::Relaxed);
        }
        // (One woken could be a worker that takes passes over the screen
        // alone: it would leave the pipeline to nobody.)
        AVAILABLE.notify_all();
    }
}

/// Workers that make passes over the screen alone, besides the others.
const SCREEN_WORKERS: usize = 2;

fn next(screens_only: bool) -> (Arc<Pending>, u8) {
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(job) = queue.pop(screens_only, slow_limit(), early_limit()) {
            return job;
        }
        queue = AVAILABLE.wait(queue).unwrap_or_else(|e| e.into_inner());
    }
}

/// How many early (and slow) jobs may be in flight at once. In-world two
/// workers are reserved for requested pipelines (when there are enough);
/// during loading screens nothing is reserved.
fn early_limit() -> usize {
    let workers = worker_total();
    if LOADING_BOOST.load(Ordering::Relaxed) || workers < 4 {
        usize::MAX
    } else {
        workers - 2
    }
}

/// Whether no requested or screen pipelines are queued (the background saver
/// defers while they are).
pub(crate) fn queues_empty() -> bool {
    let queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    queue.requested.is_empty() && queue.screen.is_empty()
}

/// How many workers make pipelines: `asked` (`VICTORY_PIPELINE_WORKERS`, 1 to
/// 64), else two thirds of the logical processors, 2 to 16. They run below the
/// title's own threads, and a cold cache queues hundreds at once.
fn worker_count(asked: Option<usize>, processors: usize) -> usize {
    match asked {
        Some(n) => n.clamp(1, 64),
        None => (processors * 2 / 3).clamp(2, 16),
    }
}

/// The number of pipeline workers ([`worker_count`]), cached.
fn worker_total() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        worker_count(
            std::env::var("VICTORY_PIPELINE_WORKERS").ok().and_then(|v| v.trim().parse().ok()),
            std::thread::available_parallelism().map_or(2, |n| n.get()),
        )
    })
}

/// Starts the workers ([`worker_total`]) on first use.
fn workers() -> bool {
    static RUNNING: OnceLock<bool> = OnceLock::new();
    *RUNNING.get_or_init(|| {
        let mut running = false;
        let workers = worker_total();
        tracing::info!(workers, "pipeline workers");
        super::library::start_saver();
        let screens = if screens_first() { SCREEN_WORKERS } else { 0 };
        for i in 0..workers + screens {
            let screens_only = i >= workers;
            let spawned = std::thread::Builder::new()
                .name(format!("victory-pipelines-{i}"))
                .spawn(move || {
                    // Below the title's own threads (loading, recording), and
                    // the streaming one, which is below normal itself
                    // (`VICTORY_PIPELINE_LOWEST=0`: level with it, as before).
                    let lowest = std::env::var_os("VICTORY_PIPELINE_LOWEST").is_none_or(|v| v != "0");
                    let mut current_prio: Option<i32> = None;
                    let apply_priority = |current: &mut Option<i32>, tier: u8| {
                        let want = desired_priority(
                            tier,
                            lowest,
                            boost_enabled() && LOADING_BOOST.load(Ordering::Relaxed),
                        );
                        if *current != Some(want) {
                            // SAFETY: a plain call on our own thread.
                            unsafe {
                                use windows::Win32::System::Threading::*;
                                let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY(want));
                            }
                            *current = Some(want);
                        }
                    };
                    apply_priority(&mut current_prio, TIER_EARLY);
                    loop {
                        let (p, tier) = next(screens_only);
                        apply_priority(&mut current_prio, tier);
                        p.make();
                        WARM_DONE.fetch_add(1, Ordering::Relaxed);
                        // Release the early/slow fast-lane reservation.
                        if tier == TIER_EARLY {
                            let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
                            if p.was_slow.load(Ordering::Relaxed) {
                                queue.slow_now -= 1;
                            } else {
                                queue.early_now -= 1;
                            }
                            AVAILABLE.notify_all();
                        }
                        if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                            drained();
                        }
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "no pipeline worker");
            } else {
                running = true;
            }
        }
        running
    })
}

/// The most pipelines waiting at once since the workers last ran out.
static PEAK: AtomicU64 = AtomicU64::new(0);

/// The workers ran out of work: logged after a batch of 10 or more, and
/// the cache saved (at most every 20 s).
fn drained() {
    use super::library::{EARLY, LOADED, MADE};
    WARM_TOTAL.store(0, Ordering::Relaxed);
    WARM_DONE.store(0, Ordering::Relaxed);
    if PEAK.swap(0, Ordering::Relaxed) >= 10 {
        tracing::info!(
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            "host pipelines all made"
        );
    }
    // (The library is saved by the background saver, off this thread.)
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_WORKERS: Mutex<()> = Mutex::new(());

    #[test]
    fn worker_count_follows_the_processors_and_the_switch() {
        assert_eq!(worker_count(None, 1), 2);
        assert_eq!(worker_count(None, 24), 16);
        assert_eq!(worker_count(None, 64), 16);
        assert_eq!(worker_count(Some(0), 24), 1);
        assert_eq!(worker_count(Some(20), 24), 20);
    }

    #[test]
    fn desired_priority_follows_role_and_loading_boost() {
        use windows::Win32::System::Threading::{
            THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_LOWEST, THREAD_PRIORITY_NORMAL,
        };
        // Screen and requested jobs run level with the recording threads.
        assert_eq!(desired_priority(TIER_SCREEN, true, false), THREAD_PRIORITY_NORMAL.0);
        assert_eq!(desired_priority(TIER_REQUESTED, true, false), THREAD_PRIORITY_NORMAL.0);
        // Background warming boosts to normal while loading.
        assert_eq!(desired_priority(TIER_EARLY, true, true), THREAD_PRIORITY_NORMAL.0);
        assert_eq!(desired_priority(TIER_EARLY, false, true), THREAD_PRIORITY_NORMAL.0);
        // In-world (boost inactive): drops to lowest or below normal.
        assert_eq!(desired_priority(TIER_EARLY, true, false), THREAD_PRIORITY_LOWEST.0);
        assert_eq!(desired_priority(TIER_EARLY, false, false), THREAD_PRIORITY_BELOW_NORMAL.0);
    }

    /// A first use queues the pipeline and, within the budget, gets the
    /// worker's answer (here: no host pipeline for no shaders).
    #[test]
    fn first_use_is_made_by_a_worker() {
        let _guard = TEST_WORKERS.lock().unwrap_or_else(|e| e.into_inner());
        let p = Arc::new(Pending::new(Vec::new(), None));
        assert!(ready(&p).is_none());
        assert_ne!(p.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    #[test]
    fn progress_reflects_queued_done_waiting_and_resets_on_drain() {
        assert_eq!(progress_of(0, 0, 0), (0, 0, 0));
        assert_eq!(progress_of(3, 10, 4), (3, 10, 4));
        assert_eq!(progress_of(5, 2, 4), (5, 9, 4));

        let _guard = TEST_WORKERS.lock().unwrap_or_else(|e| e.into_inner());
        // Wait for any previously queued work from workers to finish draining.
        let started = Instant::now();
        while WAITING.load(Ordering::Relaxed) > 0 && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert_eq!(WAITING.load(Ordering::Relaxed), 0);

        WARM_TOTAL.store(5, Ordering::Relaxed);
        WARM_DONE.store(2, Ordering::Relaxed);
        assert_eq!(progress(), (2, 5, 0));

        drained();
        assert_eq!(progress(), (0, 0, 0));

        // Real worker round-trip increments WARM_TOTAL/WARM_DONE and drains back to 0.
        let p = Arc::new(Pending::new(Vec::new(), None));
        start(&p, REQUESTED);
        let started = Instant::now();
        while (p.made.get().is_none() || progress() != (0, 0, 0))
            && started.elapsed() < Duration::from_secs(10)
        {
            std::thread::yield_now();
        }
        assert!(p.made.get().is_some());
        assert_eq!(progress(), (0, 0, 0));
    }

    #[test]
    fn pending_pipeline_is_recorded_for_the_next_launch() {
        let mut p = Pending::new(Vec::new(), None);
        p.key = "pending-pipeline-test".to_owned();
        // Simulate a worker that has taken the job but cannot finish it yet.
        p.queue_state.store(MAKING, Ordering::Relaxed);
        let p = Arc::new(p);

        assert!(!super::super::library::wanted_early(&p.key));
        assert!(ready(&p).is_none());
        assert!(p.made.get().is_none());
        assert!(super::super::library::wanted_early(&p.key));
    }

    #[test]
    fn draws_promote_early_pipelines_without_claiming_them_twice() {
        let earlier = Arc::new(Pending::new(Vec::new(), None));
        let promoted = Arc::new(Pending::new(Vec::new(), None));
        let requested = Arc::new(Pending::new(Vec::new(), None));
        let mut queue = Queue::new();

        assert_eq!(queue.push(&earlier, EARLY), Some(true));
        assert_eq!(queue.push(&promoted, EARLY), Some(true));
        assert_eq!(queue.push(&requested, REQUESTED), Some(true));
        assert_eq!(queue.push(&promoted, REQUESTED), Some(false));
        assert_eq!(queue.push(&promoted, REQUESTED), None);

        assert!(Arc::ptr_eq(&queue.pop(false, 1, usize::MAX).unwrap().0, &requested));
        assert!(Arc::ptr_eq(&queue.pop(false, 1, usize::MAX).unwrap().0, &promoted));
        assert_eq!(queue.push(&promoted, REQUESTED), None);
        assert!(Arc::ptr_eq(&queue.pop(false, 1, usize::MAX).unwrap().0, &earlier));
        assert!(queue.pop(false, 1, usize::MAX).is_none());
    }

    #[test]
    fn pipelines_known_to_take_long_leave_workers_for_the_others() {
        let new = || Arc::new(Pending::new(Vec::new(), None));
        let (long, longer, asked, quick) = (new(), new(), new(), new());
        let mut queue = Queue::new();
        for p in [&long, &longer, &asked] {
            assert_eq!(queue.push_as(p, EARLY, true), Some(true));
        }
        assert_eq!(queue.push_as(&quick, EARLY, false), Some(true));
        // The quick one first, then one long one at a time; one a draw asks
        // for waits for nobody.
        assert!(Arc::ptr_eq(&queue.pop(false, 1, usize::MAX).unwrap().0, &quick));
        assert!(Arc::ptr_eq(&queue.pop(false, 1, usize::MAX).unwrap().0, &long));
        assert!(queue.pop(false, 1, usize::MAX).is_none());
        assert_eq!(queue.push_as(&asked, REQUESTED, true), Some(false));
        assert!(Arc::ptr_eq(&queue.pop(false, 1, usize::MAX).unwrap().0, &asked));
        assert!(!asked.was_slow.load(Ordering::Relaxed));
        assert!(queue.pop(false, 1, usize::MAX).is_none());
        queue.slow_now -= 1;
        assert!(Arc::ptr_eq(&queue.pop(false, 1, usize::MAX).unwrap().0, &longer));
        assert!(queue.pop(false, 1, usize::MAX).is_none());
    }

    /// Passes over the screen go before all others, and workers of their own
    /// take nothing else.
    #[test]
    fn passes_over_the_screen_go_first() {
        let of = |screen| {
            let mut p = Pending::new(Vec::new(), None);
            p.screen = screen;
            Arc::new(p)
        };
        let (world, screen) = (of(false), of(true));
        let mut queue = Queue::new();
        assert_eq!(queue.push(&world, REQUESTED), Some(true));
        assert_eq!(queue.push(&screen, REQUESTED), Some(true));
        assert!(Arc::ptr_eq(&queue.pop(true, 1, usize::MAX).unwrap().0, &screen));
        assert!(queue.pop(true, 1, usize::MAX).is_none());
        assert!(Arc::ptr_eq(&queue.pop(false, 1, usize::MAX).unwrap().0, &world));
    }
}
