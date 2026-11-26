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

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};
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
    /// A pass over the screen ([`GraphicsState::screen_pass`]).
    screen: bool,
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
    pub(crate) fn new(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let mut key = blake3::Hasher::new();
        for s in &shaders {
            key.update(s.stage.as_bytes());
            key.update(s.hash.as_bytes());
        }
        key.update(graphics.as_ref().map(|g| g.describe()).unwrap_or_default().as_bytes());
        Pending {
            screen: graphics.as_ref().is_some_and(|g| g.screen_pass()),
            shaders,
            graphics,
            queue_state: AtomicU8::new(NOT_QUEUED),
            was_slow: AtomicBool::new(false),
            made: OnceLock::new(),
            key: key.finalize().to_hex()[..32].to_owned(),
            used: AtomicBool::new(false),
            spec_constants: SpecConstants::new(),
        }
    }

    pub(super) fn key(&self) -> &str {
        &self.key
    }

    /// The specialization constants to patch into the shader when the host
    /// pipeline is made (empty by default).
    pub(crate) fn with_spec_constants(mut self, spec_constants: SpecConstants) -> Pending {
        self.spec_constants = spec_constants;
        self
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
        let (lock, done) = &DONE;
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        done.notify_all();
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
/// the child died. A child that died costs this one pipeline,
/// unless the translate path's bisect has cached a mnemonic whose disabling
/// lets the driver compile it: then the pipeline is made with that fallback.
/// A pipeline [`proven`] before is made at once, without a child.
fn isolated_make(
    key: &str,
    shaders: &[Arc<Shader>],
    graphics: Option<&GraphicsState>,
    spec_constants: &SpecConstants,
) -> Option<HostPipeline> {
    // (The key does not cover specialization constants: only the plain
    // pipeline is taken as proven.)
    let plain = spec_constants.is_empty();
    if plain && proven(key) {
        PROVEN_SKIPPED.fetch_add(1, Ordering::Relaxed);
        return HostPipeline::new(shaders, graphics, spec_constants);
    }
    let bytes = super::known::pending(shaders, graphics, spec_constants);
    if canary(&bytes, None) {
        if plain {
            note_proven(key);
        }
    } else {
        let Some((hash, mnemonic)) = fallback_for(shaders) else {
            tracing::warn!(
                ?shaders,
                "no host pipeline: the driver crashed compiling it (isolated in a child process)"
            );
            return None;
        };
        // The fallback is re-checked in a child first: a wrong or stale
        // mnemonic must cost a child, not the run (the driver's crash is
        // deterministic, so this child's survival means the fallback works).
        if !canary(&bytes, Some((hash.as_str(), mnemonic))) {
            tracing::warn!(
                ?shaders,
                "no host pipeline: the driver crashed compiling it even with the cached fallback"
            );
            return None;
        }
        tracing::info!(?shaders, %mnemonic, "host pipeline made with the cached fallback");
        // The mnemonic is passed into this one shader's translation, not set
        // process-wide, so other shaders are untouched.
        return HostPipeline::new_with(
            shaders,
            graphics,
            spec_constants,
            Some((hash.as_str(), mnemonic)),
        );
    }
    HostPipeline::new(shaders, graphics, spec_constants)
}

/// Pipelines made without a child because they were [`proven`], since the
/// start (for the log's canary line).
static PROVEN_SKIPPED: AtomicU64 = AtomicU64::new(0);

/// `warm-done.txt` in the pipeline cache folder: `<fingerprint> <key>` for
/// each pipeline made in a child process without a crash, by `victory warm`
/// or by a run's canary, with the translator and driver of
/// [`super::known::fingerprint`].
fn proven_file() -> Option<std::path::PathBuf> {
    super::library::folder().map(|dir| dir.join("warm-done.txt"))
}

/// This run's translator and driver, as `warm-done.txt` names them.
fn fingerprint() -> &'static str {
    static FINGERPRINT: OnceLock<String> = OnceLock::new();
    FINGERPRINT.get_or_init(super::known::fingerprint)
}

/// Whether `key` was made in a child process before, with this translator
/// and driver: its compile does not crash (the driver's crash is
/// deterministic), so it needs no canary. `victory warm` ahead of a run, and
/// each canary that made its pipeline, add to the list; a new translator or
/// driver starts it again.
fn proven(key: &str) -> bool {
    static PROVEN: OnceLock<std::collections::HashSet<String>> = OnceLock::new();
    PROVEN
        .get_or_init(|| {
            let of = fingerprint();
            let text = proven_file().and_then(|f| std::fs::read_to_string(f).ok()).unwrap_or_default();
            let set: std::collections::HashSet<String> = text
                .lines()
                .filter_map(|l| l.split_once(' '))
                .filter(|(f, _)| *f == of)
                .map(|(_, k)| k.trim().to_owned())
                .collect();
            tracing::info!(pipelines = set.len(), fingerprint = of, "pipelines proven in a child before: made without a canary");
            set
        })
        .contains(key)
}

/// Adds `key` to [`proven_file`] for later runs.
fn note_proven(key: &str) {
    use std::io::Write;
    static FILE: Mutex<()> = Mutex::new(());
    let Some(path) = proven_file() else { return };
    let _held = FILE.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{} {key}", fingerprint());
    }
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

/// How a canary child ended.
enum Ended {
    Made,
    /// Exit code 1: the pipeline refused, or the child had no host device.
    Refused,
    /// Any other end (a driver crash is an exception code), or not started.
    Died(Option<i32>),
}

/// Canary children at once (`VICTORY_CANARY_JOBS`, default 16). Each opens a
/// D3D12 device of its own, and many at once can lose theirs: started 36 at
/// once, 28 exited 1 with "The GPU device instance has been suspended"
/// (0x887A0005; an offline bisect of run-keep's crashes). At 4
/// the canaries could not keep up: 1,500 draws a frame waited for theirs five
/// minutes into the world (run combined-run2).
fn canary_jobs() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_CANARY_JOBS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(16).max(1)
    })
}

/// Canary children running, and signalled when one ends.
static CANARIES: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

/// Canary outcomes since the start: made, refused (exit 1), died.
static CANARY_COUNTS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// Has a child process make the serialised pipeline; whether the run may make
/// it too: "no" only if the child died (a driver crash ends it with an
/// exception code, such as 0xc0000005). A child that exits 1 refused it
/// without a crash, and the child refuses some pipelines the run makes: 251
/// in run combined-run2, refused again on a retry alone, while runs without
/// isolation drew with every one of them. Taken for crashes, those were left
/// out for the rest of the run and the world was not drawn. With `fallback`
/// (`(shader hash, mnemonic)`), the child makes the pipeline with that
/// shader's mnemonic lowered as a no-op, as the run would.
fn canary(bytes: &[u8], fallback: Option<(&str, &str)>) -> bool {
    let ended = canary_once(bytes, fallback);
    let at = match &ended {
        Ended::Made => 0,
        Ended::Refused => 1,
        Ended::Died(code) => {
            tracing::info!(code = ?code.map(|c| format!("{:#010x}", c as u32)), "a pipeline canary child died");
            2
        }
    };
    let n = CANARY_COUNTS[at].fetch_add(1, Ordering::Relaxed) + 1;
    let total: u64 = CANARY_COUNTS.iter().map(|c| c.load(Ordering::Relaxed)).sum();
    if n == 1 || total % 100 == 0 {
        let [made, refused, dead] = CANARY_COUNTS.each_ref().map(|c| c.load(Ordering::Relaxed));
        tracing::info!(
            made,
            refused_in_child = refused,
            died = dead,
            proven_before = PROVEN_SKIPPED.load(Ordering::Relaxed),
            at_once = canary_jobs(),
            "pipeline canaries since the start (VICTORY_CANARY_JOBS)"
        );
    }
    !matches!(ended, Ended::Died(_))
}

/// One canary child, when fewer than [`canary_jobs`] run. The child runs at
/// idle priority, like [`workers`] do.
fn canary_once(bytes: &[u8], fallback: Option<(&str, &str)>) -> Ended {
    {
        let (count, ended) = &CANARIES;
        let mut n = count.lock().unwrap_or_else(|e| e.into_inner());
        while *n >= canary_jobs() {
            n = ended.wait(n).unwrap_or_else(|e| e.into_inner());
        }
        *n += 1;
    }
    let made = canary_child(bytes, fallback);
    let (count, ended) = &CANARIES;
    *count.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
    ended.notify_one();
    made
}

fn canary_child(bytes: &[u8], fallback: Option<(&str, &str)>) -> Ended {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    const IDLE_PRIORITY_CLASS: u32 = 0x40;
    // Staged in the pipeline cache folder (always writable, and outside the
    // repository), not the system's temp folder.
    let dir = super::library::folder().unwrap_or_else(std::env::temp_dir);
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join(format!(
        "victory-make-pending-{}-{}.bin",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    if std::fs::write(&file, bytes).is_err() {
        return Ended::Died(None);
    }
    let made = std::env::current_exe()
        .ok()
        .and_then(|exe| {
            let mut cmd = Command::new(exe);
            cmd.arg("make-pending")
                .arg(&file)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(IDLE_PRIORITY_CLASS);
            if let Some((hash, mnemonic)) = fallback {
                cmd.env(super::known::PENDING_FALLBACK, format!("{hash} {mnemonic}"));
            }
            cmd.spawn().ok()
        })
        .and_then(|mut child| child.wait().ok())
        .map_or(Ended::Died(None), |s| match s.code() {
            _ if s.success() => Ended::Made,
            Some(1) => Ended::Refused,
            code => Ended::Died(code),
        });
    let _ = std::fs::remove_file(&file);
    made
}

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

/// Signalled whenever a worker has made a pipeline.
static DONE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

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

/// Pipelines queued and not made yet.
static WAITING: AtomicU64 = AtomicU64::new(0);

/// Pipelines queued and not made yet (for the frame line).
pub(crate) fn waiting() -> u64 {
    WAITING.load(Ordering::Relaxed)
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
        wait(p, crate::frame::current());
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

/// Waits for `p` while frame `frame` has budget left.
fn wait(p: &Pending, frame: u64) {
    let screen = p.screen && screens_first();
    let budget = if screen { screen_budget() } else { wait_budget() };
    if budget.is_zero() {
        return;
    }
    // The frame the budget belongs to, and what was waited of it (or is
    // taken by a thread waiting now); of the passes over the screen too.
    static SPENT: [Mutex<(u64, Duration)>; 2] = [const { Mutex::new((u64::MAX, Duration::ZERO)) }; 2];
    let spent_of = &SPENT[usize::from(screen)];
    let shared = shared_budget();
    let left = {
        let mut spent = spent_of.lock().unwrap_or_else(|e| e.into_inner());
        if spent.0 != frame {
            *spent = (frame, Duration::ZERO);
        }
        let left = budget.saturating_sub(spent.1);
        if shared {
            spent.1 += left;
        }
        left
    };
    if left.is_zero() {
        return;
    }
    let waiting = crate::timeline::span("pipeline: first use waits for a host pipeline");
    let started = Instant::now();
    let (lock, done) = &DONE;
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
    if screen && waited > Duration::from_millis(500) {
        tracing::info!(
            ms = waited.as_millis() as u64,
            made = p.made.get().is_some(),
            shaders = ?p.shaders,
            "a pass over the screen was waited for (VICTORY_SCREEN_WAIT_MS=0: dropped past half a second a frame)"
        );
    }
    {
        let mut spent = spent_of.lock().unwrap_or_else(|e| e.into_inner());
        // A frame that began meanwhile has its own budget.
        if spent.0 == frame {
            if shared {
                spent.1 = spent.1.saturating_sub(left - waited);
            } else {
                spent.1 += waited;
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
            (worker_count(None, std::thread::available_parallelism().map_or(2, |n| n.get())) / 4).max(1)
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
    requested: VecDeque<Arc<Pending>>,
}

impl Queue {
    const fn new() -> Self {
        Self {
            screen: VecDeque::new(),
            early: VecDeque::new(),
            slow: VecDeque::new(),
            slow_now: 0,
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

    fn pop(&mut self, screens_only: bool, slow_limit: usize) -> Option<Arc<Pending>> {
        // An early entry can have been promoted and claimed already.
        let unclaimed = |list: &mut VecDeque<Arc<Pending>>| {
            std::iter::from_fn(|| list.pop_front()).find(|p| p.queue_state.load(Ordering::Relaxed) == EARLY)
        };
        let mut p = self.screen.pop_front();
        if p.is_none() && !screens_only {
            p = self.requested.pop_front().or_else(|| unclaimed(&mut self.early));
            if p.is_none() && self.slow_now < slow_limit {
                p = unclaimed(&mut self.slow);
                if let Some(p) = &p {
                    p.was_slow.store(true, Ordering::Relaxed);
                    self.slow_now += 1;
                }
            }
        }
        let p = p?;
        p.queue_state.store(MAKING, Ordering::Relaxed);
        Some(p)
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
        }
        // (One woken could be a worker that takes passes over the screen
        // alone: it would leave the pipeline to nobody.)
        AVAILABLE.notify_all();
    }
}

/// Workers that make passes over the screen alone, besides the others.
const SCREEN_WORKERS: usize = 2;

fn next(screens_only: bool) -> Arc<Pending> {
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(p) = queue.pop(screens_only, slow_limit()) {
            return p;
        }
        queue = AVAILABLE.wait(queue).unwrap_or_else(|e| e.into_inner());
    }
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

/// Starts the workers ([`worker_count`]) on first use.
fn workers() -> bool {
    static RUNNING: OnceLock<bool> = OnceLock::new();
    *RUNNING.get_or_init(|| {
        let mut running = false;
        let workers = worker_count(
            std::env::var("VICTORY_PIPELINE_WORKERS")
                .ok()
                .and_then(|v| v.trim().parse().ok()),
            std::thread::available_parallelism().map_or(2, |n| n.get()),
        );
        tracing::info!(workers, "pipeline workers");
        let screens = if screens_first() { SCREEN_WORKERS } else { 0 };
        for i in 0..workers + screens {
            let screens_only = i >= workers;
            let spawned = std::thread::Builder::new()
                .name(format!("victory-pipelines-{i}"))
                .spawn(move || {
                    // Below the title's own threads (loading, recording),
                    // and the streaming one, which is below normal itself
                    // (`VICTORY_PIPELINE_LOWEST=0`: level with it, as before).
                    let lowest = !screens_only && std::env::var_os("VICTORY_PIPELINE_LOWEST").is_none_or(|v| v != "0");
                    // SAFETY: a plain call on our own thread.
                    unsafe {
                        use windows::Win32::System::Threading::*;
                        let priority = if lowest { THREAD_PRIORITY_LOWEST } else { THREAD_PRIORITY_BELOW_NORMAL };
                        let _ = SetThreadPriority(GetCurrentThread(), priority);
                    }
                    loop {
                        let p = next(screens_only);
                        p.make();
                        if p.was_slow.load(Ordering::Relaxed) {
                            let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
                            queue.slow_now -= 1;
                            AVAILABLE.notify_all();
                        }
                        if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                            drained();
                        } else {
                            super::library::save(false);
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
    if PEAK.swap(0, Ordering::Relaxed) >= 10 {
        tracing::info!(
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            "host pipelines all made"
        );
    }
    super::library::save(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_count_follows_the_processors_and_the_switch() {
        assert_eq!(worker_count(None, 1), 2);
        assert_eq!(worker_count(None, 24), 16);
        assert_eq!(worker_count(None, 64), 16);
        assert_eq!(worker_count(Some(0), 24), 1);
        assert_eq!(worker_count(Some(20), 24), 20);
    }

    /// A first use queues the pipeline and, within the budget, gets the
    /// worker's answer (here: no host pipeline for no shaders).
    #[test]
    fn first_use_is_made_by_a_worker() {
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

        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &requested));
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &promoted));
        assert_eq!(queue.push(&promoted, REQUESTED), None);
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &earlier));
        assert!(queue.pop(false, 1).is_none());
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
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &quick));
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &long));
        assert!(queue.pop(false, 1).is_none());
        assert_eq!(queue.push_as(&asked, REQUESTED, true), Some(false));
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &asked));
        assert!(!asked.was_slow.load(Ordering::Relaxed));
        assert!(queue.pop(false, 1).is_none());
        queue.slow_now -= 1;
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &longer));
        assert!(queue.pop(false, 1).is_none());
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
        assert!(Arc::ptr_eq(&queue.pop(true, 1).unwrap(), &screen));
        assert!(queue.pop(true, 1).is_none());
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &world));
    }
}
