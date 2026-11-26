//! The wrappers around what threads wait and wake each other with. The
//! title's imports are bound to them ([`import`]), and the entries of our
//! own import table point at them, if a timeline was asked for; else nothing
//! is wrapped. While nothing is recorded a wrapper is a load and a jump.

use std::ffi::{CStr, c_void};
use std::sync::Once;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::record::{kernel, on, standing, stood_at, ticks, waited, woken};

type Handle = *mut c_void;

macro_rules! wrapped {
    ($($index:literal $name:ident ($($arg:ident: $type:ty),*) $(-> $result:ty)? => $body:expr;)*) => {
        /// The functions wrapped, by name: their own addresses, ours.
        static TABLE: [(&str, AtomicUsize, usize); [$($index),*].len()] = [
            $((stringify!($name), AtomicUsize::new(0), $index)),*
        ];

        fn wrapper(index: usize) -> usize {
            match index {
                $($index => wrappers::$name as *const () as usize,)*
                _ => 0,
            }
        }

        #[allow(non_snake_case)]
        mod wrappers {
            use super::*;
            $(
                pub(super) unsafe extern "system" fn $name($($arg: $type),*) $(-> $result)? {
                    // SAFETY: the function of that name, as the system has it
                    // (or the module the title imports it from).
                    let real: unsafe extern "system" fn($($type),*) $(-> $result)? =
                        unsafe { std::mem::transmute(TABLE[$index].1.load(Ordering::Relaxed)) };
                    if !on() {
                        // SAFETY: the caller's own call.
                        return unsafe { real($($arg),*) };
                    }
                    ($body)(real)
                }
            )*
        }
    };
}

/// Runs `call`, a wait on `object` that may be long; what the caller may
/// ask [`GetLastError`] about stays.
///
/// [`GetLastError`]: windows_sys::Win32::Foundation::GetLastError
fn wait<T>(name: &'static str, object: u64, call: impl FnOnce() -> T) -> T {
    use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
    let t0 = ticks();
    let result = {
        let _standing = standing(object);
        call()
    };
    // SAFETY: plain queries of this thread's own.
    unsafe {
        let error = GetLastError();
        waited(name, object, t0);
        SetLastError(error);
    }
    result
}

/// Runs `call`, a call of the kernel's on `bytes`.
fn measured<T>(name: &'static str, bytes: usize, call: impl FnOnce() -> T) -> T {
    use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
    let t0 = ticks();
    let result = call();
    // SAFETY: plain queries of this thread's own.
    unsafe {
        let error = GetLastError();
        kernel(name, bytes as u64, t0);
        SetLastError(error);
    }
    result
}

/// The function `name` of the table, as the system has it.
fn real(name: &str) -> usize {
    TABLE.iter().find(|t| t.0 == name).map_or(0, |t| t.1.load(Ordering::Relaxed))
}

/// `take(lock)`, a try that does not wait, then `wait`.
fn lock_of(name: &'static str, lock: Handle, take: &str, call: impl FnOnce()) {
    let take = real(take);
    if take != 0 {
        // SAFETY: `TryEnterCriticalSection` or a `TryAcquireSRWLock` call:
        // one pointer, a truth value (one byte of it for the latter).
        let taken = unsafe { std::mem::transmute::<usize, unsafe extern "system" fn(Handle) -> u8>(take)(lock) };
        if taken != 0 {
            return;
        }
    }
    wait(name, lock as u64, call);
}

fn released(name: &'static str, object: Handle) {
    if stood_at(object as u64) {
        woken(name, object as u64);
    }
}

wrapped! {
    0 WaitForSingleObject(object: Handle, ms: u32) -> u32 => |real: unsafe extern "system" fn(Handle, u32) -> u32| {
        // SAFETY: the caller's own call (so below).
        wait("WaitForSingleObject", object as u64, || unsafe { real(object, ms) })
    };
    1 WaitForSingleObjectEx(object: Handle, ms: u32, alertable: i32) -> u32 => |real: unsafe extern "system" fn(Handle, u32, i32) -> u32| {
        wait("WaitForSingleObjectEx", object as u64, || unsafe { real(object, ms, alertable) })
    };
    2 Sleep(ms: u32) => |real: unsafe extern "system" fn(u32)| {
        wait("Sleep", 0, || unsafe { real(ms) })
    };
    3 SleepEx(ms: u32, alertable: i32) -> u32 => |real: unsafe extern "system" fn(u32, i32) -> u32| {
        wait("SleepEx", 0, || unsafe { real(ms, alertable) })
    };
    4 SetEvent(object: Handle) -> i32 => |real: unsafe extern "system" fn(Handle) -> i32| {
        woken("SetEvent", object as u64);
        unsafe { real(object) }
    };
    5 ReleaseSemaphore(object: Handle, count: i32, before: *mut i32) -> i32 => |real: unsafe extern "system" fn(Handle, i32, *mut i32) -> i32| {
        woken("ReleaseSemaphore", object as u64);
        unsafe { real(object, count, before) }
    };
    6 ReleaseMutex(object: Handle) -> i32 => |real: unsafe extern "system" fn(Handle) -> i32| {
        woken("ReleaseMutex", object as u64);
        unsafe { real(object) }
    };
    7 EnterCriticalSection(lock: Handle) => |real: unsafe extern "system" fn(Handle)| {
        lock_of("EnterCriticalSection", lock, "TryEnterCriticalSection", || unsafe { real(lock) })
    };
    8 LeaveCriticalSection(lock: Handle) => |real: unsafe extern "system" fn(Handle)| {
        released("LeaveCriticalSection", lock);
        unsafe { real(lock) }
    };
    9 AcquireSRWLockExclusive(lock: Handle) => |real: unsafe extern "system" fn(Handle)| {
        lock_of("AcquireSRWLockExclusive", lock, "TryAcquireSRWLockExclusive", || unsafe { real(lock) })
    };
    10 ReleaseSRWLockExclusive(lock: Handle) => |real: unsafe extern "system" fn(Handle)| {
        released("ReleaseSRWLockExclusive", lock);
        unsafe { real(lock) }
    };
    11 AcquireSRWLockShared(lock: Handle) => |real: unsafe extern "system" fn(Handle)| {
        lock_of("AcquireSRWLockShared", lock, "TryAcquireSRWLockShared", || unsafe { real(lock) })
    };
    12 ReleaseSRWLockShared(lock: Handle) => |real: unsafe extern "system" fn(Handle)| {
        released("ReleaseSRWLockShared", lock);
        unsafe { real(lock) }
    };
    13 WaitOnAddress(at: Handle, compare: Handle, size: usize, ms: u32) -> i32 => |real: unsafe extern "system" fn(Handle, Handle, usize, u32) -> i32| {
        wait("WaitOnAddress", at as u64, || unsafe { real(at, compare, size, ms) })
    };
    14 WakeByAddressSingle(at: Handle) => |real: unsafe extern "system" fn(Handle)| {
        released("WakeByAddressSingle", at);
        unsafe { real(at) }
    };
    15 WakeByAddressAll(at: Handle) => |real: unsafe extern "system" fn(Handle)| {
        released("WakeByAddressAll", at);
        unsafe { real(at) }
    };
    16 SleepConditionVariableSRW(at: Handle, lock: Handle, ms: u32, flags: u32) -> i32 => |real: unsafe extern "system" fn(Handle, Handle, u32, u32) -> i32| {
        wait("SleepConditionVariableSRW", at as u64, || unsafe { real(at, lock, ms, flags) })
    };
    17 SleepConditionVariableCS(at: Handle, lock: Handle, ms: u32) -> i32 => |real: unsafe extern "system" fn(Handle, Handle, u32) -> i32| {
        wait("SleepConditionVariableCS", at as u64, || unsafe { real(at, lock, ms) })
    };
    18 WakeConditionVariable(at: Handle) => |real: unsafe extern "system" fn(Handle)| {
        released("WakeConditionVariable", at);
        unsafe { real(at) }
    };
    19 WakeAllConditionVariable(at: Handle) => |real: unsafe extern "system" fn(Handle)| {
        released("WakeAllConditionVariable", at);
        unsafe { real(at) }
    };
    20 WaitForMultipleObjects(count: u32, objects: *const Handle, all: i32, ms: u32) -> u32 => |real: unsafe extern "system" fn(u32, *const Handle, i32, u32) -> u32| {
        // (Named by the first of them.)
        // SAFETY: the caller passes `count` handles.
        let first = if count > 0 && !objects.is_null() { (unsafe { *objects }) as u64 } else { 0 };
        wait("WaitForMultipleObjects", first, || unsafe { real(count, objects, all, ms) })
    };
    21 GetWriteWatch(flags: u32, at: Handle, size: usize, found: Handle, count: Handle, granularity: Handle) -> u32 => |real: unsafe extern "system" fn(u32, Handle, usize, Handle, Handle, Handle) -> u32| {
        measured("GetWriteWatch", size, || unsafe { real(flags, at, size, found, count, granularity) })
    };
    22 VirtualProtect(at: Handle, size: usize, new: u32, old: Handle) -> i32 => |real: unsafe extern "system" fn(Handle, usize, u32, Handle) -> i32| {
        measured("VirtualProtect", size, || unsafe { real(at, size, new, old) })
    };
    23 ReadProcessMemory(process: Handle, at: Handle, to: Handle, size: usize, done: Handle) -> i32 => |real: unsafe extern "system" fn(Handle, Handle, Handle, usize, Handle) -> i32| {
        measured("ReadProcessMemory", size, || unsafe { real(process, at, to, size, done) })
    };
    24 VirtualQuery(at: Handle, info: Handle, size: usize) -> usize => |real: unsafe extern "system" fn(Handle, Handle, usize) -> usize| {
        measured("VirtualQuery", 0, || unsafe { real(at, info, size) })
    };
    25 VirtualAlloc(at: Handle, size: usize, kind: u32, protect: u32) -> Handle => |real: unsafe extern "system" fn(Handle, usize, u32, u32) -> Handle| {
        measured("VirtualAlloc", size, || unsafe { real(at, size, kind, protect) })
    };
    26 VirtualFree(at: Handle, size: usize, kind: u32) -> i32 => |real: unsafe extern "system" fn(Handle, usize, u32) -> i32| {
        measured("VirtualFree", size, || unsafe { real(at, size, kind) })
    };
    27 _Mtx_lock(lock: Handle) -> i32 => |real: unsafe extern "system" fn(Handle) -> i32| {
        wait("_Mtx_lock", lock as u64, || unsafe { real(lock) })
    };
    28 _Mtx_unlock(lock: Handle) -> i32 => |real: unsafe extern "system" fn(Handle) -> i32| {
        released("_Mtx_unlock", lock);
        unsafe { real(lock) }
    };
    29 TryEnterCriticalSection(lock: Handle) -> u8 => |real: unsafe extern "system" fn(Handle) -> u8| unsafe { real(lock) };
    30 TryAcquireSRWLockExclusive(lock: Handle) -> u8 => |real: unsafe extern "system" fn(Handle) -> u8| unsafe { real(lock) };
    31 TryAcquireSRWLockShared(lock: Handle) -> u8 => |real: unsafe extern "system" fn(Handle) -> u8| unsafe { real(lock) };
}

/// The wrappers from here on are there for their real functions only.
const ONLY_REAL: usize = 29;

/// Asks the system for the real functions, and points our own import table
/// at the wrappers: once, if a timeline is wanted.
fn arm() -> bool {
    static ONCE: Once = Once::new();
    if !super::record::wanted() {
        return false;
    }
    ONCE.call_once(wrap_own);
    true
}

/// See [`arm`].
pub(super) fn wrap_own() {
    find_real();
    // SAFETY: our own module's mapped image.
    let changed = unsafe { wrap_imports() };
    tracing::info!(changed, "the timeline's wrappers are in (VICTORY_TIMELINE)");
}

fn find_real() {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    for dll in [c"kernelbase.dll", c"kernel32.dll", c"ntdll.dll"] {
        // SAFETY: NUL-terminated names of modules every process has.
        let module = unsafe { GetModuleHandleA(dll.as_ptr().cast()) };
        for (name, at, _) in &TABLE {
            if at.load(Ordering::Relaxed) == 0 && !module.is_null() {
                let text = std::ffi::CString::new(*name).expect("a plain name");
                // SAFETY: as above.
                if let Some(f) = unsafe { GetProcAddress(module, text.as_ptr().cast()) } {
                    at.store(f as *const () as usize, Ordering::Relaxed);
                }
            }
        }
    }
}

/// `VICTORY_SLEEP_COUNT=1`: the title's `Sleep`, `SleepEx` and
/// `SwitchToThread` are bound to counting wrappers for the whole run (the
/// timeline's wrappers then leave those three alone). A zero-millisecond
/// sleep gives the rest of the time slice to a thread ready on that
/// processor, or returns at once: each is timed, and every five seconds a
/// line says per frame how many there were, the time inside them, how many
/// waited long (another thread ran meanwhile), and the threads that made
/// most of them (ids as `VICTORY_PROFILE` names threads). At the first bind,
/// how long one `pause` takes on this processor: the title's `mwaitx` spin
/// loops are patched to it (`victory_loader::console_insn`).
mod sleeps {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
    use std::sync::{Mutex, Once, OnceLock};
    use std::time::{Duration, Instant};

    fn on() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("VICTORY_SLEEP_COUNT").is_some_and(|v| v == "1"))
    }

    /// The real `Sleep`, `SleepEx`, `SwitchToThread`.
    static REAL: [AtomicUsize; 3] = [const { AtomicUsize::new(0) }; 3];
    /// Zero-millisecond sleeps by how long the call took (under 10 µs, under
    /// 100 µs, under 1 ms, longer): calls, ns.
    static ZERO: [[AtomicU64; 2]; 4] = [const { [const { AtomicU64::new(0) }; 2] }; 4];
    /// All zero-millisecond sleeps, for when a line is looked for.
    static CALLS: AtomicU64 = AtomicU64::new(0);
    /// Sleeps of a millisecond or more: calls, ns.
    static OTHER: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
    /// `SwitchToThread`: calls, ns, those that gave the processor away.
    static SWITCH: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
    /// Zero-millisecond sleeps by thread: id, calls, ns.
    static THREADS: [[AtomicU64; 3]; 64] = [const { [const { AtomicU64::new(0) }; 3] }; 64];

    /// Our wrapper for the title's import `name`, whose real function is at
    /// `address`; `None` if it is not counted.
    pub(super) fn bind(name: &str, address: usize) -> Option<usize> {
        if !on() || address == 0 {
            return None;
        }
        let (i, ours) = match name {
            "Sleep" => (0, sleep as *const () as usize),
            "SleepEx" => (1, sleep_ex as *const () as usize),
            "SwitchToThread" => (2, switch as *const () as usize),
            _ => return None,
        };
        REAL[i].store(address, Relaxed);
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            tracing::info!(
                pause_ns = format_args!("{:.1}", pause_ns()),
                "the title's Sleep, SleepEx and SwitchToThread are counted (VICTORY_SLEEP_COUNT); pause_ns: one pause on this processor"
            );
        });
        tracing::info!(name, "a title import counted (VICTORY_SLEEP_COUNT)");
        Some(ours)
    }

    /// One `pause`, ns: the best of five runs of 100,000.
    fn pause_ns() -> f64 {
        (0..5)
            .map(|_| {
                let t = Instant::now();
                for _ in 0..100_000 {
                    std::hint::spin_loop();
                }
                t.elapsed().as_secs_f64() * 1e9 / 100_000.0
            })
            .fold(f64::INFINITY, f64::min)
    }

    unsafe extern "system" fn sleep(ms: u32) {
        // SAFETY: the real function, stored before this wrapper was handed out.
        let real = unsafe { std::mem::transmute::<usize, unsafe extern "system" fn(u32)>(REAL[0].load(Relaxed)) };
        let t0 = Instant::now();
        // SAFETY: the caller's own call.
        unsafe { real(ms) };
        done(ms, t0.elapsed());
    }

    unsafe extern "system" fn sleep_ex(ms: u32, alertable: i32) -> u32 {
        // SAFETY: as in `sleep`.
        let real = unsafe { std::mem::transmute::<usize, unsafe extern "system" fn(u32, i32) -> u32>(REAL[1].load(Relaxed)) };
        let t0 = Instant::now();
        // SAFETY: the caller's own call.
        let result = unsafe { real(ms, alertable) };
        done(ms, t0.elapsed());
        result
    }

    unsafe extern "system" fn switch() -> i32 {
        // SAFETY: as in `sleep`.
        let real = unsafe { std::mem::transmute::<usize, unsafe extern "system" fn() -> i32>(REAL[2].load(Relaxed)) };
        let t0 = Instant::now();
        // SAFETY: the caller's own call.
        let result = unsafe { real() };
        let ns = t0.elapsed().as_nanos() as u64;
        SWITCH[0].fetch_add(1, Relaxed);
        SWITCH[1].fetch_add(ns, Relaxed);
        SWITCH[2].fetch_add(u64::from(result != 0), Relaxed);
        result
    }

    fn done(ms: u32, took: Duration) {
        use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
        // SAFETY: plain queries of this thread's own.
        let error = unsafe { GetLastError() };
        let ns = took.as_nanos() as u64;
        if ms == 0 {
            let bucket = if ns < 10_000 {
                0
            } else if ns < 100_000 {
                1
            } else if ns < 1_000_000 {
                2
            } else {
                3
            };
            ZERO[bucket][0].fetch_add(1, Relaxed);
            ZERO[bucket][1].fetch_add(ns, Relaxed);
            // SAFETY: a plain query.
            let id = u64::from(unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() });
            let mut i = (id as usize).wrapping_mul(0x9E37_79B9) % THREADS.len();
            for _ in 0..THREADS.len() {
                let t = &THREADS[i];
                let there = t[0].compare_exchange(0, id, Relaxed, Relaxed).unwrap_or_else(|there| there);
                if there == 0 || there == id {
                    t[1].fetch_add(1, Relaxed);
                    t[2].fetch_add(ns, Relaxed);
                    break;
                }
                i = (i + 1) % THREADS.len();
            }
            if CALLS.fetch_add(1, Relaxed) % 4096 == 0 {
                maybe_line();
            }
        } else {
            OTHER[0].fetch_add(1, Relaxed);
            OTHER[1].fetch_add(ns, Relaxed);
            maybe_line();
        }
        // SAFETY: as above.
        unsafe { SetLastError(error) };
    }

    /// A line if five seconds have passed since the last; never waited for.
    fn maybe_line() {
        static LAST: Mutex<Option<(Instant, u64)>> = Mutex::new(None);
        let Ok(mut last) = LAST.try_lock() else { return };
        let (now, frame) = (Instant::now(), crate::frame::current());
        let (since, from) = *last.get_or_insert((now, frame));
        if now.duration_since(since) < Duration::from_secs(5) {
            return;
        }
        *last = Some((now, frame));
        drop(last);
        line(now.duration_since(since), frame.saturating_sub(from));
    }

    fn line(took: Duration, frames: u64) {
        let f = frames.max(1) as f64;
        let take = |a: &AtomicU64| a.swap(0, Relaxed);
        let z: Vec<(u64, u64)> = ZERO.iter().map(|b| (take(&b[0]), take(&b[1]))).collect();
        let calls: u64 = z.iter().map(|b| b.0).sum();
        let ns: u64 = z.iter().map(|b| b.1).sum();
        let (other_calls, other_ns) = (take(&OTHER[0]), take(&OTHER[1]));
        let (switches, switch_ns, yielded) = (take(&SWITCH[0]), take(&SWITCH[1]), take(&SWITCH[2]));
        let mut threads: Vec<(u64, u64, u64)> = THREADS
            .iter()
            .map(|t| (t[0].load(Relaxed), take(&t[1]), take(&t[2])))
            .filter(|t| t.1 > 0)
            .collect();
        threads.sort_unstable_by_key(|t| std::cmp::Reverse(t.2));
        let top: Vec<String> = threads
            .iter()
            .take(4)
            .map(|t| format!("thread {} {:.0} calls {:.2} ms", t.0, t.1 as f64 / f, t.2 as f64 / 1e6 / f))
            .collect();
        tracing::info!(
            "Sleep(0) per frame ({} frames in {:.1} s): {:.0} calls, {:.2} ms inside ({:.2} us each); by length: {:.0} under 10 us, {:.0} under 100 us, {:.1} under 1 ms, {:.2} longer ({:.2} ms of them); by thread: {}; longer sleeps {:.1} calls {:.2} ms; SwitchToThread {:.0} calls {:.2} ms, {:.0} gave the processor away (VICTORY_SLEEP_COUNT)",
            frames,
            took.as_secs_f64(),
            calls as f64 / f,
            ns as f64 / 1e6 / f,
            if calls > 0 { ns as f64 / 1e3 / calls as f64 } else { 0.0 },
            z[0].0 as f64 / f,
            z[1].0 as f64 / f,
            z[2].0 as f64 / f,
            z[3].0 as f64 / f,
            z[3].1 as f64 / 1e6 / f,
            top.join(", "),
            other_calls as f64 / f,
            other_ns as f64 / 1e6 / f,
            switches as f64 / f,
            switch_ns as f64 / 1e6 / f,
            yielded as f64 / f
        );
    }
}

/// What the title's import `name`, found at `address`, is bound to: the
/// address itself, or our wrapper of it.
pub fn import(name: &str, address: usize) -> usize {
    if let Some(ours) = sleeps::bind(name, address) {
        return ours;
    }
    if !arm() {
        return address;
    }
    let Some((i, (_, at, _))) = TABLE.iter().enumerate().find(|(i, t)| *i < ONLY_REAL && t.0 == name) else {
        return address;
    };
    if at.load(Ordering::Relaxed) == 0 {
        at.store(address, Ordering::Relaxed);
    }
    wrapper(i)
}

/// Points the entries of our own import table that are in [`TABLE`] at the
/// wrappers: how many.
///
/// # Safety
/// The process's module is mapped as the loader left it.
unsafe fn wrap_imports() -> usize {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
    // SAFETY: a plain query.
    let base = unsafe { GetModuleHandleW(std::ptr::null()) } as usize;
    let mut changed = 0;
    // SAFETY: the headers, the import directory and the tables it names lie
    // in the mapped image, as the loader read them; an entry is eight bytes,
    // aligned, and made writable for the store.
    unsafe {
        let at = |rva: u32| base + rva as usize;
        let read = |at: usize| (at as *const u32).read_unaligned();
        let headers = at(read(base + 0x3c));
        // The second data directory of the 64-bit optional header: imports.
        let mut descriptor = at(read(headers + 0x18 + 0x70 + 8));
        loop {
            let (names, first) = (read(descriptor), read(descriptor + 16));
            if first == 0 {
                break;
            }
            let names = if names != 0 { names } else { first };
            for i in 0.. {
                let entry = (at(names) as *const u64).add(i).read_unaligned();
                if entry == 0 {
                    break;
                }
                if entry >> 63 != 0 {
                    continue; // by number
                }
                let name = CStr::from_ptr((at(entry as u32) + 2) as *const _);
                let Some(index) = TABLE[..ONLY_REAL].iter().position(|t| t.0.as_bytes() == name.to_bytes()) else {
                    continue;
                };
                if TABLE[index].1.load(Ordering::Relaxed) == 0 {
                    continue;
                }
                let slot = (at(first) as *mut usize).add(i);
                let (mut old, mut ignored) = (0, 0);
                if VirtualProtect(slot.cast(), 8, PAGE_READWRITE, &mut old) != 0 {
                    (*slot.cast::<AtomicUsize>()).store(wrapper(index), Ordering::SeqCst);
                    VirtualProtect(slot.cast(), 8, old, &mut ignored);
                    changed += 1;
                }
            }
            descriptor += 20;
        }
    }
    changed
}
