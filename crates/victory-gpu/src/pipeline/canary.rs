//! Canary children that make host pipelines in isolation
//! (`VICTORY_PIPELINE_ISOLATE=1`): a pool of persistent `victory canary-worker`
//! processes, each making many pipelines, so a driver crash ends a child, not
//! the run, without spawning a process (and opening a D3D12 device and the
//! host pipeline library) for every pipeline.
//!
//! A request and a response are framed over the child's pipes:
//! request `[b"VCW1"][fallback_len: u32][payload_len: u32][fallback][payload]`,
//! response `[b"VCW1"][status: u8]` (0 made, 1 refused, 2 device removed).
//! The children never open `host-pipelines.bin`: the app starts each with
//! [`super::known::use_translations`] (translations only), not the pipeline
//! library.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

use super::known;

/// A Win32 handle kept as its raw value. `HANDLE` is a pointer and so not
/// `Send`/`Sync`, but the value is just an integer whose lifetime is managed
/// here: wrap it so worker pools and statics can hold it across threads.
#[derive(Clone, Copy)]
struct RawHandle(usize);

// SAFETY: a handle value has no thread affinity; the handle it names is owned
// and closed exactly once by its holder.
unsafe impl Send for RawHandle {}
unsafe impl Sync for RawHandle {}

/// How a canary child ended.
pub(crate) enum Ended {
    Made,
    Refused,
    /// The child's device was removed (the driver crashed inside it). The
    /// pool retries a removed device on a fresh worker, so this is never
    /// returned by [`make`].
    DeviceRemoved,
    /// Any other end: a driver crash (an exception code), a timeout (the
    /// watchdog's own code), or a child that could not be reaped.
    Died(Option<i32>),
}

const MAGIC: &[u8; 4] = b"VCW1";
/// The exit code the watchdog gives a child it kills for a timeout (not an NT
/// exception code, so it is never blacklisted as a crash).
const TIMEOUT_EXIT: u32 = 42;

/// Children at once (`VICTORY_CANARY_JOBS`, default 4, 1..=16).
fn jobs() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_CANARY_JOBS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(4)
            .clamp(1, 16)
    })
}

/// Pipelines a child makes before it is replaced (`VICTORY_CANARY_MAX_JOBS`,
/// default 64): the compiler's and the translations' memory goes with it.
fn max_jobs() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_CANARY_MAX_JOBS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(64)
            .max(1)
    })
}

/// Seconds a child may sit idle before it is evicted (`VICTORY_CANARY_IDLE_S`,
/// default 30).
fn idle_s() -> Duration {
    static D: OnceLock<Duration> = OnceLock::new();
    *D.get_or_init(|| {
        let s = std::env::var("VICTORY_CANARY_IDLE_S")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(30)
            .max(1);
        Duration::from_secs(s)
    })
}

/// Seconds a make may take before the watchdog kills the child
/// (`VICTORY_CANARY_TIMEOUT_S`, default 180, matching `victory warm`'s
/// `LONGEST`).
fn timeout_s() -> Duration {
    static D: OnceLock<Duration> = OnceLock::new();
    *D.get_or_init(|| {
        let s = std::env::var("VICTORY_CANARY_TIMEOUT_S")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(180)
            .max(1);
        Duration::from_secs(s)
    })
}

/// One persistent child and its pipes.
struct Worker {
    child: Option<Child>,
    stdin: ChildStdin,
    stdout: ChildStdout,
    /// A `PROCESS_TERMINATE` handle, kept apart from the pipes so the
    /// watchdog can kill the child while its pipes are held.
    process: RawHandle,
    made: usize,
    at: Instant,
}

impl Worker {
    /// Closes the child and reaps it (once): drops its stdin first, so a live
    /// child sees end-of-file and exits. Returns its exit code.
    fn reap(mut self) -> Option<i32> {
        drop(self.stdin);
        drop(self.stdout);
        // SAFETY: our own process handle.
        unsafe { CloseHandle(self.process.0 as HANDLE) };
        self.child
            .take()
            .and_then(|mut c| c.wait().ok())
            .and_then(|s| s.code())
    }

    /// Reaps an already-dead child (once) and returns its exit code.
    fn wait_code(&mut self) -> Option<i32> {
        self.child
            .take()
            .and_then(|mut c| c.wait().ok())
            .and_then(|s| s.code())
    }
}

struct Pool {
    idle: Vec<Worker>,
    jobs: usize,
}

static POOL: Mutex<Pool> = Mutex::new(Pool {
    idle: Vec::new(),
    jobs: 0,
});
static AVAILABLE: Condvar = Condvar::new();

/// Outcomes since the start: made, refused, device removed, died.
static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

fn count(at: usize) {
    COUNTS[at].fetch_add(1, Ordering::Relaxed);
}

/// The watchdog's armed children: process handle → deadline.
static WATCHED: OnceLock<Mutex<HashMap<usize, Instant>>> = OnceLock::new();

fn watched() -> &'static Mutex<HashMap<usize, Instant>> {
    WATCHED.get_or_init(|| Mutex::new(HashMap::new()))
}

fn arm(process: RawHandle, deadline: Instant) {
    watched()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(process.0, deadline);
}

fn disarm(process: RawHandle) {
    watched()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&process.0);
}

/// Starts the watchdog thread once: it polls every second and kills (without
/// touching any pipe) a child whose make overran its deadline.
fn watchdog() {
    static ON: OnceLock<()> = OnceLock::new();
    ON.get_or_init(|| {
        if let Err(e) = std::thread::Builder::new()
            .name("victory-canary-watchdog".into())
            .spawn(|| loop {
                std::thread::sleep(Duration::from_secs(1));
                let now = Instant::now();
                let due: Vec<RawHandle> = {
                    let mut map = watched().lock().unwrap_or_else(|e| e.into_inner());
                    let mut due = Vec::new();
                    map.retain(|&h, d| {
                        if now >= *d {
                            due.push(RawHandle(h));
                            false
                        } else {
                            true
                        }
                    });
                    due
                };
                for handle in due {
                    // SAFETY: our own child's handle, opened with
                    // PROCESS_TERMINATE.
                    unsafe { TerminateProcess(handle.0 as HANDLE, TIMEOUT_EXIT) };
                }
            })
        {
            tracing::warn!(error = %e, "no canary watchdog");
        }
    });
}

/// The shared job object (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`), so a parent
/// crash or `TerminateProcess` leaves no orphaned canary children.
fn job_object() -> Option<RawHandle> {
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    static JOB: OnceLock<Option<RawHandle>> = OnceLock::new();
    *JOB.get_or_init(|| {
        // SAFETY: a plain job object creation.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            tracing::warn!("no canary job object: children may outlive a crash");
            return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: a live job handle and a whole structure.
        let set = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                std::mem::size_of_val(&info) as u32,
            )
        };
        if set == 0 {
            // SAFETY: our handle.
            unsafe { CloseHandle(job) };
            tracing::warn!("canary job object not limited: children may outlive a crash");
            return None;
        }
        Some(RawHandle(job as usize))
    })
}

/// Joins a child process to the shared job object.
fn assign_to_job(job: RawHandle, pid: u32) {
    use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };
    // SAFETY: opening a handle to our own child, with the rights the job needs.
    let h = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
    if h.is_null() {
        tracing::warn!("a canary child could not be joined to the job object");
        return;
    }
    // SAFETY: our job object and a live child process handle.
    if unsafe { AssignProcessToJobObject(job.0 as HANDLE, h) } == 0 {
        tracing::warn!("a canary child was not joined to the job object");
    }
    // SAFETY: our handle.
    unsafe { CloseHandle(h) };
}

/// Spawns one `victory canary-worker` child (below-normal priority).
fn spawn_worker() -> Option<Worker> {
    use std::os::windows::process::CommandExt;
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x4000;
    let exe = std::env::current_exe().ok()?;
    let mut child = Command::new(exe)
        .arg("canary-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(BELOW_NORMAL_PRIORITY_CLASS)
        .spawn()
        .ok()?;
    let stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    let process = {
        // SAFETY: opening a handle to our own child.
        let h = unsafe { OpenProcess(PROCESS_TERMINATE, 0, child.id()) };
        if h.is_null() {
            tracing::warn!("a canary child has no watchdog handle");
        }
        RawHandle(h as usize)
    };
    if let Some(job) = job_object() {
        assign_to_job(job, child.id());
    }
    Some(Worker {
        child: Some(child),
        stdin,
        stdout,
        process,
        made: 0,
        at: Instant::now(),
    })
}

fn decrement_jobs() {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    pool.jobs -= 1;
    drop(pool);
    AVAILABLE.notify_all();
}

fn return_worker(mut worker: Worker) {
    worker.at = Instant::now();
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    pool.idle.push(worker);
    drop(pool);
    AVAILABLE.notify_all();
}

/// Takes an idle worker, evicting long-idle ones and spawning as needed.
fn take_worker() -> Worker {
    watchdog();
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let idle = idle_s();
        if let Some(i) = pool.idle.iter().position(|w| w.at.elapsed() > idle) {
            let worker = pool.idle.remove(i);
            pool.jobs -= 1;
            drop(pool);
            worker.reap();
            pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
            continue;
        }
        if let Some(worker) = pool.idle.pop() {
            return worker;
        }
        if pool.jobs < jobs() {
            pool.jobs += 1;
            drop(pool);
            if let Some(worker) = spawn_worker() {
                return worker;
            }
            pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
            pool.jobs -= 1;
        }
        let next = AVAILABLE.wait(pool).unwrap_or_else(|e| e.into_inner());
        pool = next;
    }
}

/// One framed make on one child.
fn run_once(worker: &mut Worker, bytes: &[u8], fallback: Option<(&str, &str)>) -> Ended {
    let fallback = fallback
        .map(|(h, m)| format!("{h} {m}"))
        .unwrap_or_default();
    let mut request = Vec::with_capacity(12 + fallback.len() + bytes.len());
    request.extend_from_slice(MAGIC);
    request.extend_from_slice(&(fallback.len() as u32).to_le_bytes());
    request.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    request.extend_from_slice(fallback.as_bytes());
    request.extend_from_slice(bytes);

    let deadline = Instant::now() + timeout_s();
    arm(worker.process, deadline);
    let wrote = worker
        .stdin
        .write_all(&request)
        .and_then(|()| worker.stdin.flush());
    let mut response = [0u8; 5];
    let read = worker.stdout.read_exact(&mut response);
    disarm(worker.process);

    if wrote.is_err() || read.is_err() || &response[0..4] != MAGIC {
        return Ended::Died(worker.wait_code());
    }
    match response[4] {
        0 => {
            count(0);
            Ended::Made
        }
        1 => {
            count(1);
            Ended::Refused
        }
        2 => {
            count(2);
            Ended::DeviceRemoved
        }
        _ => Ended::Died(worker.wait_code()),
    }
}

/// Makes the pipeline through a canary child; `fallback` is `(hash, mnemonic)`.
/// A child whose device was removed is discarded and the make retried once on
/// a fresh worker.
pub(crate) fn make(bytes: &[u8], fallback: Option<(String, &'static str)>) -> Ended {
    for attempt in 0..2 {
        let mut worker = take_worker();
        let ended = run_once(
            &mut worker,
            bytes,
            fallback.as_ref().map(|(h, m)| (h.as_str(), *m)),
        );
        if matches!(ended, Ended::DeviceRemoved) && attempt == 0 {
            worker.reap();
            decrement_jobs();
            continue;
        }
        worker.made += 1;
        let recycle = worker.made >= max_jobs() || matches!(ended, Ended::Died(_));
        if recycle {
            worker.reap();
            decrement_jobs();
        } else {
            return_worker(worker);
        }
        return match ended {
            Ended::DeviceRemoved => Ended::Died(None),
            other => other,
        };
    }
    Ended::Died(None)
}

/// Whether this process's host device was removed.
fn device_removed() -> bool {
    let Some(host) = crate::host::host() else {
        return false;
    };
    // SAFETY: a plain query.
    unsafe { host.device.GetDeviceRemovedReason() }.is_err()
}

/// The `victory canary-worker` child: reads framed requests from stdin and
/// writes one framed response to stdout for each, until stdin closes. The app
/// calls [`super::known::use_translations`] first, so this process never opens
/// `host-pipelines.bin`.
pub fn run_worker() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut stdin = stdin.lock();
    let mut stdout = stdout.lock();
    loop {
        let mut head = [0u8; 12];
        if stdin.read_exact(&mut head).is_err() {
            break;
        }
        if &head[0..4] != MAGIC {
            break;
        }
        let fallback_len = u32::from_le_bytes(head[4..8].try_into().expect("4 bytes")) as usize;
        let payload_len = u32::from_le_bytes(head[8..12].try_into().expect("4 bytes")) as usize;
        let mut fallback = vec![0u8; fallback_len];
        let mut payload = vec![0u8; payload_len];
        if stdin.read_exact(&mut fallback).is_err() || stdin.read_exact(&mut payload).is_err() {
            break;
        }
        let fb = std::str::from_utf8(&fallback)
            .ok()
            .and_then(|s| s.split_once(' '));
        let made = known::make_pending_with(&payload, fb);
        let status: u8 = if made {
            0
        } else if device_removed() {
            2
        } else {
            1
        };
        let mut out = [0u8; 5];
        out[0..4].copy_from_slice(MAGIC);
        out[4] = status;
        if stdout.write_all(&out).is_err() || stdout.flush().is_err() {
            break;
        }
    }
    std::process::exit(0);
}
