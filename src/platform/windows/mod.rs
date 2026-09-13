use std::collections::HashMap;
use std::os::raw::c_int;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};

pub mod tray;

#[link(name = "dwmapi")]
unsafe extern "system" {
    fn DwmSetWindowAttribute(
        hwnd: isize,
        dw_attribute: u32,
        pv_attribute: *const c_int,
        cb_attribute: u32,
    ) -> i32;
}

// Native window helpers for the floating usage ball. These manipulate the
// window by its `HWND` (captured at creation) so they work independently of
// GPUI's window API, which this revision does not expose for always-on-top,
// show/hide, frameless dragging, or circular clipping.
#[link(name = "user32")]
unsafe extern "system" {
    fn SetWindowPos(
        hwnd: isize,
        insert_after: isize,
        x: c_int,
        y: c_int,
        cx: c_int,
        cy: c_int,
        flags: u32,
    ) -> i32;
    fn ShowWindow(hwnd: isize, n_cmd_show: c_int) -> i32;
    fn GetClientRect(hwnd: isize, lprect: *mut Rect) -> i32;
    fn SetWindowRgn(hwnd: isize, hrgn: isize, bredraw: i32) -> i32;
    fn GetDpiForWindow(hwnd: isize) -> u32;
}

#[link(name = "gdi32")]
unsafe extern "system" {
    fn CreateEllipticRgn(nleft: c_int, ntop: c_int, nright: c_int, nbottom: c_int) -> isize;
}

#[repr(C)]
struct Rect {
    left: c_int,
    top: c_int,
    right: c_int,
    bottom: c_int,
}

const SWP_NOMOVE: u32 = 0x0002;
const SWP_NOSIZE: u32 = 0x0001;
const HWND_TOPMOST: isize = -1;
const HWND_NOTOPMOST: isize = -2;
const SW_SHOW: c_int = 5;
const SW_HIDE: c_int = 0;

/// Pin a window above all others (used for the always-on-top floating ball).
pub fn set_always_on_top(hwnd: isize, on_top: bool) {
    if hwnd == 0 {
        return;
    }
    unsafe {
        SetWindowPos(
            hwnd,
            if on_top { HWND_TOPMOST } else { HWND_NOTOPMOST },
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE,
        );
    }
}

/// Show or hide a window by its native handle (Win32 `ShowWindow`).
pub fn show_window(hwnd: isize, visible: bool) {
    if hwnd == 0 {
        return;
    }
    unsafe {
        ShowWindow(hwnd, if visible { SW_SHOW } else { SW_HIDE });
    }
}

/// Logical-px size of a window's client area, or `None` if it is not laid out
/// yet.
///
/// [`set_window_circle_region`] clips in *client* pixels, so anything drawn
/// inside that window must be laid out against this same rectangle — otherwise
/// the ball and the circle it is clipped to are centred on two different
/// rectangles. That mismatch is invisible in the arithmetic but very visible on
/// screen: the clip then shaves one side of the ball (a hard, aliased rim) and
/// eats the glow on exactly that side.
///
/// GPUI's `Window::viewport_size()` is *not* a safe substitute. It reports the
/// size GPUI believes the window has, which can disagree with the real client
/// area (DPI rounding, a popup the OS sized differently). Deriving the ball from
/// the viewport while clipping from the client rect is what produced the
/// one-sided cut.
pub fn client_size_logical(hwnd: isize, scale: f32) -> Option<(f32, f32)> {
    if hwnd == 0 || !(scale > 0.0) {
        return None;
    }
    let mut rc = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe {
        GetClientRect(hwnd, &mut rc);
    }
    let w = (rc.right - rc.left) as f32;
    let h = (rc.bottom - rc.top) as f32;
    if w <= 0.0 || h <= 0.0 {
        None
    } else {
        Some((w / scale, h / scale))
    }
}

/// Clip the floating window to a circle of the given `diameter` (logical px,
/// concentric with a square window of `window_size` logical px). This removes
/// the square OS frame and its rectangular shadow, and makes clicks outside
/// the ball pass through to the desktop — the 360 / Thunder-style floating
/// ball.
///
/// The region is computed as a **fraction of the client rect**
/// (`diameter / window_size`), not by converting logical px to device px
/// through an assumed scale. That is what the layout actually guarantees: the
/// ball is laid out at `diameter / window_size` of the window whatever the DPI,
/// whichever monitor the ball is dragged onto, and whatever rounding the OS
/// applied to the window size. Scaling *both* sides by the same unknown factor
/// cancels out, so the clip can never come out smaller than the ball and shave
/// its rim — the failure mode that made the ball look cut off.
///
/// Two more things keep the result a true, centered circle:
/// * The circle is centered on the client rect on **each axis**, so a client
///   area that is not exactly square cannot shift the clip off-center.
/// * The diameter is clamped to `min(client_w, client_h)`. A region *larger*
///   than the window makes `SetWindowRgn` degenerate into "no clip at all",
///   which shows the square frame. (The caller already keeps `diameter` inside
///   `window_size`, so this is belt-and-braces.)
pub fn set_window_circle_region(hwnd: isize, diameter: f32, window_size: f32) {
    if hwnd == 0 || window_size <= 0.0 || diameter <= 0.0 {
        return;
    }
    let mut rc = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe {
        GetClientRect(hwnd, &mut rc);
    }
    let client_w = (rc.right - rc.left) as f32;
    let client_h = (rc.bottom - rc.top) as f32;
    let (cw, ch) = if client_w > 0.0 && client_h > 0.0 {
        (client_w, client_h)
    } else {
        // Not laid out yet: the very first frame can run before the client
        // rect is valid. Fall back to the monitor DPI so the window is still
        // round on frame one, instead of staying square until the next render
        // (which, with a static ball, may never come).
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
        let side = window_size * scale;
        (side, side)
    };
    let max_d = cw.min(ch);
    let frac = (diameter / window_size).clamp(0.0, 1.0);
    let d = (max_d * frac).round().clamp(1.0, max_d.round()) as c_int;
    let left = ((cw - d as f32) / 2.0).round() as c_int;
    let top = ((ch - d as f32) / 2.0).round() as c_int;
    let hrgn = unsafe { CreateEllipticRgn(left, top, left + d, top + d) };
    if hrgn != 0 {
        unsafe {
            SetWindowRgn(hwnd, hrgn, 1);
        }
        // Remember the ellipse (device px) so the drag subclass can answer
        // `WM_NCHITTEST` against the exact circle the window is clipped to.
        if left >= 0 && top >= 0 && d > 0 {
            CLIP_REGION
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .unwrap()
                .insert(hwnd, (left, top, d));
        }
    }
}

// ── Custom drag via low-level mouse hook ─────────────────────────────────────
//
// The floating ball is moved by a system-wide low-level mouse hook
// (`WH_MOUSE_LL`). This is the only reliable way to make a transparent,
// click-through floating widget draggable on Windows, because every earlier
// approach failed for the same structural reason:
//
// Our window is created with `window_background: Transparent`, and GPUI sets
// `WS_EX_NOREDIRECTIONBITMAP` on it (`crates/gpui_windows/src/window.rs:493`).
// That style tells the OS there is no per-pixel bitmap to sample, so Windows
// cannot do alpha hit-testing and treats the **entire window** as
// `HTTRANSPARENT` — every mouse press falls through to the desktop and is never
// delivered to the window at all. Neither `WindowControlArea::Drag` (GPUI only
// returns `HTCAPTION` when `is_movable` is true, and a transparent borderless
// popup has `is_movable == false`, `events.rs:955`) nor a `WM_NCHITTEST`
// subclass (the message is never dispatched to the window) can receive the
// press, so the ball could not be dragged by any in-window technique.
//
// A low-level mouse hook receives *every* mouse event system-wide — including
// events over a click-through / transparent window — so it does not depend on
// hit testing, `is_movable`, `WS_EX_*` styles, or subclass ordering. When the
// cursor is inside the ball's circle we `SetWindowPos` the window to follow the
// cursor and swallow the event (return 1) so the desktop behind does not react;
// clicks outside the ball fall through to `CallNextHookEx` unchanged, and the
// circular `SetWindowRgn` clip still makes the corners click-through to the
// desktop. The hook is installed on the GUI thread (the one that pumps
// messages), so its callback fires normally.

#[link(name = "user32")]
unsafe extern "system" {
    fn GetWindowRect(hwnd: isize, lprect: *mut Rect) -> i32;
    fn IsWindow(hwnd: isize) -> i32;
    fn SetWindowsHookExW(id_hook: i32, lpfn: *const (), hmod: isize, dw_thread_id: u32) -> isize;
    fn CallNextHookEx(hhk: isize, n_code: i32, w_param: usize, l_param: isize) -> isize;
}

#[repr(C)]
struct Point {
    x: i32,
    y: i32,
}

const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONUP: u32 = 0x0202;
const WH_MOUSE_LL: i32 = 14;
const HC_ACTION: i32 = 0;
const SWP_NOZORDER: u32 = 0x0002;
const SWP_NOACTIVATE: u32 = 0x0010;

/// The `HWND` of the floating ball, read by the hook each event so it can move
/// the right window. Set at creation; the hook ignores events while this is 0.
static FLOAT_HWND: OnceLock<Mutex<isize>> = OnceLock::new();

/// `(offset_x, offset_y)` = cursor minus window origin captured at drag start.
static DRAG_OFFSET: OnceLock<Mutex<(i32, i32)>> = OnceLock::new();

/// Whether a drag is in progress (left button is down on the ball).
static DRAGGING: OnceLock<Mutex<bool>> = OnceLock::new();

/// Installed low-level mouse hook handle, kept so it is never dropped and could
/// be uninstalled on shutdown.
static MOUSE_HOOK: OnceLock<Mutex<isize>> = OnceLock::new();

/// Circular clip region (device px) for each floating hwnd, as computed in
/// [`set_window_circle_region`]: `(left, top, diameter)`. The hook uses it to
/// decide whether the cursor is inside the ball (start a drag) or outside it
/// (click-through to the desktop) — the same circle the window is visually
/// clipped to, so the drag surface matches what is drawn.
static CLIP_REGION: OnceLock<Mutex<HashMap<isize, (c_int, c_int, c_int)>>> = OnceLock::new();

#[repr(C)]
struct MouseHookStruct {
    pt: Point,
    hwnd: isize,
    w_hit_test_code: u32,
    dw_extra_info: usize,
}

/// Is the screen point `(sx, sy)` inside the ball's circular clip? Uses the
/// same ellipse [`set_window_circle_region`] applied (`CLIP_REGION`), offset by
/// the window's current screen origin.
unsafe fn cursor_inside_ball(sx: i32, sy: i32) -> bool {
    let hwnd = *FLOAT_HWND.get_or_init(|| Mutex::new(0)).lock().unwrap();
    if hwnd == 0 || IsWindow(hwnd) == 0 {
        return false;
    }
    let Some((rl, rt, rd)) = CLIP_REGION
        .get()
        .and_then(|m| m.lock().unwrap().get(&hwnd).copied())
    else {
        return false;
    };
    let mut rc = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if GetWindowRect(hwnd, &mut rc) == 0 {
        return false;
    }
    let r = rd as f64 / 2.0;
    if r <= 0.0 {
        return false;
    }
    let cx = rc.left as f64 + (rl as f64 + r);
    let cy = rc.top as f64 + (rt as f64 + r);
    let dx = sx as f64 - cx;
    let dy = sy as f64 - cy;
    dx * dx + dy * dy <= r * r
}

unsafe extern "system" fn mouse_proc(n_code: i32, w_param: usize, l_param: isize) -> isize {
    if n_code == HC_ACTION {
        let info = &*(l_param as *const MouseHookStruct);
        match w_param as u32 {
            WM_LBUTTONDOWN => {
                if cursor_inside_ball(info.pt.x, info.pt.y) {
                    let hwnd = *FLOAT_HWND.get_or_init(|| Mutex::new(0)).lock().unwrap();
                    let mut rc = Rect {
                        left: 0,
                        top: 0,
                        right: 0,
                        bottom: 0,
                    };
                    if hwnd != 0 && GetWindowRect(hwnd, &mut rc) != 0 {
                        *DRAG_OFFSET
                            .get_or_init(|| Mutex::new((0, 0)))
                            .lock()
                            .unwrap() = (info.pt.x - rc.left, info.pt.y - rc.top);
                        *DRAGGING.get_or_init(|| Mutex::new(false)).lock().unwrap() = true;
                        // Swallow the press so the window behind the ball does
                        // not start its own interaction (text selection, etc.).
                        return 1;
                    }
                }
            }
            WM_MOUSEMOVE => {
                if *DRAGGING.get_or_init(|| Mutex::new(false)).lock().unwrap() {
                    let hwnd = *FLOAT_HWND.get_or_init(|| Mutex::new(0)).lock().unwrap();
                    if hwnd != 0 {
                        let (ox, oy) = *DRAG_OFFSET
                            .get_or_init(|| Mutex::new((0, 0)))
                            .lock()
                            .unwrap();
                        // Move the window so the cursor stays on the same point
                        // of the ball. `SWP_NOZORDER` keeps always-on-top;
                        // `SWP_NOACTIVATE` avoids stealing focus mid-drag.
                        SetWindowPos(
                            hwnd,
                            0,
                            info.pt.x - ox,
                            info.pt.y - oy,
                            0,
                            0,
                            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                        return 1;
                    }
                }
            }
            WM_LBUTTONUP => {
                if *DRAGGING.get_or_init(|| Mutex::new(false)).lock().unwrap() {
                    *DRAGGING.get_or_init(|| Mutex::new(false)).lock().unwrap() = false;
                    return 1;
                }
            }
            _ => {}
        }
    }
    CallNextHookEx(0, n_code, w_param, l_param)
}

/// Install the low-level mouse hook that drives ball dragging. Idempotent: the
/// hook is installed once for the whole process and simply reads the current
/// ball `HWND` (updated here on every (re)creation) each event, so it survives
/// window hide/show and re-creation without being re-installed.
pub fn install_ball_drag(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    *FLOAT_HWND.get_or_init(|| Mutex::new(0)).lock().unwrap() = hwnd;
    let already = MOUSE_HOOK
        .get()
        .map(|m| *m.lock().unwrap() != 0)
        .unwrap_or(false);
    if already {
        return;
    }
    unsafe {
        // `WH_MOUSE_LL` requires `hmod == NULL` (the hook lives in this
        // process) and `dw_thread_id == 0` (the calling / GUI thread, which is
        // the one pumping messages so the callback fires).
        let hhk = SetWindowsHookExW(WH_MOUSE_LL, mouse_proc as *const (), 0, 0);
        if hhk != 0 {
            *MOUSE_HOOK.get_or_init(|| Mutex::new(0)).lock().unwrap() = hhk;
        }
    }
}

/// Whether `hwnd` still refers to a live window. Used by the show/hide toggle so
/// a window that GPUI has since destroyed is re-created instead of left pointing
/// at a dead handle (which would make the toggle do nothing).
pub fn is_window_alive(hwnd: isize) -> bool {
    if hwnd == 0 {
        return false;
    }
    unsafe { IsWindow(hwnd) != 0 }
}

#[link(name = "user32")]
unsafe extern "system" {
    fn EnumWindows(
        lp_enum_func: Option<unsafe extern "system" fn(isize, isize) -> i32>,
        l_param: isize,
    ) -> i32;
    fn GetWindowThreadProcessId(hwnd: isize, lpdw_process_id: *mut u32) -> u32;
}

/// TokenMonitor's own data directory (`%APPDATA%\TokenMonitor`).
pub fn app_data_dir() -> Result<PathBuf> {
    let dir = dirs::data_dir()
        .map(|p| p.join("TokenMonitor"))
        .context("resolve OS data directory")?;
    Ok(dir)
}

/// Legacy `rToken` data directory (`%APPDATA%\rToken`) from before the rename.
/// Consulted only for the one-time database migration in
/// `storage::migrate_legacy_db`.
pub fn legacy_data_dir() -> Result<PathBuf> {
    dirs::data_dir()
        .map(|p| p.join("rToken"))
        .context("resolve OS data directory")
}

pub fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().context("resolve home directory")
}

/// Whether the app is running from a portable (免安装) layout rather than an
/// MSI install. The portable zip ships a `.portable` marker and `README.md`
/// next to the exe; the MSI installs only the exe into `bin\`.
pub fn is_portable() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let Some(dir) = exe.parent() else {
        return false;
    };
    dir.join(".portable").is_file() || dir.join("README.md").is_file()
}

/// Open a path in Windows Explorer.
pub fn open_path_in_explorer(path: &Path) -> Result<()> {
    std::process::Command::new("explorer.exe")
        .arg(path)
        .spawn()
        .context("spawn explorer.exe")?;
    Ok(())
}

/// Launch a downloaded Windows installer (an `.msi`) through the Windows
/// Installer wizard. Spawns and returns immediately so the app can quit.
pub fn launch_installer(path: &Path) -> Result<()> {
    std::process::Command::new("msiexec.exe")
        .arg("/i")
        .arg(path)
        .spawn()
        .context("spawn msiexec")?;
    Ok(())
}

/// Force the native titlebar into dark mode so it matches TokenMonitor's dark panels
/// regardless of the OS theme. GPUI sizes the titlebar by the system
/// appearance; this overrides it with `DWMWA_USE_IMMERSIVE_DARK_MODE`.
pub fn apply_dark_titlebar() {
    unsafe {
        EnumWindows(
            Some(apply_dark_to_process_window),
            std::process::id() as isize,
        );
    }
}

unsafe extern "system" fn apply_dark_to_process_window(hwnd: isize, l_param: isize) -> i32 {
    let mut window_pid: u32 = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut window_pid);
        if window_pid == l_param as u32 {
            set_immersive_dark_mode(hwnd);
        }
    }
    1 // keep enumerating
}

unsafe fn set_immersive_dark_mode(hwnd: isize) {
    const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
    // Windows 10 before 20H1 used attribute 19 for the same toggle.
    const DWMWA_USE_IMMERSIVE_DARK_MODE_LEGACY: u32 = 19;
    let enabled: c_int = 1;
    let size = std::mem::size_of::<c_int>() as u32;
    unsafe {
        if DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &enabled, size) != 0 {
            DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE_LEGACY, &enabled, size);
        }
    }
}
