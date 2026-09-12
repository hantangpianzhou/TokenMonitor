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
    }
}

// ── Custom drag ──────────────────────────────────────────────────────────────
//
// The floating ball is moved by a Win32 subclass on its own `HWND` instead of
// GPUI's `WindowControlArea::Drag`. `Drag` lets `DefWindowProc` run the OS
// caption-move loop; during that loop the app's frame pump is blocked and the
// OS either draws its hollow drag-outline (a full window-sized **square**, when
// "show window contents while dragging" is off) or a black frame while the
// transparent window is not being re-presented — that is the black square the
// user sees. Driving the move ourselves (capture the pointer, track it with
// `GetCursorPos`, relocate with `SetWindowPos`) keeps GPUI rendering the live
// circular ball every frame and never enters the OS move loop, so no black
// square. Clicks still pass through outside the ball because `SetWindowRgn`
// makes the OS return `HTTRANSPARENT` for the corners.

#[link(name = "user32")]
unsafe extern "system" {
    fn SetWindowLongPtrW(hwnd: isize, nindex: i32, dwnewlong: isize) -> isize;
    fn GetWindowLongPtrW(hwnd: isize, nindex: i32) -> isize;
    fn CallWindowProcW(
        lp_prev_wnd_func: isize,
        hwnd: isize,
        msg: u32,
        wparam: usize,
        lparam: isize,
    ) -> isize;
    fn DefWindowProcW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;
    fn GetCursorPos(lp_point: *mut Point) -> i32;
    fn SetCapture(hwnd: isize) -> isize;
    fn ReleaseCapture() -> i32;
    fn GetWindowRect(hwnd: isize, lprect: *mut Rect) -> i32;
}

#[repr(C)]
struct Point {
    x: i32,
    y: i32,
}

const GWLP_WNDPROC: i32 = -4;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONUP: u32 = 0x0202;
const SWP_NOZORDER: u32 = 0x0002;
const SWP_NOACTIVATE: u32 = 0x0010;

/// Original `WndProc` per subclassed floating window (so we can chain every
/// message back to GPUI after handling the drag ones).
static ORIG_WNDPROC: OnceLock<Mutex<HashMap<isize, isize>>> = OnceLock::new();
/// `(offset_x, offset_y)` = cursor minus window origin at drag start, per hwnd.
static DRAG_OFFSET: OnceLock<Mutex<HashMap<isize, (i32, i32)>>> = OnceLock::new();

unsafe extern "system" fn ball_drag_wndproc(
    hwnd: isize,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    match msg {
        WM_LBUTTONDOWN => {
            // Begin a drag from anywhere inside the clipped ball (clicks in the
            // transparent corners never reach here — `SetWindowRgn` routes them
            // to the desktop). Record the grab offset so the ball does not jump.
            let mut pt = Point { x: 0, y: 0 };
            let mut rc = Rect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if GetCursorPos(&mut pt) != 0 && GetWindowRect(hwnd, &mut rc) != 0 {
                DRAG_OFFSET
                    .get_or_init(|| Mutex::new(HashMap::new()))
                    .lock()
                    .unwrap()
                    .insert(hwnd, (pt.x - rc.left, pt.y - rc.top));
                SetCapture(hwnd);
            }
        }
        WM_MOUSEMOVE => {
            if let Some((ox, oy)) = DRAG_OFFSET
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .unwrap()
                .get(&hwnd)
                .copied()
            {
                let mut pt = Point { x: 0, y: 0 };
                if GetCursorPos(&mut pt) != 0 {
                    // Move the whole window so the cursor stays on the same
                    // point of the ball. `SWP_NOZORDER` keeps the always-on-top
                    // state; `SWP_NOACTIVATE` avoids stealing focus mid-drag.
                    SetWindowPos(
                        hwnd,
                        0,
                        pt.x - ox,
                        pt.y - oy,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
            }
        }
        WM_LBUTTONUP => {
            if DRAG_OFFSET
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .unwrap()
                .remove(&hwnd)
                .is_some()
            {
                ReleaseCapture();
            }
        }
        _ => {}
    }
    // Chain every message (including the ones we handled) back to GPUI so hover
    // and click handling keep working.
    let orig = ORIG_WNDPROC
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .get(&hwnd)
        .copied()
        .unwrap_or(0);
    if orig == 0 {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    } else {
        CallWindowProcW(orig, hwnd, msg, wparam, lparam)
    }
}

/// Subclass the floating window so it can be dragged with the native pointer
/// (see the module note above). Idempotent per hwnd. Call once at creation,
/// right after the window handle is known.
pub fn install_ball_drag(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    let mut map = ORIG_WNDPROC
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    if map.contains_key(&hwnd) {
        return; // already subclassed
    }
    unsafe {
        let prev = GetWindowLongPtrW(hwnd, GWLP_WNDPROC);
        if prev == 0 {
            return;
        }
        map.insert(hwnd, prev);
        SetWindowLongPtrW(hwnd, GWLP_WNDPROC, ball_drag_wndproc as *const () as isize);
    }
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
