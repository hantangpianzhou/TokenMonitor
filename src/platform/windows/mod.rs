use std::os::raw::c_int;
use std::path::{Path, PathBuf};

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
