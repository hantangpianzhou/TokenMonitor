use std::os::raw::c_int;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};

pub mod autostart;
pub use autostart::{autostart_enabled, set_autostart};

pub mod single_instance;
pub use single_instance::{acquire_single_instance, activate_running_instance};

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
/// `GetSystemMetrics` index for the system double-click width (px). Used as the
/// distance tolerance when timing the ball's two presses into a double-click.
const SM_CXDOUBLECLK: c_int = 36;

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

// ── Custom drag via Win32 subclass ───────────────────────────────────────────
//
// The ball is dragged by a Win32 subclass on the ball HWND. The subclass owns the
// whole interaction, so it does not depend on GPUI's `WindowControlArea::Drag` or
// on the ball's client area being hit-testable:
//
// 1. `WM_NCHITTEST` → the subclass answers `HTCAPTION` for every point. Every
//    point the OS can hit is inside the circular window region (see
//    `set_window_circle_region`), i.e. the ball, so an unconditional `HTCAPTION`
//    is correct — clicks outside the circle never reach this window at all. This
//    forces the OS to route a press as a **non-client** `WM_NCLBUTTONDOWN`
//    instead of a client `WM_LBUTTONDOWN`. That non-client path is the one we
//    could prove reaches the window on this transparent popup; the client path
//    is not relied on.
// 2. On `WM_NCLBUTTONDOWN` (or the client `WM_LBUTTONDOWN` fallback) the subclass
//    calls `SetCapture`, records the cursor→window offset, and **swallows** the
//    message (`return 0`). Swallowing is what stops `DefWindowProc` from starting
//    the OS caption-move loop — that loop, on a transparent frameless window with
//    no visible caption, is what flashed the black square.
// 3. From then on the OS delivers move messages to the captured window — as
//    `WM_NCMOUSEMOVE` while it stays in non-client mode, or `WM_MOUSEMOVE` once it
//    crosses into the client area. The subclass handles **both** and relocates the
//    window with `SetWindowPos`, so the ball follows the cursor. (Handling only one
//    of the two is what made earlier attempts look "stuck": the drag began on the
//    non-client path, so every move arrived as `WM_NCMOUSEMOVE`.)
// 4. `WM_NCLBUTTONUP` / `WM_LBUTTONUP` release the capture.
// 5. A double-click opens the main window: the first press starts a drag, and
//    the second press (still within `GetDoubleClickTime` and the same spot) is
//    detected either by timing the two presses in `WM_NCLBUTTONDOWN` or by the
//    OS `WM_*BUTTONDBLCLK` message — both call `show_main_window` (the same
//    path the tray's left-click uses) and cancel the in-progress drag so the
//    ball never moves. So a quick double-click brings the app forward while a
//    press-and-drag repositions the ball.
//
// `SetWindowSubclass` (not `SetWindowLongPtrW`) composes with GPUI's own window
// proc instead of replacing it, so the window keeps receiving every other
// message (hover, show/hide, paint) and stays responsive. Clicks outside the
// ball's circle pass through to the desktop via `SetWindowRgn` + the transparent
// window style (the 360 / Thunder look).

#[link(name = "user32")]
unsafe extern "system" {
    fn GetWindowRect(hwnd: isize, lprect: *mut Rect) -> i32;
    fn IsWindow(hwnd: isize) -> i32;
    fn GetCursorPos(lp_point: *mut Point) -> i32;
    fn SetCapture(hwnd: isize) -> isize;
    fn ReleaseCapture() -> i32;
    // Double-click detection for the floating ball: time the two presses
    // ourselves (robust whether or not the window class carries CS_DBLCLKS).
    fn GetMessageTime() -> i32;
    fn GetDoubleClickTime() -> u32;
    fn GetSystemMetrics(n_index: c_int) -> c_int;
}

#[link(name = "comctl32")]
unsafe extern "system" {
    fn InitCommonControls();
    fn SetWindowSubclass(
        hwnd: isize,
        pfn: *const (),
        u_id_subclass: usize,
        dw_ref_data: usize,
    ) -> i32;
    fn DefSubclassProc(hwnd: isize, msg: u32, wparam: usize, l_param: isize) -> isize;
}

#[repr(C)]
struct Point {
    x: i32,
    y: i32,
}

const WM_NCHITTEST: u32 = 0x0084;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_NCLBUTTONDOWN: u32 = 0x00A1;
const WM_NCLBUTTONUP: u32 = 0x00A2;
const WM_NCMOUSEMOVE: u32 = 0x00A0;
// Double-click messages (the OS synthesises these when the window class carries
// `CS_DBLCLKS`; otherwise we detect the double-click by timing two presses).
const WM_LBUTTONDBLCLK: u32 = 0x0203;
const WM_NCLBUTTONDBLCLK: u32 = 0x00A3;
/// `HTCAPTION` (2): the hit-test answer that makes the OS treat a press as a
/// non-client caption press, delivering `WM_NCLBUTTONDOWN`.
const HTCAPTION: isize = 2;
/// `SWP_NOZORDER` is `0x0004`. Note `0x0002` is **`SWP_NOMOVE`** — using the
/// wrong value here silently turns the drag into a no-op: `SetWindowPos` still
/// succeeds and returns non-zero, but with `SWP_NOMOVE` set it ignores the x/y
/// and the window never moves. That exact mistake is what made several earlier
/// "the messages all arrive but the ball won't budge" attempts fail.
const SWP_NOZORDER: u32 = 0x0004;
const SWP_NOACTIVATE: u32 = 0x0010;
/// Unique id for this subclass (arbitrary; just must not collide with GPUI's
/// own subclass ids).
const DRAG_SUBCLASS_ID: usize = 0xF10A;

/// `(offset_x, offset_y)` = cursor minus window origin captured at drag start.
static DRAG_OFFSET: OnceLock<Mutex<(i32, i32)>> = OnceLock::new();

/// Whether a left-button drag is in progress (the captured window is following
/// the cursor).
static DRAGGING: OnceLock<Mutex<bool>> = OnceLock::new();

/// `(message_time, cursor_x, cursor_y)` of the previous left-button press on the
/// ball. Used to detect a double-click by comparing the current press against it
/// (time within `GetDoubleClickTime`, position within `SM_CXDOUBLECLK`). `None`
/// until the first press.
static LAST_CLICK: OnceLock<Mutex<Option<(i32, i32, i32)>>> = OnceLock::new();

/// Hit-rect (logical px, relative to the ball window's client rect, top-left
/// origin) of the refresh icon. Written by `FloatingView` on every render and
/// read by `ball_drag_subclass` so a press on the icon routes to a refresh
/// (via `TrayCommand::Refresh`) instead of a drag or a double-click-open. `None`
/// until the first render.
static FLOATING_REFRESH_HIT: OnceLock<Mutex<Option<(f32, f32, f32, f32)>>> = OnceLock::new();

/// Publish the refresh icon's hit-rect (see [`FLOATING_REFRESH_HIT`]) so the
/// ball's Win32 subclass can tell a click on the icon apart from a drag. Called
/// from `FloatingView`'s render.
pub fn set_floating_refresh_hit(rect: Option<(f32, f32, f32, f32)>) {
    *FLOATING_REFRESH_HIT.get_or_init(|| Mutex::new(None)).lock().unwrap() = rect;
}

/// `InitCommonControls` must run once before `SetWindowSubclass` is usable
/// (comctl32 subclasses are uninitialised until then on some Windows builds).
static COMCTL_INIT: OnceLock<()> = OnceLock::new();

unsafe extern "system" fn ball_drag_subclass(
    hwnd: isize,
    msg: u32,
    wparam: usize,
    lparam: isize,
    _u_id_subclass: usize,
    _dw_ref_data: usize,
) -> isize {
    match msg {
        WM_NCHITTEST => {
            // Answer `HTCAPTION` so the OS routes a press here as a non-client
            // `WM_NCLBUTTONDOWN`. Every hit-testable point of this window is
            // inside the circular region (the ball), so this is unconditional.
            // This is the reliable way to make the press reach the window on
            // this transparent popup; it does not depend on GPUI's control areas.
            return HTCAPTION;
        }
        WM_NCLBUTTONDOWN | WM_LBUTTONDOWN => {
            // Double-click → open the main window. We time the two presses
            // ourselves so this works whether or not the window class carries
            // `CS_DBLCLKS`; the `WM_*BUTTONDBLCLK` branch below handles the case
            // where the OS *does* synthesise that message. Every unsafe fn call
            // here is in the `unsafe extern fn` body (not a closure), so it is a
            // valid unsafe context.
            let now = GetMessageTime();
            let mut cur = Point { x: 0, y: 0 };
            let have_pos = GetCursorPos(&mut cur) != 0;
            // Refresh-icon hit-test takes priority over both the drag and the
            // double-click-open: a press on the icon must refresh, never move or
            // open the window. `FLOATING_REFRESH_HIT` is the icon's rect in the
            // window's logical px (written by `FloatingView`); we map the press
            // from screen px into that same space via the window rect and the
            // client size, so the hit stays aligned at any DPI / window size.
            if have_pos {
                if let Some((rl, rt, rr, rb)) = FLOATING_REFRESH_HIT
                    .get()
                    .and_then(|m| *m.lock().unwrap())
                {
                    let dpi = GetDpiForWindow(hwnd);
                    let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
                    if let Some((lw, lh)) = client_size_logical(hwnd, scale) {
                        let mut wrc = Rect {
                            left: 0,
                            top: 0,
                            right: 0,
                            bottom: 0,
                        };
                        if GetWindowRect(hwnd, &mut wrc) != 0 {
                            let pw = (wrc.right - wrc.left) as f32;
                            let ph = (wrc.bottom - wrc.top) as f32;
                            if pw > 0.0 && ph > 0.0 {
                                let lx = (cur.x - wrc.left) as f32 / pw * lw;
                                let ly = (cur.y - wrc.top) as f32 / ph * lh;
                                if lx >= rl && lx <= rr && ly >= rt && ly <= rb {
                                    crate::platform::windows::tray::send_tray_command(
                                        crate::platform::windows::tray::TrayCommand::Refresh,
                                    );
                                    return 0;
                                }
                            }
                        }
                    }
                }
            }
            let tol = GetSystemMetrics(SM_CXDOUBLECLK).max(1);
            let dbl_time = GetDoubleClickTime();
            let is_double = have_pos
                && {
                    let mut last = LAST_CLICK.get_or_init(|| Mutex::new(None)).lock().unwrap();
                    let hit = match *last {
                        Some((t, x, y))
                            if now.abs_diff(t) <= dbl_time
                                && (cur.x - x).abs() <= tol
                                && (cur.y - y).abs() <= tol =>
                        {
                            true
                        }
                        _ => false,
                    };
                    *last = Some((now, cur.x, cur.y));
                    hit
                };
            if is_double {
                // Cancel the drag the first press of the pair started, then open
                // the main window. Swallow so the OS caption loop never runs.
                *DRAGGING.get_or_init(|| Mutex::new(false)).lock().unwrap() = false;
                ReleaseCapture();
                crate::platform::windows::tray::show_main_window();
                return 0;
            }
            // Begin a drag. `WM_NCLBUTTONDOWN` is the normal path (we answered
            // `HTCAPTION` above); the client `WM_LBUTTONDOWN` is kept as a
            // fallback. Capture the pointer so every later move message
            // (`WM_NCMOUSEMOVE` or `WM_MOUSEMOVE`) is routed to this window even
            // after the cursor leaves the ball, and record the grab offset so the
            // ball does not jump under the cursor.
            let mut pt = Point { x: 0, y: 0 };
            let mut rc = Rect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if GetCursorPos(&mut pt) != 0 && GetWindowRect(hwnd, &mut rc) != 0 {
                *DRAG_OFFSET
                    .get_or_init(|| Mutex::new((0, 0)))
                    .lock()
                    .unwrap() = (pt.x - rc.left, pt.y - rc.top);
                *DRAGGING.get_or_init(|| Mutex::new(false)).lock().unwrap() = true;
                SetCapture(hwnd);
            }
            // Swallow the press so `DefWindowProc` never starts the OS caption
            // move loop (the black square). Clients never see it either.
            return 0;
        }
        WM_MOUSEMOVE | WM_NCMOUSEMOVE => {
            if *DRAGGING.get_or_init(|| Mutex::new(false)).lock().unwrap() {
                let mut pt = Point { x: 0, y: 0 };
                if GetCursorPos(&mut pt) != 0 {
                    let (ox, oy) = *DRAG_OFFSET
                        .get_or_init(|| Mutex::new((0, 0)))
                        .lock()
                        .unwrap();
                    // Move the whole window so the cursor stays on the same point
                    // of the ball. `SWP_NOSIZE` keeps the size, `SWP_NOACTIVATE`
                    // avoids stealing focus mid-drag. `SWP_NOZORDER` keeps
                    // always-on-top — and its value must be `0x0004`: `0x0002`
                    // is `SWP_NOMOVE`, which would make this call a silent no-op.
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
        WM_LBUTTONUP | WM_NCLBUTTONUP => {
            if *DRAGGING.get_or_init(|| Mutex::new(false)).lock().unwrap() {
                *DRAGGING.get_or_init(|| Mutex::new(false)).lock().unwrap() = false;
                ReleaseCapture();
                // Swallow the release so the OS caption loop (if any) is closed.
                return 0;
            }
        }
        WM_LBUTTONDBLCLK | WM_NCLBUTTONDBLCLK => {
            // The OS synthesised a double-click (window class has `CS_DBLCLKS`):
            // open the main window directly. Cancel any drag the first press
            // started and swallow, so the caption double-click is never treated
            // as maximise/restore of this frameless popup.
            *DRAGGING.get_or_init(|| Mutex::new(false)).lock().unwrap() = false;
            ReleaseCapture();
            crate::platform::windows::tray::show_main_window();
            return 0;
        }
        _ => {}
    }
    // Chain to GPUI (and any other subclass) through `DefSubclassProc`. This is
    // the correct forwarder for `SetWindowSubclass`: it preserves GPUI's own
    // WndProc and any subclass GPUI installed, so hover, click-through and
    // show/hide keep flowing. Replacing the wndproc with `SetWindowLongPtrW`
    // (tried earlier) clobbered GPUI's WndProc on the first show/hide cycle and
    // froze the window — that is why we use `SetWindowSubclass`.
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

/// Subclass the floating ball HWND so it can be dragged with the native pointer
/// (see the module note above). Idempotent per hwnd. Call once at creation,
/// right after the window handle is known.
///
/// Uses `SetWindowSubclass` (not `SetWindowLongPtrW`) on purpose: it composes
/// with GPUI's own window proc instead of replacing it, so the window stays
/// responsive and the subclass survives show/hide cycles.
pub fn install_ball_drag(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    COMCTL_INIT.get_or_init(|| unsafe { InitCommonControls() });
    unsafe {
        SetWindowSubclass(hwnd, ball_drag_subclass as *const (), DRAG_SUBCLASS_ID, 0);
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
