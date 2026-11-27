//! Research (native keyboard): give the title's keyboard events a listener.
//!
//! Each RAGE input device broadcasts its events (`{source = code << 8 |
//! device, type 7 pressed / 11 released / 3 axis, 0, value}`) to the
//! subscribers on a list at device+8: nodes `{object, handler, next}`,
//! null-terminated. The title subscribes one handler (an event buffer at
//! title +0x41c94d0) to the first pad and none to the keyboard (registry
//! slot 0), so key events, made in full by the keyboard device, go nowhere.
//!
//! `VICTORY_KEYBOARD_EVENTS=1` subscribes that same buffer to the keyboard
//! through [`relay`], which logs the events and whether the buffer took them
//! (its count at +0x26b8 grew), then lets the title's handler have them;
//! `=log` only logs. Nothing changes without the switch.
//!
//! `VICTORY_KEY_BINDS=<action>:<key>:<flags>,...` (hex: action id = joaat of its
//! `INPUT_*` name, Windows virtual key, record flags) adds a keyboard record to
//! each action's bindings as soon as the actions exist (input system
//! 0x14ab77b90: actions of 0x160 bytes, count at +0x44, array at +0x48; id at
//! +0x128; entry +0x10 -> `{capacity, count, records}`, records of 12 bytes
//! `{source, second source or 0xf000, flags}`). An action links its sources
//! when it activates (title +0x1d67c0), and the title rebuilds these arrays
//! from its data and frees them (e.g. around the pause menu): the records go
//! in from a hook on that activation, every time, in blocks of the title's own
//! allocator (title +0x4d9c140), on the title's thread.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};

/// Registry slots 0 (keyboard) and 1 (first pad), as offsets in the image.
const KEYBOARD_SLOT: usize = 0x9ab_14c8;
const PAD_SLOT: usize = 0x9ab_14d0;
/// The title's event buffer handler, expected on the pad's list.
const BUFFER_HANDLER: usize = 0x41c_94d0;
/// The buffer's event count (u16) in its object.
const BUFFER_COUNT: usize = 0x26b8;

type Handler = unsafe extern "system" fn(*mut c_void, *const Event);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Event {
    source: u32,
    kind: u32,
    extra: u32,
    value: f32,
}

/// What [`relay`] is given as its object.
#[repr(C)]
struct Relay {
    object: *mut c_void,
    handler: Handler,
    forward: bool,
}

static SEEN: AtomicU64 = AtomicU64::new(0);
static TAKEN: AtomicU64 = AtomicU64::new(0);

unsafe extern "system" fn relay(me: *mut c_void, event: *const Event) {
    // SAFETY: `me` is our leaked `Relay`; `event` the title's, for this call.
    let (me, e) = unsafe { (&*me.cast::<Relay>(), *event) };
    let n = SEEN.fetch_add(1, Ordering::Relaxed) + 1;
    if !me.forward {
        if n <= 64 || n.is_power_of_two() {
            tracing::info!(target: "keyboard_events", n, source = format_args!("{:#x}", e.source), key = format_args!("{:#x}", e.source >> 8), kind = e.kind, value = e.value, "keyboard event (not forwarded)");
        }
        return;
    }
    // SAFETY: on the title's thread, with the title's handler and object.
    unsafe { translate(me, &e) };
    let count = (me.object as usize + BUFFER_COUNT) as *const u16;
    // SAFETY: the buffer object's count; read without its lock, for the log only.
    let before = unsafe { count.read_volatile() };
    // SAFETY: the title's handler with its own object and the event it was given.
    unsafe { (me.handler)(me.object, event) };
    // SAFETY: as above.
    let after = unsafe { count.read_volatile() };
    let took = after != before;
    let taken = if took { TAKEN.fetch_add(1, Ordering::Relaxed) + 1 } else { TAKEN.load(Ordering::Relaxed) };
    if n <= 64 || n.is_power_of_two() {
        tracing::info!(target: "keyboard_events", n, taken, took, source = format_args!("{:#x}", e.source), key = format_args!("{:#x}", e.source >> 8), kind = e.kind, value = e.value, "keyboard event to the title's buffer");
    }
}

/// The input system's actions (count, array), as offsets in the image.
const ACTION_COUNT: usize = 0xab7_7bd4;
const ACTION_ARRAY: usize = 0xab7_7bd8;
/// The first pad's index in the device registry: the low byte of its events' source.
const PAD_INDEX: u32 = 1;

/// A key, the actions it stands for in play, and those it stands for in menus (in order of
/// preference; ids = joaat of their `INPUT_*` names).
type KeyEntry = (u32, Vec<u32>, Vec<u32>);

/// The keys' actions: from `VICTORY_NATIVE_KEYS` (read again when the file changes, looked at at
/// most once a second) or `VICTORY_KEY_ACTIONS`.
struct KeyMap {
    file: Option<std::path::PathBuf>,
    modified: Option<std::time::SystemTime>,
    looked: Option<std::time::Instant>,
    entries: Vec<KeyEntry>,
}

static KEY_MAP: std::sync::Mutex<KeyMap> =
    std::sync::Mutex::new(KeyMap { file: None, modified: None, looked: None, entries: Vec::new() });

impl KeyMap {
    /// Reads the file again if it changed since it was read.
    fn refresh(&mut self) {
        let Some(file) = self.file.clone() else { return };
        let now = std::time::Instant::now();
        if self.looked.is_some_and(|l| now.duration_since(l) < std::time::Duration::from_secs(1)) {
            return;
        }
        self.looked = Some(now);
        let modified = std::fs::metadata(&file).and_then(|m| m.modified()).ok();
        if modified == self.modified && !self.entries.is_empty() {
            return;
        }
        match std::fs::read_to_string(&file) {
            Ok(text) => {
                self.entries = parse_native_keys(&text);
                self.modified = modified;
                tracing::info!(target: "keyboard_events", keys = self.entries.len(), file = %file.display(), "native keys read: each key stands for the game's own actions");
            }
            Err(e) => tracing::warn!(target: "keyboard_events", error = %e, "native keys file not read"),
        }
    }
}

/// The action whose activity tells play from a menu: on foot and in vehicles it is active (+0x158
/// bit 2); the pause menu and the like turn it off (seen 2026-11-27: 0x0a in play, 0x08 in a menu).
const PLAY_ACTION: u32 = 0x8fd0_15d8; // INPUT_MOVE_UP_ONLY
/// Actions active only on the Map screen (inactive on foot; bit 2 set when map is open).
const MAP_MOVE_ACTION: u32 = 0x6140_af6c; // INPUT_FRONTEND_MAP_MOVE_LR
const MAP_ACCEPT_ACTION: u32 = 0x8ddd_c16e; // INPUT_MAP_MENU_ACCEPT
/// Action active only in vehicles (used to seamlessly re-translate held keys when entering/exiting).
const VEH_ACCEL_ACTION: u32 = 0x5b9f_d4e2; // INPUT_VEH_ACCELERATE
/// Actions active only while a radial wheel (Weapon / Item / Emote Wheel) is open.
const RADIAL_SLOT_NEXT: u32 = 0xe71f_89b8; // INPUT_RADIAL_MENU_SLOT_NAV_NEXT
const RADIAL_SLOT_PREV: u32 = 0x93d6_723f; // INPUT_RADIAL_MENU_SLOT_NAV_PREV
const SELECT_NEXT_WHEEL: u32 = 0x77e5_6fb3; // INPUT_SELECT_NEXT_WHEEL
/// Actions that open a radial wheel when held.
const OPEN_WHEEL_ACTION: u32 = 0xac4b_d4f1; // INPUT_OPEN_WHEEL_MENU (Tab)
const OPEN_EMOTE_WHEEL: u32 = 0xe2b5_57a3; // INPUT_OPEN_EMOTE_WHEEL
const SELECT_ITEM_WHEEL: u32 = 0x1f6d_95e5; // INPUT_SELECT_ITEM_WHEEL
const VEH_RADIO_ACTION: u32 = 0x22e0_f7e7; // INPUT_VEH_NEXT_RADIO (hold Q in car = Radio Wheel)
/// Vertex meta-actions for on-foot walk speed (scales LX/LY to 0.42 while walking).
const WALK_TOGGLE: u32 = joaat("VERTEX_WALK_TOGGLE");
const WALK_HOLD: u32 = joaat("VERTEX_WALK_HOLD");
const WALK_SCALE: f32 = 0.42;

/// The title is in a menu (pause menu, map...): [`PLAY_ACTION`] inactive, polled every 50 ms
/// while native keys are on.
pub static IN_MENU: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The title is on the Map screen inside menus: mouse motion smoothly pans the map cursor
/// (left stick) and mouse wheel zooms (RT/LT).
pub static IN_MAP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A radial wheel (Weapon Wheel / Radio Wheel) is open in play: mouse motion drives a held
/// radial right-stick angle instead of velocity-based camera look.
pub static IN_WHEEL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The player is in a vehicle (used for smooth steering ramp and context re-translation).
pub static IN_VEHICLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// On-foot walk modifier (scales LX/LY to [`WALK_SCALE`]; auto-clears in menus/vehicles).
static WALKING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Currently held wheel-open key `(vk, pressed_at, activation_delay)`.
static WHEEL_HOLD: std::sync::Mutex<Option<(u32, std::time::Instant, std::time::Duration)>> =
    std::sync::Mutex::new(None);

/// Smooth analog steering ramp for A/D (`LX`, 0x3010) in vehicles so quick highway taps steer
/// gently while holding hits full lock. Configured via `VICTORY_STEER_RAMP_MS` (default 110; 0 off).
struct SteerRamp {
    current: f32,
    target: f32,
    active: bool,
    last: Option<std::time::Instant>,
    ramp_ms: f32,
}

impl SteerRamp {
    const INITIAL_BITE: f32 = 0.28;

    const fn new(ramp_ms: f32) -> Self {
        Self { current: 0.0, target: 0.0, active: false, last: None, ramp_ms }
    }

    fn configured_ms() -> f32 {
        static MS: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
        *MS.get_or_init(|| {
            std::env::var("VICTORY_STEER_RAMP_MS")
                .ok()
                .and_then(|v| v.trim().parse::<f32>().ok())
                .filter(|v| v.is_finite() && *v >= 0.0)
                .unwrap_or(110.0)
        })
    }

    /// Sets a new steering target (`-1.0..=1.0`). Returns the immediate value to emit now.
    fn set_target(&mut self, target: f32, now: std::time::Instant, ramp_ms: f32) -> f32 {
        self.ramp_ms = ramp_ms;
        self.target = target.clamp(-1.0, 1.0);
        if self.target.abs() < 1e-4 || ramp_ms <= 0.0 {
            self.current = self.target;
            self.active = false;
            self.last = None;
            return self.current;
        }
        if self.current * self.target <= 0.0 {
            self.current = Self::INITIAL_BITE * self.target.signum();
        }
        self.active = (self.target - self.current).abs() > 1e-3;
        self.last = self.active.then_some(now);
        self.current
    }

    /// Advances an active ramp toward `target`, returning `Some(new_value)` if it changed.
    fn tick(&mut self, now: std::time::Instant) -> Option<f32> {
        if !self.active || self.ramp_ms <= 0.0 {
            return None;
        }
        let dt_s = self.last.map_or(0.016, |t| now.saturating_duration_since(t).as_secs_f32().clamp(0.001, 0.1));
        self.last = Some(now);
        let rate = (1.0 - Self::INITIAL_BITE) / (self.ramp_ms * 0.001);
        let diff = self.target - self.current;
        let step = rate * dt_s;
        if diff.abs() <= step {
            self.current = self.target;
            self.active = false;
            self.last = None;
        } else {
            self.current += diff.signum() * step;
        }
        Some(self.current)
    }

    fn reset(&mut self) {
        self.current = 0.0;
        self.target = 0.0;
        self.active = false;
        self.last = None;
    }
}

static STEER_RAMP: std::sync::Mutex<SteerRamp> = std::sync::Mutex::new(SteerRamp::new(110.0));

/// Target `(object, handler)` once subscribed to the title's event buffer, for context transitions.
static RELAY_TARGET: std::sync::Mutex<Option<(usize, Handler)>> = std::sync::Mutex::new(None);

fn wheel_hold_active(now: std::time::Instant) -> bool {
    let hold = WHEEL_HOLD.lock().unwrap_or_else(|p| p.into_inner());
    hold.is_some_and(|(_, since, delay)| now.saturating_duration_since(since) >= delay)
}

/// Looks up an action pointer by id in the live action array (read-only).
fn find_action(base: usize, id: u32, cached: &mut usize) -> usize {
    let read32 = |at: usize| unsafe { std::ptr::read_volatile(at as *const u32) };
    let read = |at: usize| unsafe { std::ptr::read_volatile(at as *const usize) };
    let count = read32(base + ACTION_COUNT) as usize;
    let array = read(base + ACTION_ARRAY);
    if count == 0 || array == 0 {
        return 0;
    }
    if *cached == 0 || read32(*cached + 0x128) != id {
        *cached = (0..count).map(|i| array + i * 0x160).find(|&a| read32(a + 0x128) == id).unwrap_or(0);
    }
    *cached
}

fn action_active(action: usize) -> bool {
    action != 0 && unsafe { std::ptr::read_volatile((action + 0x158) as *const u32) } & 2 != 0
}

/// Keeps [`IN_MENU`], [`IN_MAP`], [`IN_WHEEL`], and [`IN_VEHICLE`] current, drives smooth vehicle
/// steering ramps, and cleanly updates held keys across context switches.
fn watch_menus(base: usize) {
    let mut play_ptr = 0usize;
    let mut map_move_ptr = 0usize;
    let mut map_acc_ptr = 0usize;
    let mut veh_ptr = 0usize;
    let mut rad_next_ptr = 0usize;
    let mut rad_prev_ptr = 0usize;
    let mut rad_tab_ptr = 0usize;
    let mut was_veh = false;
    loop {
        let ramp_running = STEER_RAMP.lock().unwrap_or_else(|p| p.into_inner()).active;
        std::thread::sleep(std::time::Duration::from_millis(if ramp_running { 16 } else { 50 }));
        let now = std::time::Instant::now();
        if ramp_running {
            let next_lx = STEER_RAMP.lock().unwrap_or_else(|p| p.into_inner()).tick(now);
            if let Some(val) = next_lx {
                if let Some((object, handler)) = *RELAY_TARGET.lock().unwrap_or_else(|p| p.into_inner()) {
                    send_pad_event(object, handler, 0x3010, 3, val);
                }
            }
        }
        let play = find_action(base, PLAY_ACTION, &mut play_ptr);
        if play == 0 {
            continue;
        }
        let menu = !action_active(play);
        let map_move = find_action(base, MAP_MOVE_ACTION, &mut map_move_ptr);
        let map_acc = find_action(base, MAP_ACCEPT_ACTION, &mut map_acc_ptr);
        let map = menu && (action_active(map_move) || action_active(map_acc));
        let veh = !menu && action_active(find_action(base, VEH_ACCEL_ACTION, &mut veh_ptr));
        let wheel = !menu
            && (action_active(find_action(base, RADIAL_SLOT_NEXT, &mut rad_next_ptr))
                || action_active(find_action(base, RADIAL_SLOT_PREV, &mut rad_prev_ptr))
                || action_active(find_action(base, SELECT_NEXT_WHEEL, &mut rad_tab_ptr))
                || wheel_hold_active(now));

        let menu_changed = IN_MENU.swap(menu, Ordering::Relaxed) != menu;
        let map_changed = IN_MAP.swap(map, Ordering::Relaxed) != map;
        let wheel_changed = IN_WHEEL.swap(wheel, Ordering::Relaxed) != wheel;
        IN_VEHICLE.store(veh, Ordering::Relaxed);
        if menu_changed || map_changed || wheel_changed {
            tracing::info!(target: "keyboard_events", menu, map, wheel, veh, "input context changed");
        }
        if menu_changed {
            WALKING.store(false, Ordering::Relaxed);
            *WHEEL_HOLD.lock().unwrap_or_else(|p| p.into_inner()) = None;
            STEER_RAMP.lock().unwrap_or_else(|p| p.into_inner()).reset();
            release_all_held();
        } else if veh != was_veh {
            if veh {
                WALKING.store(false, Ordering::Relaxed);
            } else {
                STEER_RAMP.lock().unwrap_or_else(|p| p.into_inner()).reset();
            }
            retranslate_held(base, menu);
        }
        was_veh = veh;
    }
}

/// One held key and the pad source/value it contributes.
#[derive(Clone, Copy, Debug, PartialEq)]
struct HeldKey {
    key: u32,
    source: u32,
    axis: bool,
    value: f32,
}

/// Keys currently down and each key's translated pad source contribution.
static HELD: std::sync::Mutex<Vec<HeldKey>> = std::sync::Mutex::new(Vec::new());

fn is_trigger(source: u32) -> bool {
    matches!(source, 0x3009 | 0x300c)
}

fn axis_value(held: &[HeldKey], source: u32) -> f32 {
    let sum: f32 = held.iter().filter(|h| h.source == source).map(|h| h.value).sum();
    if is_trigger(source) { sum.clamp(0.0, 1.0) } else { sum.clamp(-1.0, 1.0) }
}

fn button_down(held: &[HeldKey], source: u32) -> bool {
    held.iter().any(|h| h.source == source)
}

#[cfg(test)]
fn apply_press(
    held: &mut Vec<HeldKey>,
    key: u32,
    source: u32,
    axis: bool,
    value: f32,
    emit: impl FnMut(u32, u32, f32),
) {
    apply_press_many(held, key, &[(source, axis, value)], emit);
}

fn apply_press_many(
    held: &mut Vec<HeldKey>,
    key: u32,
    sources: &[(u32, bool, f32)],
    mut emit: impl FnMut(u32, u32, f32),
) {
    if held.iter().any(|h| h.key == key) || sources.is_empty() {
        return;
    }
    for &(source, axis, value) in sources {
        if axis {
            let before = axis_value(held, source);
            held.push(HeldKey { key, source, axis, value });
            let after = axis_value(held, source);
            if (after - before).abs() > 1e-4 {
                emit(source, 3, after);
            }
        } else {
            let was_down = button_down(held, source);
            held.push(HeldKey { key, source, axis, value });
            if !was_down {
                emit(source, 7, 1.0);
            }
        }
    }
}

fn apply_release(held: &mut Vec<HeldKey>, key: u32, mut emit: impl FnMut(u32, u32, f32)) {
    let mut touched: Vec<(u32, bool, f32, bool)> = Vec::new();
    for h in held.iter().filter(|h| h.key == key) {
        if !touched.iter().any(|t| t.0 == h.source) {
            let before_val = if h.axis { axis_value(held, h.source) } else { 0.0 };
            let was_down = if !h.axis { button_down(held, h.source) } else { false };
            touched.push((h.source, h.axis, before_val, was_down));
        }
    }
    if touched.is_empty() {
        return;
    }
    held.retain(|h| h.key != key);
    for (source, axis, before_val, was_down) in touched {
        if axis {
            let after = axis_value(held, source);
            if (after - before_val).abs() > 1e-4 {
                emit(source, 3, after);
            }
        } else if was_down && !button_down(held, source) {
            emit(source, 11, 0.0);
        }
    }
}

fn send_pad_event(object: usize, handler: Handler, source: u32, kind: u32, value: f32) {
    let ev = Event { source: source << 8 | PAD_INDEX, kind, extra: 0, value };
    // SAFETY: the title's event buffer handler locks object+8 internally before appending.
    unsafe { handler(object as *mut c_void, &ev) };
}

/// Releases all held translated pad sources (e.g. when entering/exiting a menu so movement doesn't stick).
fn release_all_held() {
    let target = *RELAY_TARGET.lock().unwrap_or_else(|p| p.into_inner());
    let Some((object, handler)) = target else { return };
    let mut held = HELD.lock().unwrap_or_else(|p| p.into_inner());
    let keys: Vec<u32> = held.iter().map(|h| h.key).collect();
    for k in keys {
        apply_release(&mut held, k, |src, kind, val| send_pad_event(object, handler, src, kind, val));
    }
}

/// Re-evaluates held keys when play context changes (e.g. holding W while getting into a car).
fn retranslate_held(base: usize, in_menu: bool) {
    let target = *RELAY_TARGET.lock().unwrap_or_else(|p| p.into_inner());
    let Some((object, handler)) = target else { return };
    let mut held = HELD.lock().unwrap_or_else(|p| p.into_inner());
    if held.is_empty() {
        return;
    }
    let mut keys: Vec<u32> = Vec::new();
    for h in held.iter() {
        if !keys.contains(&h.key) {
            keys.push(h.key);
        }
    }
    let map = KEY_MAP.lock().unwrap_or_else(|p| p.into_inner());
    for key in keys {
        let Some((_, play, menu)) = map.entries.iter().find(|(k, _, _)| *k == key) else { continue };
        let actions: Vec<u32> = if in_menu { menu.iter().chain(play.iter()).copied().collect() } else { play.clone() };
        let Some((_, new_sources)) = resolve_active_sources(base, &actions) else { continue };
        let old_sources: Vec<(u32, bool, f32)> =
            held.iter().filter(|h| h.key == key).map(|h| (h.source, h.axis, h.value)).collect();
        let same = old_sources.len() == new_sources.len()
            && old_sources
                .iter()
                .zip(new_sources.iter())
                .all(|(a, b)| a.0 == b.0 && a.1 == b.1 && (a.2 - b.2).abs() <= 1e-4);
        if !same {
            apply_release(&mut held, key, |src, kind, val| send_pad_event(object, handler, src, kind, val));
            apply_press_many(&mut held, key, &new_sources, |src, kind, val| {
                send_pad_event(object, handler, src, kind, val)
            });
        }
    }
}

fn parse_key_actions(spec: &str) -> Vec<KeyEntry> {
    let hex = |x: &str| u32::from_str_radix(x.trim().trim_start_matches("0x"), 16).ok();
    spec.split(',')
        .filter_map(|b| {
            let (key, actions) = b.split_once('=')?;
            Some((hex(key)?, actions.split('/').filter_map(hex).collect(), Vec::new()))
        })
        .collect()
}

/// RAGE's name hash (Jenkins one-at-a-time, lower case): the actions' ids.
const fn joaat(name: &str) -> u32 {
    let bytes = name.as_bytes();
    let mut h: u32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i].to_ascii_lowercase();
        h = h.wrapping_add(c as u32);
        h = h.wrapping_add(h << 10);
        h ^= h >> 6;
        i += 1;
    }
    h = h.wrapping_add(h << 3);
    h ^= h >> 11;
    h.wrapping_add(h << 15)
}

/// A key name of `native-keys.txt` as the virtual key the title's keyboard device gets. The
/// mouse buttons come as F13 to F17 (`VICTORY_NATIVE_INPUT`, gameinput.rs).
fn key_code(name: &str) -> Option<u32> {
    let n = name.trim();
    let named = match n.to_ascii_lowercase().as_str() {
        "space" => 0x20, "shift" => 0xa0, "rightshift" => 0xa1, "ctrl" => 0xa2, "rightctrl" => 0xa3,
        "alt" => 0xa4, "rightalt" => 0xa5, "tab" => 0x09, "esc" => 0x1b, "enter" => 0x0d, "backspace" => 0x08,
        "capslock" => 0x14, "insert" => 0x2d, "delete" => 0x2e, "home" => 0x24, "end" => 0x23,
        "pageup" => 0x21, "pagedown" => 0x22, "arrowup" => 0x26, "arrowdown" => 0x28, "arrowleft" => 0x25,
        "arrowright" => 0x27, "comma" => 0xbc, "period" => 0xbe, "minus" => 0xbd, "equals" => 0xbb,
        "leftbracket" => 0xdb, "rightbracket" => 0xdd, "semicolon" => 0xba, "quote" => 0xde, "slash" => 0xbf,
        "backslash" => 0xdc, "backquote" => 0xc0, "leftclick" => 0x7c, "rightclick" => 0x7d,
        "middleclick" => 0x7e, "mouse4" => 0x7f, "mouse5" => 0x80,
        "wheelup" => 0x81, "wheeldown" => 0x82,
        _ => 0,
    };
    if named != 0 {
        return Some(named);
    }
    let up = n.to_ascii_uppercase();
    if up.len() == 1 && up.as_bytes()[0].is_ascii_alphanumeric() {
        return Some(u32::from(up.as_bytes()[0]));
    }
    if let Some(f) = up.strip_prefix('F').and_then(|d| d.parse::<u32>().ok()).filter(|f| (1..=12).contains(f)) {
        return Some(0x6f + f);
    }
    if let Some(d) = up.strip_prefix("NUMPAD").and_then(|d| d.parse::<u32>().ok()).filter(|d| *d <= 9) {
        return Some(0x60 + d);
    }
    None
}

/// `native-keys.txt`: lines `KEY = INPUT_ACTION, ... | INPUT_MENU_ACTION, ...` ('#' comments);
/// the actions after `|` are for menus only.
fn parse_native_keys(text: &str) -> Vec<KeyEntry> {
    let ids = |s: &str| -> Vec<u32> { s.split(',').map(str::trim).filter(|a| !a.is_empty()).map(joaat).collect() };
    text.lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .filter_map(|l| {
            let (key, actions) = l.split_once('=')?;
            let key = key_code(key)?;
            let (play, menu) = actions.split_once('|').unwrap_or((actions, ""));
            let (play, menu) = (ids(play), ids(menu));
            (!play.is_empty() || !menu.is_empty()).then_some((key, play, menu))
        })
        .collect()
}

/// A pad source that is an axis (sticks and triggers) rather than a button.
fn is_axis(source: u32) -> bool {
    matches!(source, 0x3009 | 0x300c | 0x3010..=0x3013)
}

/// Chord/combo binding entry vtable (`0x1465B4E00`, e.g. `INPUT_SPECIAL_ABILITY` = `L3 + R3`,
/// `INPUT_SELECT_ITEM_WHEEL` = `RB + DDown`): ALL pad sources in `desc` must be held together.
const CHORD_ENTRY_VTABLE: usize = 0x65b_4e00;

/// Finds the first active action in `actions` that has pad binding record(s) (`0x3000..=0x3013`).
/// Returns `(action_id, vec![(pad_source, is_axis, value), ...])` — 1 source for normal OR entries,
/// or all sources in the chord for `CHORD_ENTRY_VTABLE` entries so holding the PC key holds the full chord.
fn resolve_active_sources(base: usize, actions: &[u32]) -> Option<(u32, Vec<(u32, bool, f32)>)> {
    // SAFETY (both): the title's action array and its objects, read only.
    let read32 = |at: usize| unsafe { std::ptr::read_volatile(at as *const u32) };
    let read = |at: usize| unsafe { std::ptr::read_volatile(at as *const usize) };
    let count = read32(base + ACTION_COUNT) as usize;
    let array = read(base + ACTION_ARRAY);
    if count == 0 || array == 0 {
        return None;
    }
    for &id in actions {
        let Some(action) = (0..count).map(|i| array + i * 0x160).find(|&a| read32(a + 0x128) == id) else {
            continue;
        };
        if read32(action + 0x158) & 2 == 0 || read32(action + 4) == 0 || read(action + 8) == 0 {
            continue;
        }
        let entry = read(read(action + 8));
        let desc = if entry == 0 { 0 } else { read(entry + 0x10) };
        let rec_count = if desc == 0 { 0 } else { read32(desc + 4) as usize };
        if rec_count == 0 {
            continue;
        }
        let is_chord = read(entry) == base + CHORD_ENTRY_VTABLE;
        let records = read(desc + 8);
        let mut out: Vec<(u32, bool, f32)> = Vec::new();
        for r in 0..rec_count {
            let rec = records + r * 12;
            let (source, flags) = (read32(rec), read32(rec + 8) & 0xff);
            if !(0x3000..=0x3013).contains(&source) {
                continue;
            }
            if out.iter().any(|&(s, _, _)| s == source) {
                continue;
            }
            let axis = is_axis(source);
            let value = if axis && flags == 1 { -1.0 } else { 1.0 };
            out.push((source, axis, value));
            if !is_chord {
                break;
            }
        }
        if !out.is_empty() {
            return Some((id, out));
        }
    }
    None
}

/// A key event, as the action it stands for: of the key's actions, the first the title has active
/// right now (+0x158 bit 2), sent as that action's own pad source to the title's buffer, combining
/// simultaneous held keys cleanly (so W+S / A+D never cancel each other dead on release).
///
/// # Safety
/// On the title's thread, inside the keyboard's event broadcast.
unsafe fn translate(me: &Relay, e: &Event) {
    let key = e.source >> 8;
    let (play, menu) = {
        let mut map = KEY_MAP.lock().unwrap_or_else(|p| p.into_inner());
        if e.kind == 7 {
            map.refresh();
        }
        let Some((_, play, menu)) = map.entries.iter().find(|(k, _, _)| *k == key) else { return };
        (play.clone(), menu.clone())
    };
    let base = IMAGE.load(Ordering::Relaxed);
    // SAFETY (both): the title's action array and its objects, read only.
    let read32 = |at: usize| unsafe { std::ptr::read_volatile(at as *const u32) };
    let read = |at: usize| unsafe { std::ptr::read_volatile(at as *const usize) };
    let in_menu = {
        let count = read32(base + ACTION_COUNT) as usize;
        let array = read(base + ACTION_ARRAY);
        (0..count)
            .map(|i| array + i * 0x160)
            .find(|&a| read32(a + 0x128) == PLAY_ACTION)
            .is_some_and(|a| read32(a + 0x158) & 2 == 0)
    };
    // In play only the play actions; in a menu the menu actions first.
    let actions: Vec<u32> = if in_menu { menu.iter().chain(play.iter()).copied().collect() } else { play };
    let in_veh = !in_menu && IN_VEHICLE.load(Ordering::Relaxed);
    let send = |source: u32, kind: u32, value: f32| {
        let final_val = if kind == 3 {
            if in_veh && source == 0x3010 {
                STEER_RAMP.lock().unwrap_or_else(|p| p.into_inner()).set_target(
                    value,
                    std::time::Instant::now(),
                    SteerRamp::configured_ms(),
                )
            } else if !in_menu && !in_veh && WALKING.load(Ordering::Relaxed) && matches!(source, 0x3010 | 0x3011) {
                value * WALK_SCALE
            } else {
                value
            }
        } else {
            value
        };
        let ev = Event { source: source << 8 | PAD_INDEX, kind, extra: 0, value: final_val };
        // SAFETY: the title's handler with its own object and an event of the pad's form.
        unsafe { (me.handler)(me.object, &ev) };
    };
    let mut held = HELD.lock().unwrap_or_else(|p| p.into_inner());
    // Handle Vertex Walk meta-actions (on-foot speed scaling for LX/LY).
    if !in_menu && !in_veh {
        if actions.first().copied() == Some(WALK_TOGGLE) {
            if e.kind == 7 {
                let walking = !WALKING.load(Ordering::Relaxed);
                WALKING.store(walking, Ordering::Relaxed);
                for src in [0x3010u32, 0x3011] {
                    let v = axis_value(&held, src);
                    if v.abs() > 1e-4 {
                        send(src, 3, v);
                    }
                }
            }
            return;
        }
        if actions.first().copied() == Some(WALK_HOLD) {
            if matches!(e.kind, 7 | 11) {
                WALKING.store(e.kind == 7, Ordering::Relaxed);
                for src in [0x3010u32, 0x3011] {
                    let v = axis_value(&held, src);
                    if v.abs() > 1e-4 {
                        send(src, 3, v);
                    }
                }
            }
            return;
        }
    }
    match e.kind {
        7 => {
            if held.iter().any(|h| h.key == key) {
                return;
            }
            if let Some((id, sources)) = resolve_active_sources(base, &actions) {
                if !in_menu {
                    if matches!(id, OPEN_WHEEL_ACTION | OPEN_EMOTE_WHEEL | SELECT_ITEM_WHEEL) {
                        *WHEEL_HOLD.lock().unwrap_or_else(|p| p.into_inner()) =
                            Some((key, std::time::Instant::now(), std::time::Duration::ZERO));
                        IN_WHEEL.store(true, Ordering::Relaxed);
                    } else if id == VEH_RADIO_ACTION && in_veh {
                        *WHEEL_HOLD.lock().unwrap_or_else(|p| p.into_inner()) =
                            Some((key, std::time::Instant::now(), std::time::Duration::from_millis(160)));
                    }
                }
                let (source, value) = (sources[0].0, sources[0].2);
                let chord = sources.len();
                apply_press_many(&mut held, key, &sources, send);
                static SAID: AtomicU64 = AtomicU64::new(0);
                if SAID.fetch_add(1, Ordering::Relaxed) < 200 {
                    tracing::info!(target: "keyboard_events", key = format_args!("{key:#x}"), action = format_args!("{id:#010x}"), source = format_args!("{source:#x}"), chord, value, "key sent as its active action's pad source");
                }
            }
        }
        11 => {
            {
                let mut wh = WHEEL_HOLD.lock().unwrap_or_else(|p| p.into_inner());
                if wh.is_some_and(|(k, _, _)| k == key) {
                    *wh = None;
                    IN_WHEEL.store(false, Ordering::Relaxed);
                }
            }
            apply_release(&mut held, key, send);
        }
        _ => {}
    }
}

fn parse_binds(spec: &str) -> Vec<(u32, u32, u32)> {
    spec.split(',')
        .filter_map(|b| {
            let mut p = b.trim().split(':').map(|x| u32::from_str_radix(x.trim().trim_start_matches("0x"), 16).ok());
            Some((p.next()??, p.next()??, p.next().flatten().unwrap_or(0)))
        })
        .collect()
}

/// The title's action activation (it links an action's sources to it), its first 19 bytes (all
/// position independent), and its allocator `(align, size) -> block`.
const ACTIVATE: usize = 0x1d_67c0;
const ACTIVATE_PROLOGUE: [u8; 19] =
    [0x41, 0x57, 0x41, 0x56, 0x41, 0x55, 0x41, 0x54, 0x56, 0x57, 0x55, 0x53, 0x48, 0x81, 0xec, 0xc8, 0, 0, 0];
const ALLOCATE: usize = 0x4d9_c140;

static BINDS: std::sync::OnceLock<Vec<(u32, u32, u32)>> = std::sync::OnceLock::new();
static TRAMPOLINE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static IMAGE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static ADDED: AtomicU64 = AtomicU64::new(0);

/// Before the title links an action's sources: the asked keyboard records go into its bindings,
/// in a block of the title's own allocator (the title frees and rebuilds these arrays, e.g. around
/// the pause menu, then links again: this runs each time).
unsafe extern "system" fn activate(action: usize) {
    // SAFETY: the title's action object, on the title's thread, before its activation reads it.
    unsafe { add_to(action) };
    let original = TRAMPOLINE.load(Ordering::Relaxed);
    // SAFETY: the trampoline runs the displaced prologue and goes on in the title's function.
    unsafe { std::mem::transmute::<usize, unsafe extern "system" fn(usize)>(original)(action) }
}

/// Standard 0x60-byte binding entry vtable (`0x1465B4C80`) and the title's own parallel-array
/// resize helpers (from `0x141405630` / `0x141405790`). Growing `desc->records` without also
/// growing `entry+0x28`, `entry+0x48`, and `action+0xD0` overwrote past the 0x60-byte entry object.
const ENTRY_VTABLE: usize = 0x65b_4c80;
const ENTRY_RESIZE_28: usize = 0x140_5720;
const ENTRY_GROW_48: usize = 0x575_af60;
const ACTION_GROW_D0: usize = 0x575_ae80;

/// # Safety
/// `action` is one of the title's input actions, being activated on this thread.
unsafe fn add_to(action: usize) {
    let Some(binds) = BINDS.get() else { return };
    // SAFETY (both): fields of the title's action, its entry and binding descriptor.
    let read32 = |at: usize| unsafe { std::ptr::read_volatile(at as *const u32) };
    let read = |at: usize| unsafe { std::ptr::read_volatile(at as *const usize) };
    let id = read32(action + 0x128);
    let wanted: Vec<(u32, u32)> = binds.iter().filter(|b| b.0 == id).map(|b| (b.1, b.2)).collect();
    if wanted.is_empty() || read32(action + 4) == 0 || read(action + 8) == 0 {
        return;
    }
    let entry = read(read(action + 8));
    let base = IMAGE.load(Ordering::Relaxed);
    if entry == 0 || read(entry) != base + ENTRY_VTABLE || read(entry + 0x10) == 0 {
        return;
    }
    let desc = read(entry + 0x10);
    let n = read32(desc + 4) as usize;
    let old = read(desc + 8);
    let mut records: Vec<u32> = (0..n * 3).map(|i| read32(old + 4 * i)).collect();
    let (absent, missing): (Vec<(u32, u32)>, Vec<(u32, u32)>) = wanted
        .into_iter()
        .filter(|&(key, _)| !records.chunks(3).any(|r| r[0] == key))
        .partition(|&(key, _)| !has_record(base, key));
    if !absent.is_empty() {
        static SAID: AtomicU64 = AtomicU64::new(0);
        if SAID.fetch_add(1, Ordering::Relaxed) < 16 {
            tracing::info!(target: "keyboard_events", action = format_args!("{id:#010x}"), keys = ?absent, "keys left out: no source record yet (the next activation adds them)");
        }
    }
    if missing.is_empty() {
        return;
    }
    for &(key, flags) in &missing {
        records.extend([key, 0xf000, flags]);
    }
    let new_n = records.len() / 3;
    // SAFETY: the title's allocator, called on the title's thread as its own code calls it.
    let block = unsafe {
        std::mem::transmute::<usize, unsafe extern "system" fn(usize, usize) -> *mut u32>(base + ALLOCATE)(
            4,
            records.len() * 4,
        )
    };
    if block.is_null() {
        return;
    }
    // SAFETY: install the new records array, then resize the three parallel per-record arrays
    // (`entry+0x28`, `entry+0x48`, and `action+0xD0`) using the title's own helpers so evaluator
    // `0x140EE8C00` never writes past the 0x60-byte entry object.
    unsafe {
        std::ptr::copy_nonoverlapping(records.as_ptr(), block, records.len());
        std::ptr::write_volatile((desc + 8) as *mut usize, block as usize);
        std::ptr::write_volatile((desc + 4) as *mut u32, new_n as u32);
        std::ptr::write_volatile(desc as *mut u32, new_n as u32);

        // 1. Parallel array entry+0x28 (8B per record, initialized to 0x000F000FF).
        std::mem::transmute::<usize, unsafe extern "system" fn(usize, u32)>(base + ENTRY_RESIZE_28)(
            entry,
            new_n as u32,
        );

        // 2. Parallel array entry+0x48 (8B per record, initialized to *(owner + 0x10)).
        let old_4c = read32(entry + 0x4c) as usize;
        let cap_48 = read32(entry + 0x48) as usize;
        if new_n > cap_48 {
            std::mem::transmute::<usize, unsafe extern "system" fn(usize, u32)>(base + ENTRY_GROW_48)(
                entry + 0x48,
                new_n as u32,
            );
        }
        let arr_50 = read(entry + 0x50);
        if arr_50 != 0 {
            let owner = read(entry + 8);
            let fill = if owner != 0 { u64::from(read32(owner + 0x10)) } else { 0 };
            for i in old_4c..new_n {
                std::ptr::write_volatile((arr_50 + i * 8) as *mut u64, fill);
            }
            std::ptr::write_volatile((entry + 0x4c) as *mut u32, new_n as u32);
        }

        // 3. Action-level scratch array action+0xD0 sized to max(entry+0x2C).
        if new_n > read32(action + 0xd4) as usize {
            std::mem::transmute::<usize, unsafe extern "system" fn(usize, u32)>(base + ACTION_GROW_D0)(
                action + 0xd0,
                new_n as u32,
            );
        }
    }
    let times = ADDED.fetch_add(1, Ordering::Relaxed) + 1;
    if times <= 64 || times.is_power_of_two() {
        tracing::info!(target: "keyboard_events", action = format_args!("{id:#010x}"), keys = ?missing, records = new_n, times, "keyboard bindings added at activation (parallel arrays resized)");
    }
}

/// The title's map of source state records (title 0x14ab77e80: `{u32 groups, u32 size, groups}`;
/// a group is 16 control bytes and 16 records of 0x78 bytes, the code first), looked up the way
/// title +0x5091ff0 does, without inserting.
const SOURCE_MAP: usize = 0xab7_7e80;

fn has_record(base: usize, code: u32) -> bool {
    // SAFETY (both): the title's map and its groups, read only.
    let read32 = |at: usize| unsafe { std::ptr::read_volatile(at as *const u32) };
    let read8 = |at: usize| unsafe { std::ptr::read_volatile(at as *const u8) };
    let map = base + SOURCE_MAP;
    let groups = read32(map) as usize;
    // SAFETY: as above.
    let table = unsafe { std::ptr::read_volatile((map + 8) as *const usize) };
    if groups == 0 || table == 0 || !groups.is_power_of_two() {
        return false;
    }
    let mut h = code ^ (code >> 16);
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    let tag = (h & 0x7f) as u8;
    let mut g = (h >> 7) as usize & (groups - 1);
    for _ in 0..groups {
        let group = table + g * 0x790;
        let mut empty = false;
        for i in 0..16 {
            let c = read8(group + i);
            if c == tag && read32(group + 0x10 + i * 0x78) == code {
                return true;
            }
            // vpsignb + vpmovmskb in the title: only 0x80 (empty) ends the search.
            empty |= c == 0x80;
        }
        if empty {
            return false;
        }
        g = (g + 1) & (groups - 1);
    }
    false
}

/// Puts [`activate`] in front of the title's activation.
///
/// # Safety
/// The image at `base` is mapped and none of its code has run yet.
unsafe fn hook_activation(base: usize) {
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc, VirtualProtect,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let at = base + ACTIVATE;
    // SAFETY: inside the mapped image; nothing runs yet.
    let found = unsafe { std::slice::from_raw_parts(at as *const u8, ACTIVATE_PROLOGUE.len()) };
    if found != ACTIVATE_PROLOGUE {
        tracing::warn!(target: "keyboard_events", "the action activation is not the expected code (another build?); no bindings");
        return;
    }
    let jump = |to: usize| -> [u8; 14] {
        let mut j = [0xff, 0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        j[6..].copy_from_slice(&(to as u64).to_le_bytes());
        j
    };
    // SAFETY: a fresh page of our own for the trampoline.
    let trampoline =
        unsafe { VirtualAlloc(std::ptr::null(), 64, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) }.cast::<u8>();
    if trampoline.is_null() {
        return;
    }
    // SAFETY: our page: the displaced prologue, then a jump back after it.
    unsafe {
        std::ptr::copy_nonoverlapping(ACTIVATE_PROLOGUE.as_ptr(), trampoline, ACTIVATE_PROLOGUE.len());
        let back = jump(at + ACTIVATE_PROLOGUE.len());
        std::ptr::copy_nonoverlapping(back.as_ptr(), trampoline.add(ACTIVATE_PROLOGUE.len()), back.len());
    }
    TRAMPOLINE.store(trampoline as usize, Ordering::Relaxed);
    IMAGE.store(base, Ordering::Relaxed);
    let mut patch = [0x90u8; 19];
    patch[..14].copy_from_slice(&jump(activate as unsafe extern "system" fn(usize) as usize));
    let mut old = 0;
    // SAFETY: the title's function head (checked above), made writable for the write and put back.
    unsafe {
        if VirtualProtect(at as *const _, patch.len(), PAGE_EXECUTE_READWRITE, &mut old) == 0 {
            return;
        }
        std::ptr::copy_nonoverlapping(patch.as_ptr(), at as *mut u8, patch.len());
        VirtualProtect(at as *const _, patch.len(), old, &mut old);
        FlushInstructionCache(GetCurrentProcess(), at as *const _, patch.len());
    }
    tracing::info!(target: "keyboard_events", binds = BINDS.get().map_or(0, Vec::len), "keyboard bindings go in at each action activation");
}

/// Starts the subscriber thread when `VICTORY_KEYBOARD_EVENTS` is `1` or `log`.
pub fn start(base: usize) {
    IMAGE.store(base, Ordering::Relaxed);
    if let Some(file) = std::env::var_os("VICTORY_NATIVE_KEYS") {
        let mut map = KEY_MAP.lock().unwrap_or_else(|p| p.into_inner());
        map.file = Some(file.into());
        map.refresh();
        let _ = std::thread::Builder::new().name("menu watch".into()).spawn(move || watch_menus(base));
    } else if let Ok(spec) = std::env::var("VICTORY_KEY_ACTIONS") {
        let entries = parse_key_actions(&spec);
        tracing::info!(target: "keyboard_events", keys = entries.len(), "keys stand for the title's actions (sent as each active action's pad source)");
        KEY_MAP.lock().unwrap_or_else(|p| p.into_inner()).entries = entries;
    }
    if let Ok(spec) = std::env::var("VICTORY_KEY_BINDS") {
        let binds = parse_binds(&spec);
        if !binds.is_empty() && BINDS.set(binds).is_ok() {
            // SAFETY: called before the title runs (run.rs, with the other image patches).
            unsafe { hook_activation(base) };
        }
    }
    let forward = match std::env::var("VICTORY_KEYBOARD_EVENTS").as_deref() {
        Ok("1") => true,
        Ok("log") => false,
        _ => return,
    };
    let spawned = std::thread::Builder::new().name("keyboard events".into()).spawn(move || {
        let read = |at: usize| -> usize {
            // SAFETY: pointer-sized words of the mapped image or of title objects it points at.
            unsafe { std::ptr::read_volatile(at as *const usize) }
        };
        // Wait for the title's subscriber on the first pad.
        let (keyboard, object, handler) = loop {
            let keyboard = read(base + KEYBOARD_SLOT);
            let pad = read(base + PAD_SLOT);
            if keyboard != 0 && pad != 0 {
                let node = read(pad + 8);
                if node != 0 {
                    break (keyboard, read(node), read(node + 8));
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        };
        if handler != base + BUFFER_HANDLER {
            tracing::warn!(target: "keyboard_events", handler = format_args!("+{:#x}", handler.wrapping_sub(base)), "the pad's subscriber is not the expected buffer; left alone");
            return;
        }
        if read(keyboard + 8) != 0 {
            tracing::warn!(target: "keyboard_events", "the keyboard already has a subscriber; left alone");
            return;
        }
        let handler_fn: Handler = unsafe { std::mem::transmute::<usize, Handler>(handler) };
        *RELAY_TARGET.lock().unwrap_or_else(|p| p.into_inner()) = Some((object, handler_fn));
        let me = Box::leak(Box::new(Relay {
            object: object as *mut c_void,
            handler: handler_fn,
            forward,
        }));
        // A node as the title makes them: object, handler, next (null ends the list), and room.
        let node: &'static mut [usize; 8] = Box::leak(Box::new([0usize; 8]));
        node[0] = std::ptr::from_mut(me) as usize;
        node[1] = relay as Handler as usize;
        // SAFETY: the keyboard device's list head, one pointer-sized store.
        unsafe { std::ptr::write_volatile((keyboard + 8) as *mut usize, node.as_ptr() as usize) };
        tracing::info!(
            target: "keyboard_events",
            keyboard = format_args!("{keyboard:#x}"),
            buffer = format_args!("{object:#x}"),
            forward,
            "the keyboard's events now reach the title's input buffer"
        );
    });
    if let Err(e) = spawned {
        tracing::warn!(target: "keyboard_events", error = %e, "keyboard events did not start");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ids read from the title's action table (bindings dump 2026-11-27).
    #[test]
    fn names_hash_to_the_titles_action_ids() {
        assert_eq!(joaat("INPUT_MOVE_UD"), 0xfda8_3190);
        assert_eq!(joaat("INPUT_VEH_ACCELERATE"), 0x5b9f_d4e2);
        assert_eq!(joaat("input_jump"), 0xd9d0_e1c0);
    }

    #[test]
    fn native_keys_lines_parse() {
        let map = parse_native_keys("# c\nW = INPUT_VEH_ACCELERATE, INPUT_MOVE_UP_ONLY\nLeftClick = INPUT_ATTACK | INPUT_JUMP\nWheelUp = INPUT_GAME_MENU_UP\nWheelDown = INPUT_GAME_MENU_DOWN\nF5 = INPUT_X\nbad\n");
        assert_eq!(map[0], (0x57, vec![0x5b9f_d4e2, 0x8fd0_15d8], vec![]));
        assert_eq!(map[1], (0x7c, vec![joaat("INPUT_ATTACK")], vec![0xd9d0_e1c0]));
        assert_eq!(map[2].0, 0x81);
        assert_eq!(map[3].0, 0x82);
        assert_eq!(map[4].0, 0x74);
        assert_eq!(map.len(), 5);
    }

    #[test]
    fn opposing_axis_keys_combine_and_restore_on_release() {
        let mut held = Vec::new();
        let mut events = Vec::new();
        let mut emit = |s, k, v| events.push((s, k, v));
        // Hold W (LY = -1.0)
        apply_press(&mut held, b'W' as u32, 0x3011, true, -1.0, &mut emit);
        // Tap S (LY = +1.0) while W stays held -> net 0.0
        apply_press(&mut held, b'S' as u32, 0x3011, true, 1.0, &mut emit);
        // Release S while W stays held -> immediately restores -1.0!
        apply_release(&mut held, b'S' as u32, &mut emit);
        // Release W -> 0.0
        apply_release(&mut held, b'W' as u32, &mut emit);
        assert_eq!(
            events,
            vec![
                (0x3011, 3, -1.0),
                (0x3011, 3, 0.0),
                (0x3011, 3, -1.0),
                (0x3011, 3, 0.0),
            ]
        );
    }

    #[test]
    fn shared_button_stays_pressed_until_last_key_releases() {
        let mut held = Vec::new();
        let mut events = Vec::new();
        let mut emit = |s, k, v| events.push((s, k, v));
        apply_press(&mut held, 0x20, 0x3006, false, 1.0, &mut emit);
        apply_press(&mut held, 0x0d, 0x3006, false, 1.0, &mut emit);
        apply_release(&mut held, 0x20, &mut emit);
        apply_release(&mut held, 0x0d, &mut emit);
        assert_eq!(events, vec![(0x3006, 7, 1.0), (0x3006, 11, 0.0)]);
    }

    #[test]
    fn steer_ramp_starts_with_bite_ramps_smoothly_and_snaps_back_on_release() {
        let mut ramp = SteerRamp::new(100.0);
        let t0 = std::time::Instant::now();
        // Press D (+1.0) -> starts at INITIAL_BITE (0.28)
        let v0 = ramp.set_target(1.0, t0, 100.0);
        assert!((v0 - SteerRamp::INITIAL_BITE).abs() < 1e-4);
        assert!(ramp.active);
        // Halfway through ramp (~50ms) -> midway between 0.28 and 1.0
        let v1 = ramp.tick(t0 + std::time::Duration::from_millis(50)).unwrap();
        assert!(v1 > 0.6 && v1 < 0.7);
        // After full ramp -> reaches 1.0 and deactivates
        let v2 = ramp.tick(t0 + std::time::Duration::from_millis(120)).unwrap();
        assert!((v2 - 1.0).abs() < 1e-4);
        assert!(!ramp.active);
        // Release D (0.0) -> snaps immediately to 0.0 with no lag
        let v3 = ramp.set_target(0.0, t0 + std::time::Duration::from_millis(140), 100.0);
        assert_eq!(v3, 0.0);
        assert!(!ramp.active);
        // ramp_ms = 0 disables ramping (instant full lock)
        let v4 = ramp.set_target(-1.0, t0 + std::time::Duration::from_millis(150), 0.0);
        assert_eq!(v4, -1.0);
        assert!(!ramp.active);
    }

    #[test]
    fn walk_meta_actions_hash_consistently() {
        let map = parse_native_keys("N = VERTEX_WALK_TOGGLE\nRightAlt = VERTEX_WALK_HOLD\n");
        assert_eq!(map[0], (0x4e, vec![WALK_TOGGLE], vec![]));
        assert_eq!(map[1], (0xa5, vec![WALK_HOLD], vec![]));
    }

    #[test]
    fn chord_key_presses_and_releases_all_buttons_together() {
        let mut held = Vec::new();
        let mut events = Vec::new();
        let mut emit = |s, k, v| events.push((s, k, v));
        // CapsLock (0x14) -> INPUT_SPECIAL_ABILITY chord (L3 = 0x300a + R3 = 0x300d)
        apply_press_many(&mut held, 0x14, &[(0x300a, false, 1.0), (0x300d, false, 1.0)], &mut emit);
        // Holding Shift (0xA0) for L3 while CapsLock is down should NOT double-emit L3,
        // and releasing CapsLock must keep L3 held until Shift releases!
        apply_press(&mut held, 0xa0, 0x300a, false, 1.0, &mut emit);
        apply_release(&mut held, 0x14, &mut emit);
        apply_release(&mut held, 0xa0, &mut emit);
        assert_eq!(
            events,
            vec![
                (0x300a, 7, 1.0),
                (0x300d, 7, 1.0),
                (0x300d, 11, 0.0),
                (0x300a, 11, 0.0),
            ]
        );
    }
}
