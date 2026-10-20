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
    /// The client area's top-left in **screen** px. Together with `GetWindowRect`
    /// it gives the client's origin *inside the window* — the space dock snapping
    /// and the drag maths work in.
    fn ClientToScreen(hwnd: isize, lppoint: *mut Point) -> i32;
    fn GetDpiForWindow(hwnd: isize) -> u32;
    fn GetTickCount() -> u32;
    /// Marks the whole client rect invalid so the next frame's present cannot
    /// be reduced to a damage sub-rect (see [`invalidate_whole_window`]).
    fn InvalidateRect(hwnd: isize, lprect: *const Rect, berase: i32) -> i32;
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
/// `SetWindowPos` flag that reruns the frame path — the nudge that makes DWM
/// recomposite a window whose composition queue it has dropped (see
/// [`nudge_dwm_recomposition`]).
const SWP_FRAMECHANGED: u32 = 0x0020;
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
/// Anything drawn inside the window must be laid out against this same
/// rectangle — otherwise the ball and the window it sits in are centred on two
/// different rectangles. That mismatch is invisible in the arithmetic but very
/// visible on screen: the window edge then shaves one side of the ball (a hard,
/// aliased rim) and eats the glow on exactly that side.
///
/// GPUI's `Window::viewport_size()` is *not* a safe substitute. It reports the
/// size GPUI believes the window has, which can disagree with the real client
/// area (DPI rounding, a popup the OS sized differently). Deriving the ball from
/// the viewport instead of the client rect is what produced the one-sided cut.
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

/// The client rect's size in **device px**. `None` when there is no
/// believable client area to measure (the handle is gone, or the window has
/// not been laid out yet).
///
/// The view compares this against the size a frame asked for, so a client rect
/// that changed size behind the app's back — a DPI change resizing the window
/// when the ball is dragged across monitors, an OS-clamped resize — is visible
/// as a mismatch rather than as a silently wrong layout.
pub fn client_size_device(hwnd: isize) -> Option<(i32, i32)> {
    if hwnd == 0 {
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
    let w = rc.right - rc.left;
    let h = rc.bottom - rc.top;
    (w > 0 && h > 0).then_some((w, h))
}

// ── Custom drag via Win32 subclass ───────────────────────────────────────────
//
// The ball is dragged by a Win32 subclass on the ball HWND. The subclass owns the
// whole interaction, so it does not depend on GPUI's `WindowControlArea::Drag` or
// on the ball's client area being hit-testable:
//
// 1. `WM_NCHITTEST` → the subclass answers `HTCAPTION` on the ball and
//    `HTTRANSPARENT` everywhere else. `HTTRANSPARENT` is what makes clicks
//    outside the ball fall through to whatever is underneath (the 360 / Thunder
//    look) without any Win32 region; `HTCAPTION` forces the OS to route a press
//    as a **non-client** `WM_NCLBUTTONDOWN` instead of a client
//    `WM_LBUTTONDOWN`. That non-client path is the one we could prove reaches
//    the window on this transparent popup; the client path is not relied on.
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
// ball's circle pass through to the desktop via the subclass's `WM_NCHITTEST`
// (`HTTRANSPARENT` beyond the ball) + the transparent window style (the
// 360 / Thunder look).

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
/// `HTTRANSPARENT` (-1): the hit-test answer that makes the OS hand the mouse
/// message to whatever is underneath the point — the click-through half of the
/// ball's shape, now that no `SetWindowRgn` carves the window.
const HTTRANSPARENT: isize = -1;
/// Slack (logical px) the hit-test circle keeps beyond the card's own radius,
/// so the clickable area matches the visible halo rather than just the solid
/// disc — the same generosity the old region's glow room gave.
const HIT_SLACK: f32 = 12.0;
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
    *FLOATING_REFRESH_HIT
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap() = rect;
}

/// `InitCommonControls` must run once before `SetWindowSubclass` is usable
/// (comctl32 subclasses are uninitialised until then on some Windows builds).
static COMCTL_INIT: OnceLock<()> = OnceLock::new();

/// Window-geometry messages worth a line in the frame log. These are the only
/// witnesses to the OS resizing or moving the window **behind the app's back** —
/// `WM_DPICHANGED` (dragged across the monitor boundary) rewrites the window rect
/// with the system's suggested size, and `WM_SIZE` is what GPUI re-syncs its
/// viewport and swap chain from. When the ball comes back sliced by a sharp
/// rectangle, the question is always "what did the client rect do between the
/// frame that laid the ball out and the frame that cut the region", and only
/// these messages can answer it.
const WM_MOVE: u32 = 0x0003;
const WM_SIZE: u32 = 0x0005;
const WM_WINDOWPOSCHANGING: u32 = 0x0046;
const WM_WINDOWPOSCHANGED: u32 = 0x0047;
const WM_DPICHANGED: u32 = 0x02E0;

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
            // **The shape is the hit test.** There is no `SetWindowRgn` any
            // more, so the subclass draws the boundary itself: the drag path
            // (`HTCAPTION`) on the ball's own circle, `HTTRANSPARENT` beyond
            // it — which is what makes clicks outside the ball reach whatever
            // is underneath, the 360 / Thunder look, without any region.
            // This stays the reliable way to make a press reach the window on
            // this transparent popup; it does not depend on GPUI's control
            // areas.
            if let Some(hit) = ball_hit_test(hwnd, lparam) {
                return hit;
            }
            // No believable geometry yet (first frames, DPI churn): keep the
            // whole window on the drag path rather than dropping presses.
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
                if let Some((rl, rt, rr, rb)) =
                    FLOATING_REFRESH_HIT.get().and_then(|m| *m.lock().unwrap())
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
            let is_double = have_pos && {
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
                // The drop decides the form. Released against a screen edge →
                // snap the card flush and dock (GPUI morphs the ball into its
                // square); anywhere else → free again (the circle comes back).
                // The snap is done here, in the message that ends the drag, so
                // the ball is never left visibly off the edge for a frame;
                // only the shape decision crosses into GPUI.
                let edge = dock_and_snap(hwnd);
                crate::platform::windows::tray::send_tray_command(
                    crate::platform::windows::tray::TrayCommand::Dock(edge),
                );
                // Swallow the release so the OS caption loop (if any) is closed.
                return 0;
            }
        }
        WM_MOVE | WM_SIZE | WM_WINDOWPOSCHANGING | WM_WINDOWPOSCHANGED | WM_DPICHANGED => {
            // Log only (never swallow): the geometry messages still have to reach
            // GPUI's own window proc through `DefSubclassProc` below.
            frame_log(&format!(
                "msg={msg:#06x} dragging={} window={:?} client={:?}",
                *DRAGGING.get_or_init(|| Mutex::new(false)).lock().unwrap(),
                window_rect(hwnd),
                client_size_device(hwnd),
            ));
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

// ── Docking against a screen edge ────────────────────────────────────────────
//
// Dropping the ball against the **left / right** edge of the screen docks it:
// the window is snapped so the *card* — not the window, which carries the card's
// glow room on each side, plus the non-client frame of its own, and is merely
// centred on it — comes to rest `DOCK_GAP` px inside that edge, and the view
// morphs into its square form. Dropping it anywhere else un-docks, and the circle
// comes back.
//
// Two things make this work without asking GPUI first:
//
// 1. The card's size is **published** by the view as a fraction of the **client
//    area** (`FLOATING_CARD_FRAC`, the same trick as the refresh icon's
//    hit-rect), so the Win32 side can translate "the card's own edge" into screen
//    px on its own — adding the client's own origin inside the window, since that
//    is what separates the drawn card from the window rect. The window is the
//    card plus its glow's room, so snapping on the *window* rect would leave the
//    card sitting that room inland from the edge.
// 2. The snap happens *here*, in the button-up that ends the drag, so there is
//    no frame in which the ball is visibly off the edge. Only the shape
//    decision (square vs circle) crosses into GPUI, as `TrayCommand::Dock`.

/// The screen edge a dropped ball docked against. Only the left and right edges
/// dock: the ball is a card, and a card docked to a side reads as a widget,
/// while one pinned under the taskbar would read as a clipped window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockEdge {
    Left,
    Right,
}

/// Gap (px at 100% DPI) the docked card keeps from the screen edge.
pub const DOCK_GAP: f32 = 10.0;
/// How close (px at 100% DPI) the card's own edge has to come to a screen edge
/// for the drop to dock.
const DOCK_THRESHOLD: f32 = 32.0;

/// Which edge a card occupying `card_left..card_right` is docked against inside
/// the work area `work_left..work_right`, with `threshold` px of slack; `None`
/// means it was dropped free and the ball stays a circle.
///
/// The distances are signed, so a card dragged *past* an edge — its own edge
/// already outside the work area — is simply a more negative distance and still
/// docks. When both edges are within reach (a work area narrower than the card)
/// the nearer one wins, which is what `<=` on both comparisons buys.
pub fn dock_edge_for(
    card_left: f32,
    card_right: f32,
    work_left: f32,
    work_right: f32,
    threshold: f32,
) -> Option<DockEdge> {
    let threshold = threshold.max(0.0);
    let from_left = card_left - work_left;
    let from_right = work_right - card_right;
    if from_left <= threshold && from_left <= from_right {
        Some(DockEdge::Left)
    } else if from_right <= threshold {
        Some(DockEdge::Right)
    } else {
        None
    }
}

/// Window x (device px) that leaves a card of `card` px — centred in a window
/// carrying `margin` px on each side of it — `gap` px inside `edge`.
pub fn dock_window_x(
    edge: DockEdge,
    work_left: f32,
    work_right: f32,
    card: f32,
    margin: f32,
    gap: f32,
) -> i32 {
    match edge {
        DockEdge::Left => (work_left + gap - margin).round() as i32,
        DockEdge::Right => (work_right - gap - card - margin).round() as i32,
    }
}

/// Size of the drawn card (the circle, or the docked square) as a fraction of
/// the window's smaller side, published by `FloatingView` on every render.
///
/// The published value is deliberately the **resting** card, not the hovered
/// one: the snap would otherwise depend on whether the pointer happened to be
/// over the ball at the moment it was dropped.
static FLOATING_CARD_FRAC: OnceLock<Mutex<f32>> = OnceLock::new();

/// Publish the card's size as a fraction of the window (see
/// [`FLOATING_CARD_FRAC`]). Called from `FloatingView`'s render.
pub fn set_floating_card_frac(frac: f32) {
    *FLOATING_CARD_FRAC
        .get_or_init(|| Mutex::new(0.5))
        .lock()
        .unwrap() = frac.clamp(0.0, 1.0);
}

/// The published card fraction, floored to something sane: a 0 (or a NaN) would
/// make "the card's edge" degenerate back into the window's edge and dock the
/// card a hundred pixels inland.
fn floating_card_frac() -> f32 {
    let frac = FLOATING_CARD_FRAC
        .get()
        .map(|m| *m.lock().unwrap())
        .unwrap_or(0.5);
    if frac.is_finite() {
        frac.clamp(0.05, 1.0)
    } else {
        0.5
    }
}

/// The hit-test half of the ball's shape: `Some(HTCAPTION)` on the ball's own
/// circle, `Some(HTTRANSPARENT)` beyond it, `None` when no believable geometry
/// exists yet.
///
/// The circle is the **card** (the published [`FLOATING_CARD_FRAC`] of the
/// smaller client side) plus a [`HIT_SLACK`] of halo, centred on the client
/// rect — the same rectangle the card is drawn in, so the two can never drift
/// apart at any DPI. `lparam` carries the query point in **screen** px; it is
/// mapped through the window rect into client space before the distance test.
///
/// # Safety
/// `hwnd` must be a live window handle; the Win32 calls are ordinary.
unsafe fn ball_hit_test(hwnd: isize, lparam: isize) -> Option<isize> {
    let x = (lparam & 0xFFFF) as u16 as i32 as f32;
    let y = ((lparam >> 16) & 0xFFFF) as u16 as i32 as f32;
    // Screen-negative coordinates (a monitor left of / above the primary)
    // arrive sign-extended; the u16→i32 round trip above keeps them negative.
    let x = if x > 32767.0 { x - 65536.0 } else { x };
    let y = if y > 32767.0 { y - 65536.0 } else { y };

    let mut wr = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if GetWindowRect(hwnd, &mut wr) == 0 || wr.right <= wr.left || wr.bottom <= wr.top {
        return None;
    }
    let dpi = GetDpiForWindow(hwnd);
    let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
    let (lw, lh) = client_size_logical(hwnd, scale)?;
    // The card's radius, in logical px, from the fraction the last render
    // published — plus the halo slack, so the clickable circle matches what is
    // visible rather than just the solid disc.
    let card = lw.min(lh) * floating_card_frac();
    let r = card / 2.0 + HIT_SLACK;
    // The press point in window px → logical client px. The client's origin
    // inside the window (`client_origin_in_window`) converts between the two;
    // the card is centred on the client, so the centre is client/2.
    let (ox, oy) = client_origin_in_window(hwnd);
    let px_logical = (x - wr.left as f32 - ox as f32) / scale;
    let py_logical = (y - wr.top as f32 - oy as f32) / scale;
    let cx = lw / 2.0;
    let cy = lh / 2.0;
    let dx = px_logical - cx;
    let dy = py_logical - cy;
    Some(if dx * dx + dy * dy <= r * r {
        HTCAPTION
    } else {
        HTTRANSPARENT
    })
}

#[link(name = "user32")]
unsafe extern "system" {
    fn MonitorFromWindow(hwnd: isize, flags: u32) -> isize;
    fn GetMonitorInfoW(monitor: isize, info: *mut MonitorInfo) -> i32;
}

/// `MONITORINFO` — `rc_work` is the part of the monitor the shell leaves to
/// applications, i.e. the taskbar / dock is already excluded from it.
#[repr(C)]
struct MonitorInfo {
    cb_size: u32,
    rc_monitor: Rect,
    rc_work: Rect,
    dw_flags: u32,
}

/// `MONITOR_DEFAULTTONEAREST`: the monitor the window is closest to, which is
/// the one whose work area it has to dock inside.
const MONITOR_DEFAULTTONEAREST: u32 = 2;

/// Work area of the monitor nearest to the window, in **device** px:
/// `(left, top, right, bottom)`.
pub fn monitor_work_area(hwnd: isize) -> Option<(i32, i32, i32, i32)> {
    if hwnd == 0 {
        return None;
    }
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if monitor == 0 {
            return None;
        }
        let mut info: MonitorInfo = std::mem::zeroed();
        info.cb_size = std::mem::size_of::<MonitorInfo>() as u32;
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return None;
        }
        Some((
            info.rc_work.left,
            info.rc_work.top,
            info.rc_work.right,
            info.rc_work.bottom,
        ))
    }
}

/// The window's rectangle in device px: `(left, top, right, bottom)`.
pub fn window_rect(hwnd: isize) -> Option<(i32, i32, i32, i32)> {
    if hwnd == 0 {
        return None;
    }
    let mut rc = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if unsafe { GetWindowRect(hwnd, &mut rc) } == 0 {
        return None;
    }
    Some((rc.left, rc.top, rc.right, rc.bottom))
}

/// Move the window to `(x, y)` in device px. Size, z-order and focus all stay
/// as they are — including always-on-top, which is what `SWP_NOZORDER` is for
/// (and why its value has to be `0x0004`; see the drag code).
pub fn move_window(hwnd: isize, x: i32, y: i32) {
    if hwnd == 0 {
        return;
    }
    unsafe {
        SetWindowPos(
            hwnd,
            0,
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Slide the window sideways by `dx` px — size, z-order and activation all
/// untouched — keeping an in-flight drag's grab offset in sync, exactly as
/// [`fit_floating_card_window`] does.
///
/// This is what puts a just-landed morph back on its snap: the morph freezes the
/// window (see `ui::floating::morph_envelope`), so the card is drawn centred and
/// its outer edge drifts off the snapped position by half the card's own size
/// change. One slide at the landing frame restores it, where the per-frame
/// resize used to pay fifteen `SetWindowPos` calls for the same guarantee.
pub fn shift_floating_window(hwnd: isize, dx: i32) {
    if hwnd == 0 || dx == 0 {
        return;
    }
    let Some((rect_l, rect_t, _rect_r, _rect_b)) = window_rect(hwnd) else {
        return;
    };
    unsafe {
        SetWindowPos(
            hwnd,
            0,
            rect_l + dx,
            rect_t,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
    // The origin just moved, and an in-flight drag holds a grab offset measured
    // from it — so move the offset by the same amount, or the ball would jump
    // under the cursor the moment the drag's next move resizes the window.
    if *DRAGGING.get_or_init(|| Mutex::new(false)).lock().unwrap() {
        let mut offset = DRAG_OFFSET
            .get_or_init(|| Mutex::new((0, 0)))
            .lock()
            .unwrap();
        offset.0 -= dx;
    }
}

/// Where the client area's origin sits **inside the window**, in device px.
///
/// This window is not frameless — it carries a caption and a sizing border — so
/// the client rect is not the window rect. `GetClientRect` always reports its
/// origin as `0,0`, which is why the offset has to come from `ClientToScreen`
/// minus `GetWindowRect` instead.
///
/// The distinction matters because the ball is laid out in **client** px while
/// window moves (`SetWindowPos`) are measured from the **window's** top-left:
/// any screen-space maths that mixes the two has to cancel this offset first.
pub fn client_origin_in_window(hwnd: isize) -> (i32, i32) {
    if hwnd == 0 {
        return (0, 0);
    }
    let mut origin = Point { x: 0, y: 0 };
    let mut wr = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe {
        if ClientToScreen(hwnd, &mut origin) == 0 || GetWindowRect(hwnd, &mut wr) == 0 {
            return (0, 0);
        }
    }
    (origin.x - wr.left, origin.y - wr.top)
}

/// The non-client frame's size in device px: `GetWindowRect - GetClientRect`,
/// per axis. `(0, 0)` on a frameless window.
///
/// `SetWindowPos` sizes the **window**, while everything the floating ball is
/// laid out against — and clipped against — is the **client area**. Asking for a
/// client area of `s` px therefore means asking for a window of `s + frame` px.
fn window_frame_size(hwnd: isize) -> (i32, i32) {
    let mut wr = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let mut cr = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe {
        if GetWindowRect(hwnd, &mut wr) == 0 || GetClientRect(hwnd, &mut cr) == 0 {
            return (0, 0);
        }
    }
    (
        (wr.right - wr.left) - (cr.right - cr.left),
        (wr.bottom - wr.top) - (cr.bottom - cr.top),
    )
}

/// Force a **full-window** repaint on the next pass: the whole client rect is
/// marked invalid, so the next frame's present cannot be reduced to a damage
/// sub-rect. A window whose surface shows a stale *partial* frame only
/// recovers when the entire surface is re-fetched, which is exactly what a
/// full invalidation asks of the compositor.
pub fn invalidate_whole_window(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    unsafe {
        // `None` = the whole client rect; `false` = no WM_ERASEBKGND (the
        // surface is GPUI-owned, an erase would flash black).
        InvalidateRect(hwnd, std::ptr::null(), 0);
    }
}

/// Force DWM to recomposite the window: a zero `SetWindowPos` that moves
/// nothing, sizes nothing, and only reruns the frame path.
///
/// Why this exists: a frame the app draws and presents is not necessarily a
/// frame the compositor shows. A background, always-on-top, transparent window
/// (this ball) can have its DirectComposition visual dropped from DWM's
/// recomposition queue — the frame log then shows correct content presented
/// every heartbeat while the screen keeps the old pixels, until some
/// activation change (a click anywhere) shames the compositor into catching
/// up. The nudge is the programmatic equivalent of that click.
///
/// **Safe at rest, dangerous mid-transition.** With the region retired the
/// window is at every instant a plain transparent rectangle — there is no
/// bracket, no transient open-region-over-black-backbuffer state for the
/// recomposition to sample (that is exactly how an earlier mid-render nudge
/// froze a black window onto the screen, 2026-10-20, see plan §9.28). At rest
/// the only state to sample is the settled, correct one.
pub fn nudge_dwm_recomposition(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    unsafe {
        SetWindowPos(
            hwnd,
            0,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
    }
}

/// Append one timestamped line to `%TEMP%\tm-frame.log` **while
/// `%TEMP%\tm-frame.on` exists`**.
///
/// The flag file is the switch, not an environment variable: reproducing the
/// drag-out slice needs no relaunch gymnastics and no rebuild — create the file,
/// drag the ball out of its dock once, delete the file, and the log holds every
/// frame's geometry and every window message the OS sent in between. That is
/// the difference between guessing at a desync (the layout box and the client
/// rect disagreeing) and watching it happen: a static ball paints nothing, so
/// only the message side of the log sees the moment the OS resizes the window
/// behind the app's back.
pub fn frame_log(msg: &str) {
    let dir = std::env::temp_dir();
    if !dir.join("tm-frame.on").exists() {
        return;
    }
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("tm-frame.log"))
    {
        let _ = writeln!(file, "{} {msg}", unsafe { GetTickCount() });
    }
}

/// Append one line to `%TEMP%\tokenmonitor-dock-debug.log` when
/// `TOKENMONITOR_FLOAT_DEBUG` is set, and do nothing otherwise.
///
/// The drop decision mixes three coordinate spaces — the window rect, the client
/// rect and the card's own fraction of the client — and a mistake in any of them
/// is indistinguishable from the outside (the ball just does not dock). Getting
/// the numbers out beats inferring them, the same way the `(8, 31)` client-origin
/// offset had to be measured rather than reasoned about.
fn dock_debug(msg: &str) {
    if std::env::var_os("TOKENMONITOR_FLOAT_DEBUG").is_none() {
        return;
    }
    use std::io::Write as _;
    let path = std::env::temp_dir().join("tokenmonitor-dock-debug.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{msg}");
    }
}

/// The window's DPI scale (`1.0` when the OS cannot say).
pub fn window_scale(hwnd: isize) -> f32 {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 {
        1.0
    } else {
        dpi as f32 / 96.0
    }
}

/// Where the window's origin has to move to when its client area is resized to
/// `side` px, so that the **card** stays where it was on screen.
///
/// The card sits `(1 - frac) / 2` of the window in from each edge, so keeping any
/// of its edges still means moving the origin by the *difference* the resize makes
/// to that inset — which is why every case below is a difference of two boxes:
///
/// * `None` (free): the card's **centre** stays put, so the ball grows and shrinks
///   around itself.
/// * `Some(Left)` / `Some(Right)`: the card's **outer edge** stays put.
///
/// Docked, that pinned edge is the one the snap measured against the screen edge,
/// so a docked square can shrink (the morph) or pop (hover) without ever sliding
/// along the edge it is docked against — and without the window having to repeat
/// the snap. Vertically the card is always centred, so `y` moves by half the size
/// change whatever the anchor.
///
/// Two fractions rather than one: `frac_from` is the card's share of the window
/// being left, `frac_to` its share of the window being asked for. They coincide
/// only when the card does **not** change size along with the window — and the
/// morph is precisely the case where it does, since the card shrinks by
/// `DOCK_SIDE_RATIO` while the window shrinks by `clip_diameter_for`.
///
/// `frac_from` has to describe the card that is **on screen right now**, that is
/// the fraction the view published on its *previous* frame. Publishing the new
/// fraction before resizing would anchor on the card this frame is about to draw
/// instead: every frame would chase a card that has already shrunk, and the pinned
/// edge would drift inwards by half the card's own shrink each time — 15 frames of
/// morph, tens of px, and a docked square nowhere near its `DOCK_GAP`. (Which is
/// why the view resizes first and publishes after.)
pub fn fitted_origin(
    origin: (i32, i32),
    cur_side: (f32, f32),
    side: f32,
    frac_from: f32,
    frac_to: f32,
    anchor: Option<DockEdge>,
) -> (i32, i32) {
    let from = frac_from.clamp(0.0, 1.0);
    let to = frac_to.clamp(0.0, 1.0);
    // The card's box inside a window of `s`, measured from that window's top-left.
    let box_of = |s: f32, f: f32| (s * (1.0 - f) / 2.0, s * (1.0 + f) / 2.0);
    let (cur_l, cur_r) = box_of(cur_side.0, from);
    let (new_l, new_r) = box_of(side, to);
    let dx = match anchor {
        None => (cur_l + cur_r - new_l - new_r) / 2.0,
        Some(DockEdge::Left) => cur_l - new_l,
        Some(DockEdge::Right) => cur_r - new_r,
    };
    (
        (origin.0 as f32 + dx).round() as i32,
        // Vertically the card is centred on every form, so its own centre is
        // always `s / 2` and the two fractions cancel out.
        (origin.1 as f32 + (cur_side.1 - side) / 2.0).round() as i32,
    )
}

/// Resize the floating window's client area to `side` logical px **without
/// moving its origin** — the plain shrink/grow the morph's take-off and
/// landing use, in place of [`fit_floating_card_window`]'s anchored move.
///
/// Why the anchored move is wrong exactly there: the window's surface
/// content travels with the window origin, so a `SetWindowPos` that moves
/// the origin leaves the *old* frame's pixels at their old **window**
/// coordinates — the anchored fit was designed in screen coordinates (it puts
/// the new client's centre where the old card's centre was on screen) and
/// moves the origin by the half-change to do it, which slides the still-shown
/// old content off-centre *relative to the client rect*: the client shrinks to
/// 154 while the old card sits at the old 360-layout's centre (188, 211), and
/// the compositor can sample any part of that ~30 ms gap (measured twice that
/// long by the fit's convergence re-check, `tm-frame.log` 2026-10-19
/// 19:41:47.5). The slice of the old card left inside the new client is the
/// quarter-arc shard.
///
/// The layout-centre walk in `ui::floating` removes the need to move at all:
/// by the time this runs, the view has already drawn the card at the *target*
/// form's client centre, so a plain resize leaves the old pixels sitting
/// exactly at the new client's centre — every intermediate state is whole.
/// `fit_floating_card_window` (the anchored move) stays for the at-rest
/// resizes: hover pops and DPI changes, where the card is centred in the
/// client it is resized inside and the client stays larger than the card
/// either way.
///
/// No-op when the client already matches: the resting frames of an animation
/// cost nothing. Returns the client size (device px) the window ended up
/// with; `None` when no rectangle could be read (nothing was touched).
pub fn resize_floating_window_plain(hwnd: isize, side: f32, scale: f32) -> Option<(i32, i32)> {
    if hwnd == 0 || !side.is_finite() || side <= 0.0 || !(scale > 0.0) || !scale.is_finite() {
        return None;
    }
    let (rect_l, rect_t, _, _) = window_rect(hwnd)?;
    let want_client = ((side * scale).round() as i32).max(1);
    let client = client_size_device(hwnd)?;
    if (client.0 - want_client).abs() < 1 && (client.1 - want_client).abs() < 1 {
        return Some(client);
    }
    let frame = window_frame_size(hwnd);
    frame_log(&format!(
        "plain: side={side:.1} want_client={want_client} frame={frame:?} \
         cur_client={client:?} -> window=({:?}, {:?})",
        want_client + frame.0,
        want_client + frame.1
    ));
    unsafe {
        SetWindowPos(
            hwnd,
            0,
            rect_l,
            rect_t,
            want_client + frame.0,
            want_client + frame.1,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
    client_size_device(hwnd)
}

/// Resize the floating window so its client area is `side` logical px across —
/// the card plus the room its glow needs (`ui::floating::window_side_for`) —
/// keeping the card where it is on screen (see [`fitted_origin`]).
///
/// Called from the view's render, which is the only place that knows how big the
/// card is. Docked, the window is sized to the card rather than the card being
/// laid out inside a fixed window, because the empty margin of a fixed window is
/// invisible but not inert: docked, it hangs a hundred-odd pixels off the screen
/// edge, and it swallows hover and press events nowhere near the ball. Free, the
/// view passes the side the window already has, so the call is a no-op — a circle
/// is never resized (see the module note in `ui::floating`).
///
/// **The test is the client area, not the window rect.** The frame between the
/// two is a difference of two live rects, and the OS does not update them in the
/// same instant: a DPI change rewrites the window rect before the non-client
/// metrics catch up, and `WM_DPICHANGED`'s suggested rect resizes the window
/// behind the app's back when the ball is dragged across the DPI boundary
/// between the two monitors. A window that already measures `side + frame` can
/// therefore still hold a client area the card is *not* laid out in — and since
/// the ball is centred in the client rect while the region is cut from it, a
/// client area that disagrees with `side` puts the ball's centre at the client
/// rect's own corner, where the OS slices a quarter of the ball off. (Measured:
/// the slice is a sharp rectangle, not the region's rounded square, and it heals
/// only when something finally re-measures.)
///
/// So: measure the client, resize by the exact deficit, measure again. Each pass
/// closes the gap, so this is one `SetWindowPos` in the common case (the client
/// already matches — every resting frame) and at most a few when the OS left it
/// somewhere else. The loop is bounded, and it re-reads the DPI and the frame
/// every pass because the resize itself can deliver the `WM_DPICHANGED` that
/// changes them.
///
/// `card_d` is the card's side in the same logical px, and it is what makes the
/// anchoring exact: the card's share of the window being *left* is the published
/// [`floating_card_frac`], its share of the window being *asked for* is
/// `card_d / side`, and the morph changes both at once — so the two halves of
/// that pair cannot be collapsed into one number (see [`fitted_origin`]).
/// `scale` is gpui's scale factor for the window — the same one the view lays
/// the card out in. It is passed in rather than re-read from `GetDpiForWindow`
/// because the two disagree precisely during a cross-monitor drag: gpui applies
/// the new factor *before* `WM_DPICHANGED`'s suggested rect has been honoured,
/// and a window whose DPI has not caught up yet would be resized against the
/// old scale — the "layout box is a fraction of the real client" mismatch that
/// put the ball's centre on the client rect's corner on 2026-10-14.
///
/// **Unreadable client rect → no move at all.** The window-rect fallback that
/// used to stand here computed the frame as *the whole window rect* (client
/// empty ⇒ frame ≈ window size), which made `want` absurd and risked resizing
/// the window to a phantom geometry. Skipping the frame is strictly safer: the
/// next render re-measures and re-fits.
///
/// **Returns the client size (device px) the window ended up with** — `None` only
/// when no client rect could be read at all. The caller must lay the card out in
/// *this* rectangle rather than re-measuring: a second `GetClientRect` is a second
/// instant, and the window can be resized again inside it (a `WM_DPICHANGED`
/// suggested rect, a queued `SetWindowPos`), which is how the 2026-10-14 "ball
/// clipped to a corner" frame was built — the layout used one rectangle, the
/// region was cut from another, and the memo then recorded the cut as correct.
pub fn fit_floating_card_window(
    hwnd: isize,
    side: f32,
    scale: f32,
    card_d: f32,
    anchor: Option<DockEdge>,
) -> Option<(i32, i32)> {
    if hwnd == 0 || !side.is_finite() || side <= 0.0 || !(scale > 0.0) || !scale.is_finite() {
        return None;
    }
    for _ in 0..4 {
        let Some((rect_l, rect_t, rect_r, rect_b)) = window_rect(hwnd) else {
            return None;
        };
        let want_client = ((side * scale).round() as i32).max(1);
        let Some(client) = client_size_device(hwnd) else {
            // Window mid-creation (or the OS has not published a size yet):
            // there is no trustworthy rectangle to reason about, so do nothing.
            return None;
        };
        if (client.0 - want_client).abs() < 1 && (client.1 - want_client).abs() < 1 {
            return Some(client);
        }
        let frame = window_frame_size(hwnd);
        let cur = (rect_r - rect_l, rect_b - rect_t);
        let want = (want_client + frame.0, want_client + frame.1);
        frame_log(&format!(
            "fit: side={side:.1} card={card_d:.1} want_client={want_client} \
             frame={frame:?} cur_window={cur:?} cur_client={client:?} -> window={want:?}"
        ));
        // Anchored on the **client** sizes, because that is the rectangle the card
        // is centred in and the one `floating_card_frac` is a fraction of. The
        // frame is a constant offset between the window's origin and the client's,
        // so moving the window origin by the client-size delta moves the card by
        // exactly that delta.
        //
        // The card's share of the window being left is the fraction the view just
        // published against this same rect; its share of the window being asked
        // for is `card_d / want_client`. They differ whenever the card resizes
        // together with the window (the morph, hover), and only passing both keeps
        // the pinned edge still.
        let frac_to = if want_client > 0 {
            card_d / want_client as f32
        } else {
            floating_card_frac()
        };
        let (x, y) = fitted_origin(
            (rect_l, rect_t),
            (client.0 as f32, client.1 as f32),
            want_client as f32,
            floating_card_frac(),
            frac_to,
            anchor,
        );
        unsafe {
            SetWindowPos(hwnd, 0, x, y, want.0, want.1, SWP_NOZORDER | SWP_NOACTIVATE);
        }
        // The origin just moved, and an in-flight drag holds a grab offset
        // measured from it — so move the offset by the same amount, or the ball
        // would jump under the cursor the moment a hover (or a morph) resizes the
        // window.
        if *DRAGGING.get_or_init(|| Mutex::new(false)).lock().unwrap() {
            let mut offset = DRAG_OFFSET
                .get_or_init(|| Mutex::new((0, 0)))
                .lock()
                .unwrap();
            offset.0 += rect_l - x;
            offset.1 += rect_t - y;
        }
    }
    client_size_device(hwnd)
}

/// The published card's on-screen geometry, as the dock/snap math needs it:
/// `(frac, card, margin, scale)` — the fraction the view currently publishes,
/// the card's device-px size, and the distance from the window's left edge to
/// the card's left edge (client origin + centring inset).
///
/// One source for both readers — the drop-time edge decision
/// ([`dock_and_snap`]) and the post-landing re-snap
/// ([`snap_floating_window_to_edge`]) — so the two cannot grow two different
/// opinions about where the card's edge is.
fn dock_card_geometry(hwnd: isize) -> Option<(f32, f32, f32, f32)> {
    let scale = window_scale(hwnd);
    let (client_w, client_h) = client_size_logical(hwnd, scale)?;
    let side = client_w.min(client_h);
    if !(side > 0.0) {
        return None;
    }
    // The card is centred in the **client** area (`ui::floating::ball_box`), and
    // `floating_card_frac` is the card's share of that same rectangle — so each
    // of the card's edges sits `margin` px inside the corresponding *client*
    // edge, and the client in turn starts `client_origin_in_window` px inside
    // the window. Measuring against the window rect instead would place the
    // phantom card a frame's worth inward of the real one, and the snap would
    // then leave that much more than `DOCK_GAP` against the screen edge.
    let frac = floating_card_frac();
    let card = side * frac * scale;
    let (origin_x, _) = client_origin_in_window(hwnd);
    let margin = origin_x as f32 + (client_w - side * frac) / 2.0 * scale;
    Some((frac, card, margin, scale))
}

/// Snap the window so its card comes to rest inside the nearest dockable screen
/// edge, and report which edge that was — or `None` when the card was dropped
/// free. Called from the drag subclass's button-up, so the snap lands in the
/// same message that ends the drag.
fn dock_and_snap(hwnd: isize) -> Option<DockEdge> {
    let (area_l, _area_t, area_r, _area_b) = monitor_work_area(hwnd)?;
    let (rect_l, rect_t, _rect_r, _rect_b) = window_rect(hwnd)?;
    let Some((frac, card, margin, scale)) = dock_card_geometry(hwnd) else {
        dock_debug("drop: no client rect");
        return None;
    };
    let card_left = rect_l as f32 + margin;
    let card_right = card_left + card;
    let edge = dock_edge_for(
        card_left,
        card_right,
        area_l as f32,
        area_r as f32,
        DOCK_THRESHOLD * scale,
    );
    dock_debug(&format!(
        "drop: window=({rect_l},{rect_t}) frac={frac:.4} card={card:.1} margin={margin:.1} \
         card_x={card_left:.1}..{card_right:.1} work_x=({area_l}..{area_r}) edge={edge:?}"
    ));
    let edge = edge?;
    snap_floating_window_to_edge(hwnd, edge);
    Some(edge)
}

/// Move the floating window (position only — never resized) so the card, as
/// measured from the fraction the view publishes **right now**, comes to rest
/// `DOCK_GAP` device px inside `edge`'s work-area boundary.
///
/// Two callers, one job:
///
/// * the drag subclass's button-up, via [`dock_and_snap`] — the snap at the
///   moment of the drop; and
/// * the view's first frame **after** a docking morph lands
///   (`ui::floating`'s `pending_snap`) — the re-snap with the card the morph
///   actually landed on.
///
/// The second caller is the point. The drop-time snap has to estimate the card
/// from the pre-morph fraction, while the landed card is `DOCK_SIDE_RATIO` of
/// that estimate — knowingly too big, which is what left the rest gap ~7 px
/// wide of `DOCK_GAP` (measured, 2026-10-12). Re-snapping once the published
/// fraction describes the settled card measures the card that is really on
/// screen, and the edge lands on the gap by construction.
///
/// A position-only move is safe at **any** instant, including between a resize
/// and the frame that repaints for it: the region and the drawn content both
/// travel with the window, so their intersection with the client area is
/// unchanged — there is no intermediate state for the compositor to catch.
pub fn snap_floating_window_to_edge(hwnd: isize, edge: DockEdge) -> bool {
    let Some((area_l, _area_t, area_r, _area_b)) = monitor_work_area(hwnd) else {
        return false;
    };
    let Some((rect_l, rect_t, _rect_r, _rect_b)) = window_rect(hwnd) else {
        return false;
    };
    let Some((frac, card, margin, scale)) = dock_card_geometry(hwnd) else {
        return false;
    };
    let x = dock_window_x(
        edge,
        area_l as f32,
        area_r as f32,
        card,
        margin,
        DOCK_GAP * scale,
    );
    dock_debug(&format!(
        "snap: edge={edge:?} frac={frac:.4} card={card:.1} margin={margin:.1} \
         x={x} (was {rect_l})"
    ));
    move_window(hwnd, x, rect_t);
    true
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1920-wide work area, the everyday case (`rcWork` starts at 0 because
    /// the taskbar is at the bottom).
    const WORK: (f32, f32) = (0.0, 1920.0);

    /// Docking is decided by the **card's own** edge. The window is ~2.7x the
    /// card, so a card sitting mid-screen must not dock just because the window
    /// behind it has already crossed the screen edge — which is exactly what
    /// snapping on the window rect would have done.
    #[test]
    fn docking_is_decided_by_the_card_edge_not_the_window() {
        let (left, right) = WORK;
        // Card just inside the left edge, and dragged straight past it.
        assert_eq!(
            dock_edge_for(5.0, 130.0, left, right, 32.0),
            Some(DockEdge::Left)
        );
        assert_eq!(
            dock_edge_for(-80.0, 45.0, left, right, 32.0),
            Some(DockEdge::Left)
        );
        // Just inside the right edge.
        assert_eq!(
            dock_edge_for(1780.0, 1905.0, left, right, 32.0),
            Some(DockEdge::Right)
        );
        // Inland by more than the threshold: dropped free, the circle stays.
        assert_eq!(dock_edge_for(200.0, 325.0, left, right, 32.0), None);
        assert_eq!(dock_edge_for(1500.0, 1625.0, left, right, 32.0), None);
    }

    /// With both edges in reach (a work area narrower than the card) the nearer
    /// edge wins, so the snap never flings the ball across the screen.
    #[test]
    fn the_nearer_edge_wins_when_both_are_in_reach() {
        // Work area 0..100, card 20..145: 20 px from the left, -45 from the right.
        assert_eq!(
            dock_edge_for(20.0, 145.0, 0.0, 100.0, 32.0),
            Some(DockEdge::Right)
        );
        assert_eq!(
            dock_edge_for(-45.0, 60.0, 0.0, 100.0, 32.0),
            Some(DockEdge::Left)
        );
    }

    /// The snap puts the **card** a gap inside the edge; the window — the card
    /// plus the room its glow needs, merely centred on it — therefore ends up
    /// hanging off the screen by exactly that room.
    #[test]
    fn snap_leaves_the_card_a_gap_inside_the_edge() {
        // A 170 px window with a 130 px card: 20 px of window each side.
        let (card, margin, gap) = (130.0, 20.0, 10.0);
        let left = dock_window_x(DockEdge::Left, 0.0, 1920.0, card, margin, gap);
        assert_eq!(left as f32 + margin, 10.0, "card starts 10 px in");
        assert_eq!(left, -10);
        let right = dock_window_x(DockEdge::Right, 0.0, 1920.0, card, margin, gap);
        assert_eq!(right as f32 + margin + card, 1910.0, "card ends 10 px in");
    }

    /// **The card must not move while the window is resized around it.** The
    /// window follows the card's size, and the card changes size on the morph, on
    /// hover and whenever a scan changes the usage total — all of which happen
    /// while the ball is on screen. Every one of those resizes has to leave the
    /// card exactly where it was: centred on itself while free, and pinned by its
    /// outer edge while docked against a screen edge (the edge the snap measured).
    #[test]
    fn resizing_the_window_keeps_the_card_where_it_was() {
        // What `ui::floating::window_side_for` actually publishes: the card over
        // the window. The card's box inside a window of `side` is then
        // `side * (1 -/+ frac) / 2`.
        for frac in [0.63_f32, 0.66] {
            let box_of = |side: f32| (side * (1.0 - frac) / 2.0, side * (1.0 + frac) / 2.0);
            let origin = (1000, 500);
            for (cur, want) in [
                (200.0_f32, 260.0_f32), // grew: a scan found more usage
                (260.0, 200.0),         // shrank: the count came down
                (117.0, 170.0),         // the docked square's window
                (300.0, 300.0),         // no change at all
            ] {
                let (before_l, before_r) = box_of(cur);
                let (after_l, after_r) = box_of(want);
                for anchor in [None, Some(DockEdge::Left), Some(DockEdge::Right)] {
                    let (x, y) = fitted_origin(origin, (cur, cur), want, frac, frac, anchor);
                    let dx = (x - origin.0) as f32;
                    let (kept, held) = match anchor {
                        None => ((before_l + before_r) / 2.0, dx + (after_l + after_r) / 2.0),
                        Some(DockEdge::Left) => (before_l, dx + after_l),
                        Some(DockEdge::Right) => (before_r, dx + after_r),
                    };
                    assert!(
                        (kept - held).abs() <= 0.51,
                        "{anchor:?} moved the card: it was at {kept}, now at {held} \
                         (cur {cur}, want {want}, frac {frac})"
                    );
                    // Vertically the card is centred whatever the anchor.
                    let dy = (y - origin.1) as f32;
                    assert!(
                        (dy - (cur - want) / 2.0).abs() <= 0.51,
                        "the vertical anchor must always be the card's centre"
                    );
                }
            }
        }
    }

    /// The one case where the card's share of the window is **not** constant: the
    /// morph shrinks the card by `DOCK_SIDE_RATIO` while the window shrinks by
    /// `clip_diameter_for`, so `frac_from` and `frac_to` are different numbers.
    /// Anchoring on a single fraction slides the card along the docked edge, which
    /// is what left the square a few px off its `DOCK_GAP`.
    #[test]
    fn the_morph_keeps_the_docked_cards_outer_edge() {
        // Real numbers from a ~165M-token run at 125% DPI: a 179.5px ball (hovered)
        // morphing into a 111.3px square inside a 173px client area.
        let cur = 360.0_f32;
        let card_from = 179.5_f32;
        let card_to = 111.3_f32;
        let side = 173.0_f32;
        let origin = (3000, 400);
        let (from, to) = (card_from / cur, card_to / side);

        for anchor in [Some(DockEdge::Left), Some(DockEdge::Right)] {
            let (x, _) = fitted_origin(origin, (cur, cur), side, from, to, anchor);
            let dx = (x - origin.0) as f32;
            let (kept, held) = match anchor {
                Some(DockEdge::Left) => ((cur - card_from) / 2.0, dx + (side - card_to) / 2.0),
                _ => ((cur + card_from) / 2.0, dx + (side + card_to) / 2.0),
            };
            assert!(
                (kept - held).abs() <= 0.51,
                "{anchor:?} slid the outer edge: it was at {kept}, now at {held}"
            );
            // The single-fraction shortcut this replaces drifts by
            // `cur * (from - to) / 2` — several px, i.e. a visibly wrong gap.
            let single = (cur - side) * (1.0 + from) / 2.0;
            assert!(
                (single - dx).abs() > 1.0,
                "expected the single-fraction shortcut to drift, got {}",
                (single - dx).abs()
            );
        }
    }

    /// The morph is a **sequence** of resizes, not one: the card shrinks while the
    /// window shrinks, so every frame has to anchor on the card the previous frame
    /// left on screen. Chained with the fraction published *after* each resize the
    /// pinned edge stays put; chained with the fraction published *before* it (the
    /// order this used to be in) it drifts inwards by half the card's own shrink,
    /// every single frame.
    #[test]
    fn a_run_of_morph_frames_keeps_the_docked_edge() {
        // Real frames from a ~165M-token run: a 180.2px hovered ball morphing into
        // a 105.4px square, each paired with the client side the view asks for.
        let frames = [
            (360.0_f32, 180.2_f32),
            (356.0, 177.3),
            (333.0, 164.0),
            (278.0, 141.6),
            (206.0, 119.9),
            (174.0, 111.7),
            (165.0, 105.4),
        ];
        // `origin_x` of the floating window, for turning "relative to the window"
        // back into screen px.
        const ORIGIN_X: f32 = 8.0;
        let start = 1000.0_f32 + ORIGIN_X + (frames[0].0 + frames[0].1) / 2.0;
        let edge_of = |origin: (i32, i32), side: f32, card: f32| {
            origin.0 as f32 + ORIGIN_X + (side + card) / 2.0
        };

        // Right order: resize, then publish the fraction of the client area the
        // card was actually laid out in.
        let mut good = (1000, 400);
        let mut published = frames[0].1 / frames[0].0;
        for w in frames.windows(2) {
            let (cur, _on_screen) = (w[0].0, w[0].1);
            let (side, card) = (w[1].0, w[1].1);
            let (x, _) = fitted_origin(
                good,
                (cur, cur),
                side,
                published,
                card / side,
                Some(DockEdge::Right),
            );
            good.0 = x;
            published = card / side;
        }
        let (side, card) = frames[frames.len() - 1];
        let good_edge = edge_of(good, side, card);
        assert!(
            (good_edge - start).abs() <= 0.51,
            "the pinned edge drifted: it started at {start}, ended at {good_edge}"
        );

        // Wrong order — publish first, then anchor on it. Same inputs, same maths,
        // one transposition: the edge walks inwards by half the shrink each frame.
        let mut bad = (1000, 400);
        for w in frames.windows(2) {
            let (cur, _) = (w[0].0, w[0].1);
            let (side, card) = (w[1].0, w[1].1);
            let (x, _) = fitted_origin(
                bad,
                (cur, cur),
                side,
                card / cur,
                card / side,
                Some(DockEdge::Right),
            );
            bad.0 = x;
        }
        let bad_edge = edge_of(bad, side, card);
        assert!(
            (start - bad_edge).abs() > 5.0,
            "expected the old order to drift, it moved {} px",
            (start - bad_edge).abs()
        );
    }

    /// The published fraction is what turns "the card's edge" into screen px,
    /// so a degenerate value must not survive: 0 would collapse it back onto
    /// the window edge and dock the card a hundred pixels inland.
    #[test]
    fn published_card_fraction_is_always_sane() {
        set_floating_card_frac(0.0);
        assert_eq!(floating_card_frac(), 0.05);
        set_floating_card_frac(f32::NAN);
        assert_eq!(
            floating_card_frac(),
            0.5,
            "NaN must fall back, not propagate"
        );
        set_floating_card_frac(0.42);
        assert!((floating_card_frac() - 0.42).abs() < 1e-6);
    }
}
