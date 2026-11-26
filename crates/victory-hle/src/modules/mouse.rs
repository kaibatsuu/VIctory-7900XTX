//! Mouse look for the title's window. A left click in the client area
//! captures the mouse: the cursor is hidden and clipped to the window, and
//! raw mouse motion (WM_INPUT, unaffected by pointer acceleration) is turned
//! into right-stick deflection of the pad the title reads (`gameinput`).
//! Left button: right trigger; right button: left trigger; middle button:
//! right-stick click. Alt, or the window losing focus, lets the mouse go.
//!
//! A console title only knows sticks, so motion becomes a stick held for as
//! long as the mouse moves: counts per second, smoothed over a short window
//! (the title may sample the pad only a few times a second at low frame
//! rates), scaled so that `VICTORY_MOUSE_FULL` counts per second is a full
//! deflection, then lifted past the title's own dead zone.
//!
//! Settings (environment): `VICTORY_MOUSE=0` turns it off;
//! `VICTORY_MOUSE_FULL` (default 1500 counts/s), `VICTORY_MOUSE_SMOOTH_MS`
//! (default 60), `VICTORY_MOUSE_DEADZONE` (default 0.2, the stick value the
//! slightest motion starts at), `VICTORY_MOUSE_INVERT=1` (inverts vertical).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::time::Instant;

/// What the mouse adds to a pad: right stick -1 to 1 (up positive), and
/// the buttons held.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct MouseInput {
    pub right_x: f32,
    pub right_y: f32,
    pub left: bool,
    pub right: bool,
    pub middle: bool,
}

static CAPTURED: AtomicBool = AtomicBool::new(false);
static DX: AtomicI32 = AtomicI32::new(0);
static DY: AtomicI32 = AtomicI32::new(0);
/// Bit 0 left, 1 right, 2 middle.
static BUTTONS: AtomicU32 = AtomicU32::new(0);

struct Settings {
    enabled: bool,
    full: f32,
    smooth_s: f32,
    deadzone: f32,
    invert: bool,
}

fn settings() -> &'static Settings {
    static S: std::sync::OnceLock<Settings> = std::sync::OnceLock::new();
    S.get_or_init(|| {
        let num = |name: &str, default: f32| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.trim().parse::<f32>().ok())
                .filter(|v| v.is_finite() && *v >= 0.0)
                .unwrap_or(default)
        };
        let flag = |name: &str| std::env::var(name).is_ok_and(|v| v.trim() == "1");
        Settings {
            enabled: std::env::var("VICTORY_MOUSE")
                .ok()
                .is_none_or(|v| v.trim() != "0"),
            full: num("VICTORY_MOUSE_FULL", 1500.0).max(1.0),
            smooth_s: num("VICTORY_MOUSE_SMOOTH_MS", 60.0) / 1000.0,
            deadzone: num("VICTORY_MOUSE_DEADZONE", 0.2).min(0.95),
            invert: flag("VICTORY_MOUSE_INVERT"),
        }
    })
}

/// Smoothed velocity in counts per second, between reads.
struct Motion {
    at: Option<Instant>,
    vx: f32,
    vy: f32,
}

static MOTION: Mutex<Motion> = Mutex::new(Motion {
    at: None,
    vx: 0.0,
    vy: 0.0,
});

/// One smoothing step: `dx`, `dy` counts over `dt` seconds folded into the
/// velocity (an exponential average with time constant `tau`).
fn smooth(v: (f32, f32), dx: f32, dy: f32, dt: f32, tau: f32) -> (f32, f32) {
    if dt <= 0.0 {
        return v;
    }
    let (rx, ry) = (dx / dt, dy / dt);
    let a = if tau <= 0.0 {
        1.0
    } else {
        1.0 - (-dt / tau).exp()
    };
    (v.0 + (rx - v.0) * a, v.1 + (ry - v.1) * a)
}

/// Velocity (counts/s, screen axes: y down) to stick deflection: scaled,
/// capped at full, and radially lifted past `deadzone` so that any motion
/// moves the camera.
fn to_stick(vx: f32, vy: f32, full: f32, deadzone: f32, invert: bool) -> (f32, f32) {
    let (x, y) = (vx / full, if invert { vy } else { -vy } / full);
    let mag = (x * x + y * y).sqrt();
    if mag < 1e-3 {
        return (0.0, 0.0);
    }
    let lifted = deadzone + (1.0 - deadzone) * mag.min(1.0);
    (x / mag * lifted, y / mag * lifted)
}

/// The mouse's contribution now; zero while it is not captured.
pub(crate) fn take() -> MouseInput {
    let s = settings();
    let mut m = MOTION.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let dx = DX.swap(0, Ordering::Relaxed) as f32;
    let dy = DY.swap(0, Ordering::Relaxed) as f32;
    if !CAPTURED.load(Ordering::Relaxed) {
        *m = Motion {
            at: None,
            vx: 0.0,
            vy: 0.0,
        };
        return MouseInput::default();
    }
    let dt = m
        .at
        .map_or(0.0, |at| now.duration_since(at).as_secs_f32())
        .min(0.25);
    m.at = Some(now);
    (m.vx, m.vy) = smooth((m.vx, m.vy), dx, dy, dt, s.smooth_s);
    let (right_x, right_y) = to_stick(m.vx, m.vy, s.full, s.deadzone, s.invert);
    let b = BUTTONS.load(Ordering::Relaxed);
    MouseInput {
        right_x,
        right_y,
        left: b & 1 != 0,
        right: b & 2 != 0,
        middle: b & 4 != 0,
    }
}

#[cfg(windows)]
pub(crate) use host::message;

#[cfg(windows)]
mod host {
    use std::sync::atomic::Ordering;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::MapWindowPoints;
    use windows_sys::Win32::UI::Input::{
        GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT,
        RIM_TYPEMOUSE, RegisterRawInputDevices,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        self as wm, ClipCursor, GetClientRect, HTCLIENT, IDC_ARROW, LoadCursorW, SetCursor,
    };

    use super::{BUTTONS, CAPTURED, DX, DY, settings};

    const RI_MOUSE_LEFT_BUTTON_DOWN: u16 = 0x0001;
    const RI_MOUSE_LEFT_BUTTON_UP: u16 = 0x0002;
    const RI_MOUSE_RIGHT_BUTTON_DOWN: u16 = 0x0004;
    const RI_MOUSE_RIGHT_BUTTON_UP: u16 = 0x0008;
    const RI_MOUSE_MIDDLE_BUTTON_DOWN: u16 = 0x0010;
    const RI_MOUSE_MIDDLE_BUTTON_UP: u16 = 0x0020;
    const MOUSE_MOVE_ABSOLUTE: u16 = 0x0001;
    const VK_MENU: usize = 0x12;
    const VK_LMENU: usize = 0xA4;
    const VK_RMENU: usize = 0xA5;

    /// The window's messages, before the title sees them: `Some` when this
    /// one is ours alone.
    pub(crate) fn message(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        if !settings().enabled {
            return None;
        }
        let captured = CAPTURED.load(Ordering::Relaxed);
        match msg {
            // A click that activates the window: the title's procedure eats
            // it, so it is taken here (activate, and capture on the way).
            wm::WM_MOUSEACTIVATE
                if !captured
                    && (lparam & 0xFFFF) as u32 == HTCLIENT
                    && ((lparam >> 16) & 0xFFFF) as u32 == wm::WM_LBUTTONDOWN =>
            {
                capture(hwnd);
                Some(wm::MA_ACTIVATEANDEAT as LRESULT)
            }
            wm::WM_LBUTTONDOWN if !captured => {
                capture(hwnd);
                // The click that captures is not a trigger pull.
                Some(0)
            }
            wm::WM_INPUT if captured => {
                read(lparam as HRAWINPUT);
                None
            }
            wm::WM_SETCURSOR if captured && (lparam & 0xFFFF) as u32 == HTCLIENT => {
                // SAFETY: hides the cursor over our client area.
                unsafe { SetCursor(std::ptr::null_mut()) };
                Some(1)
            }
            wm::WM_KILLFOCUS if captured => {
                release("focus lost");
                None
            }
            wm::WM_ACTIVATEAPP | wm::WM_ACTIVATE if captured && wparam & 0xFFFF == 0 => {
                release("deactivated");
                None
            }
            wm::WM_SYSKEYDOWN | wm::WM_KEYDOWN
                if captured && matches!(wparam, VK_MENU | VK_LMENU | VK_RMENU) =>
            {
                release("Alt");
                None
            }
            wm::WM_NCDESTROY if captured => {
                release("window destroyed");
                None
            }
            wm::WM_MOVE | wm::WM_SIZE | wm::WM_WINDOWPOSCHANGED if captured => {
                clip(hwnd);
                None
            }
            _ => None,
        }
    }

    fn capture(hwnd: HWND) {
        let device = RAWINPUTDEVICE {
            usUsagePage: 0x01, // generic desktop
            usUsage: 0x02,     // mouse
            dwFlags: 0,
            hwndTarget: hwnd,
        };
        // SAFETY: one valid device description.
        let ok = unsafe { RegisterRawInputDevices(&device, 1, size_of::<RAWINPUTDEVICE>() as u32) };
        if ok == 0 {
            tracing::warn!("mouse look: raw mouse input refused; the mouse stays free");
            return;
        }
        DX.store(0, Ordering::Relaxed);
        DY.store(0, Ordering::Relaxed);
        BUTTONS.store(0, Ordering::Relaxed);
        CAPTURED.store(true, Ordering::Relaxed);
        clip(hwnd);
        // SAFETY: hides the cursor now; WM_SETCURSOR keeps it hidden.
        unsafe { SetCursor(std::ptr::null_mut()) };
        tracing::info!("mouse look: captured (Alt or switching windows releases it)");
    }

    fn release(why: &str) {
        CAPTURED.store(false, Ordering::Relaxed);
        BUTTONS.store(0, Ordering::Relaxed);
        // SAFETY: lifts the clip and restores the standard arrow cursor.
        unsafe {
            ClipCursor(std::ptr::null());
            SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_ARROW));
        }
        tracing::info!(why, "mouse look: released");
    }

    /// Keeps the cursor inside the client area, in screen coordinates.
    fn clip(hwnd: HWND) {
        let mut rect = RECT::default();
        // SAFETY: plain queries on our window; the rectangle is two points.
        unsafe {
            if GetClientRect(hwnd, &mut rect) == 0 {
                return;
            }
            MapWindowPoints(hwnd, std::ptr::null_mut(), (&raw mut rect).cast::<POINT>(), 2);
            ClipCursor(&rect);
        }
    }

    fn read(handle: HRAWINPUT) {
        let mut raw = std::mem::MaybeUninit::<RAWINPUT>::zeroed();
        let mut size = size_of::<RAWINPUT>() as u32;
        // SAFETY: room for one RAWINPUT; mouse input fits it.
        let got = unsafe {
            GetRawInputData(
                handle,
                RID_INPUT,
                raw.as_mut_ptr().cast(),
                &mut size,
                size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        if got == u32::MAX || got == 0 {
            return;
        }
        // SAFETY: filled by the system above.
        let raw = unsafe { raw.assume_init() };
        if raw.header.dwType != RIM_TYPEMOUSE {
            return;
        }
        // SAFETY: a mouse record, per its header.
        let mouse = unsafe { raw.data.mouse };
        if mouse.usFlags & MOUSE_MOVE_ABSOLUTE == 0 {
            DX.fetch_add(mouse.lLastX, Ordering::Relaxed);
            DY.fetch_add(mouse.lLastY, Ordering::Relaxed);
        }
        // SAFETY: the button flags of the union's mouse view.
        let flags = unsafe { mouse.Anonymous.Anonymous.usButtonFlags };
        for (down, up, bit) in [
            (RI_MOUSE_LEFT_BUTTON_DOWN, RI_MOUSE_LEFT_BUTTON_UP, 1u32),
            (RI_MOUSE_RIGHT_BUTTON_DOWN, RI_MOUSE_RIGHT_BUTTON_UP, 2),
            (RI_MOUSE_MIDDLE_BUTTON_DOWN, RI_MOUSE_MIDDLE_BUTTON_UP, 4),
        ] {
            if flags & down != 0 {
                BUTTONS.fetch_or(bit, Ordering::Relaxed);
            }
            if flags & up != 0 {
                BUTTONS.fetch_and(!bit, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{smooth, to_stick};

    #[test]
    fn still_mouse_is_a_centred_stick() {
        assert_eq!(to_stick(0.0, 0.0, 1500.0, 0.2, false), (0.0, 0.0));
    }

    #[test]
    fn slight_motion_starts_past_the_dead_zone_and_fast_motion_caps() {
        let (x, y) = to_stick(15.0, 0.0, 1500.0, 0.2, false);
        assert!(x > 0.2 && x < 0.22, "{x}");
        assert_eq!(y, 0.0);
        let (x, _) = to_stick(1e6, 0.0, 1500.0, 0.2, false);
        assert!((x - 1.0).abs() < 1e-6);
    }

    #[test]
    fn mouse_down_is_stick_down_unless_inverted() {
        assert!(to_stick(0.0, 500.0, 1500.0, 0.2, false).1 < 0.0);
        assert!(to_stick(0.0, 500.0, 1500.0, 0.2, true).1 > 0.0);
    }

    #[test]
    fn smoothing_moves_towards_the_rate_and_settles() {
        // 15 counts in 10 ms is 1500 counts/s.
        let mut v = (0.0, 0.0);
        v = smooth(v, 15.0, 0.0, 0.01, 0.06);
        assert!(v.0 > 0.0 && v.0 < 1500.0);
        for _ in 0..100 {
            v = smooth(v, 15.0, 0.0, 0.01, 0.06);
        }
        assert!((v.0 - 1500.0).abs() < 1.0);
        // Stopping decays to rest.
        for _ in 0..100 {
            v = smooth(v, 0.0, 0.0, 0.01, 0.06);
        }
        assert!(v.0.abs() < 1.0);
        // No smoothing: the rate itself.
        assert_eq!(smooth((0.0, 0.0), 15.0, 0.0, 0.01, 0.0).0, 1500.0);
    }
}
