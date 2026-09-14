//! Windows system-tray icon.
//!
//! GPUI has no tray-icon API, so this adds a Win32 `Shell_NotifyIconW` icon
//! whose callback messages are routed to the main window through a subclassed
//! window proc (`SetWindowLongPtrW` + `CallWindowProcW`). It runs entirely on
//! the GPUI main thread — no background message loop, no comctl32 v6 manifest
//! dependency — and forwards every other message to GPUI's original proc.
//!
//! Behavior:
//! - Left-click:  show / restore the main window.
//! - Right-click: context menu (显示/隐藏主窗口 / 显示/隐藏悬浮窗 / 退出).
//! - Close (X):   hide to tray; quit from the tray menu.
//!
//! Two menu items cross into the GPUI app as a `TrayCommand`, because each needs
//! something only the app can do: `Floating` creates / toggles the ball window,
//! and `Quit` tears the **whole** application down (`App::quit`). Quitting by
//! closing just the main window leaves the ball behind: GPUI auto-quits only
//! once its *last* window closes, and the ball is a second top-level window.
//! Plain window show/hide stays pure Win32.

use std::mem;
use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicUsize, Ordering::Relaxed};
use std::sync::OnceLock;

use async_channel::Sender;

// Win32 message / constant values.
const WM_CLOSE: u32 = 0x0010;
const WM_DESTROY: u32 = 0x0002;
const WM_COMMAND: u32 = 0x0111;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_LBUTTONDBLCLK: u32 = 0x0203;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_CONTEXTMENU: u32 = 0x007b;

const SW_HIDE: i32 = 0;
const SW_RESTORE: i32 = 9;

const NIM_ADD: u32 = 0;
const NIM_DELETE: u32 = 2;

const NIF_MESSAGE: u32 = 0x01;
const NIF_ICON: u32 = 0x02;
const NIF_TIP: u32 = 0x04;

const MF_STRING: u32 = 0x0000;
const TPM_RIGHTBUTTON: u32 = 0x0002;

const GWLP_WNDPROC: i32 = -4;

const TRAY_ID: u32 = 1;
const TRAY_CMD_TOGGLE_WINDOW: usize = 1;
const TRAY_CMD_EXIT: usize = 2;
const TRAY_CMD_FLOATING: usize = 3;

/// Commands the tray icon sends to the GPUI app.
///
/// Only these two need to cross into GPUI: creating / toggling the floating
/// window, and quitting the application as a whole. Window show/hide stays pure
/// Win32.
#[derive(Clone, Copy)]
pub enum TrayCommand {
    Floating,
    Quit,
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn Shell_NotifyIconW(dw_message: u32, lp_data: *mut NotifyIconDataW) -> i32;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn SetWindowLongPtrW(hwnd: isize, n_index: i32, dw_new_long: isize) -> isize;
    fn CallWindowProcW(prev: isize, hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;
    fn DefWindowProcW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;
    fn ShowWindow(hwnd: isize, n_cmd_show: i32) -> i32;
    fn SetForegroundWindow(hwnd: isize) -> i32;
    fn IsWindowVisible(hwnd: isize) -> i32;
    fn GetCursorPos(point: *mut Point) -> i32;
    fn CreatePopupMenu() -> isize;
    fn AppendMenuW(menu: isize, flags: u32, id_new_item: usize, new_item: *const u16) -> i32;
    fn TrackPopupMenu(
        menu: isize,
        flags: u32,
        x: i32,
        y: i32,
        reserved: i32,
        hwnd: isize,
        rect: *mut c_void,
    ) -> i32;
    fn DestroyMenu(menu: isize) -> i32;
    fn PostMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> i32;
    fn RegisterWindowMessageW(name: *const u16) -> u32;
    fn LoadIconW(instance: isize, name: *const u16) -> isize;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> isize;
}

#[repr(C)]
struct NotifyIconDataW {
    cb_size: u32,
    hwnd: isize,
    u_id: u32,
    u_flags: u32,
    u_callback_message: u32,
    h_icon: isize,
    sz_tip: [u16; 128],
    dw_state: u32,
    dw_state_mask: u32,
    sz_info: [u16; 256],
    u_version_or_timeout: u32,
    sz_info_title: [u16; 64],
    dw_info_flags: u32,
    guid_item: [u8; 16],
    h_balloon_icon: isize,
}

#[repr(C)]
struct Point {
    x: i32,
    y: i32,
}

static MAIN_HWND: OnceLock<isize> = OnceLock::new();
static TRAY_CALLBACK_MSG: AtomicU32 = AtomicU32::new(0);
static TASKBAR_CREATED_MSG: AtomicU32 = AtomicU32::new(0);
static PREV_WND_PROC: AtomicUsize = AtomicUsize::new(0);
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);

/// Sends tray menu commands into the GPUI app's event loop.
static TRAY_CMD_TX: OnceLock<Sender<TrayCommand>> = OnceLock::new();
/// Native handle of the floating window (0 = not yet created).
static FLOATING_HWND: AtomicIsize = AtomicIsize::new(0);
/// Whether the floating window is currently visible.
static FLOATING_VISIBLE: AtomicBool = AtomicBool::new(false);

/// Install the tray icon and hook the main window. Call once, on the main
/// thread, after the GPUI window exists. No-op on subsequent calls.
pub fn start_tray(main_hwnd: isize) {
    if STARTED.swap(true, Relaxed) {
        return;
    }
    let _ = MAIN_HWND.set(main_hwnd);
    unsafe {
        let callback = RegisterWindowMessageW(wide("TokenMonitor.TrayNotify").as_ptr());
        let taskbar = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
        TRAY_CALLBACK_MSG.store(callback, Relaxed);
        TASKBAR_CREATED_MSG.store(taskbar, Relaxed);

        let proc: unsafe extern "system" fn(isize, u32, usize, isize) -> isize = main_subclass_proc;
        let prev = SetWindowLongPtrW(main_hwnd, GWLP_WNDPROC, proc as usize as isize);
        PREV_WND_PROC.store(prev as usize, Relaxed);

        add_tray_icon();
    }
    // The window exists now, so later launches can raise it through the
    // single-instance guard instead of opening a second window.
    super::single_instance::listen_for_activation(main_hwnd);
}

/// Hide the main window to the system tray (close-to-tray). Equivalent to the
/// title-bar X, which the subclassed window proc converts into `ShowWindow(SW_HIDE)`
/// unless a quit was requested (see the tray menu's 退出).
pub fn close_window() {
    let Some(&hwnd) = MAIN_HWND.get() else {
        return;
    };
    unsafe {
        PostMessageW(hwnd, WM_CLOSE, 0, 0);
    }
}

/// Hand the tray the sender side of the command channel. Call once, on the
/// main thread, before the tray can forward menu selections to the app.
pub fn init_tray_commands(tx: Sender<TrayCommand>) {
    let _ = TRAY_CMD_TX.set(tx);
}

/// Record the floating window's native handle (called when it is created) so
/// the tray menu and the ball's own close button can show/hide it.
pub fn register_floating_hwnd(hwnd: isize) {
    FLOATING_HWND.store(hwnd, Relaxed);
}

/// Native handle of the floating window, or 0 if it has not been created.
pub fn get_floating_hwnd() -> isize {
    FLOATING_HWND.load(Relaxed)
}

/// Whether the floating window is currently shown.
pub fn is_floating_visible() -> bool {
    FLOATING_VISIBLE.load(Relaxed)
}

/// Update the floating window's visible flag (kept in sync with the ball UI).
pub fn set_floating_visible(visible: bool) {
    FLOATING_VISIBLE.store(visible, Relaxed);
}

/// Forward a tray command into the app's event loop. Returns whether it was
/// queued; `false` means nobody is listening (the receiver is gone, or
/// `init_tray_commands` was never called) so the caller can fall back.
///
/// `try_send`, **not** `send`: `async_channel::Sender::send` returns a future
/// that only enqueues once it is polled, so `let _ = tx.send(cmd)` compiles
/// cleanly and silently sends nothing — which is exactly how the tray's
/// 显示/隐藏悬浮窗 item ended up doing nothing at all. The channel is
/// unbounded, so `try_send` cannot block; it only fails if the receiver is
/// gone (i.e. the app is shutting down).
pub fn send_tray_command(cmd: TrayCommand) -> bool {
    forward_command(TRAY_CMD_TX.get(), cmd)
}

/// The pure half of [`send_tray_command`]: enqueue onto a sender if there is
/// one, and always report whether that worked.
fn forward_command(tx: Option<&Sender<TrayCommand>>, cmd: TrayCommand) -> bool {
    tx.map(|tx| tx.try_send(cmd).is_ok()).unwrap_or(false)
}

unsafe fn add_tray_icon() {
    let Some(&hwnd) = MAIN_HWND.get() else {
        return;
    };
    let callback = TRAY_CALLBACK_MSG.load(Relaxed);
    if callback == 0 {
        return;
    }
    let instance = GetModuleHandleW(std::ptr::null());
    let mut nid: NotifyIconDataW = mem::zeroed();
    nid.cb_size = mem::size_of::<NotifyIconDataW>() as u32;
    nid.hwnd = hwnd;
    nid.u_id = TRAY_ID;
    nid.u_flags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.u_callback_message = callback;
    nid.h_icon = LoadIconW(instance, 1usize as *const u16);
    write_wide_slot(&mut nid.sz_tip, "TokenMonitor - AI 工具 Token 用量追踪");
    Shell_NotifyIconW(NIM_ADD, &mut nid);
}

unsafe fn remove_tray_icon() {
    let Some(&hwnd) = MAIN_HWND.get() else {
        return;
    };
    let mut nid: NotifyIconDataW = mem::zeroed();
    nid.cb_size = mem::size_of::<NotifyIconDataW>() as u32;
    nid.hwnd = hwnd;
    nid.u_id = TRAY_ID;
    Shell_NotifyIconW(NIM_DELETE, &mut nid);
}

unsafe extern "system" fn main_subclass_proc(
    hwnd: isize,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    let tray_msg = TRAY_CALLBACK_MSG.load(Relaxed);
    let taskbar_msg = TASKBAR_CREATED_MSG.load(Relaxed);
    if tray_msg != 0 && msg == tray_msg {
        return on_tray_message(hwnd, lparam);
    }
    if taskbar_msg != 0 && msg == taskbar_msg {
        // Explorer restarted; the shell removed our icon — re-add it.
        add_tray_icon();
        return 0;
    }
    // A second launch handed off through the single-instance guard
    // (`single_instance.rs`) instead of opening its own window / tray icon.
    let activate_msg = super::single_instance::activate_message();
    if activate_msg != 0 && msg == activate_msg {
        show_main_window();
        return 0;
    }
    match msg {
        WM_CLOSE => {
            if QUIT_REQUESTED.load(Relaxed) {
                call_previous(hwnd, msg, wparam, lparam)
            } else {
                // Close-to-tray: hide instead of quitting.
                ShowWindow(hwnd, SW_HIDE);
                0
            }
        }
        WM_COMMAND => match wparam & 0xFFFF {
            TRAY_CMD_TOGGLE_WINDOW => {
                toggle_main_window();
                0
            }
            TRAY_CMD_FLOATING => {
                send_tray_command(TrayCommand::Floating);
                0
            }
            TRAY_CMD_EXIT => {
                quit_app();
                0
            }
            _ => call_previous(hwnd, msg, wparam, lparam),
        },
        WM_DESTROY => {
            remove_tray_icon();
            call_previous(hwnd, msg, wparam, lparam)
        }
        _ => call_previous(hwnd, msg, wparam, lparam),
    }
}

unsafe fn on_tray_message(_hwnd: isize, lparam: isize) -> isize {
    match lparam as u32 {
        WM_RBUTTONUP | WM_CONTEXTMENU => show_context_menu(),
        WM_LBUTTONUP | WM_LBUTTONDBLCLK => show_main_window(),
        _ => {}
    }
    0
}

unsafe fn show_main_window() {
    let Some(&hwnd) = MAIN_HWND.get() else {
        return;
    };
    ShowWindow(hwnd, SW_RESTORE);
    SetForegroundWindow(hwnd);
}

/// Toggle the main window: hide it to tray if visible, otherwise restore it.
unsafe fn toggle_main_window() {
    let Some(&hwnd) = MAIN_HWND.get() else {
        return;
    };
    if IsWindowVisible(hwnd) != 0 {
        ShowWindow(hwnd, SW_HIDE);
    } else {
        ShowWindow(hwnd, SW_RESTORE);
        SetForegroundWindow(hwnd);
    }
}

unsafe fn show_context_menu() {
    let Some(&hwnd) = MAIN_HWND.get() else {
        return;
    };
    let menu = CreatePopupMenu();
    if menu == 0 {
        return;
    }
    // Dynamic labels reflect current visibility state.
    let main_visible = MAIN_HWND
        .get()
        .map(|&h| IsWindowVisible(h) != 0)
        .unwrap_or(false);
    let floating_visible = is_floating_visible();
    let main_label = if main_visible {
        wide("隐藏主窗口")
    } else {
        wide("显示主窗口")
    };
    let floating_label = if floating_visible {
        wide("隐藏悬浮窗")
    } else {
        wide("显示悬浮窗")
    };
    let exit = wide("退出");
    AppendMenuW(menu, MF_STRING, TRAY_CMD_TOGGLE_WINDOW, main_label.as_ptr());
    AppendMenuW(menu, MF_STRING, TRAY_CMD_FLOATING, floating_label.as_ptr());
    AppendMenuW(menu, MF_STRING, TRAY_CMD_EXIT, exit.as_ptr());
    let mut point = Point { x: 0, y: 0 };
    GetCursorPos(&mut point);
    TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON,
        point.x,
        point.y,
        0,
        hwnd,
        std::ptr::null_mut(),
    );
    DestroyMenu(menu);
}

unsafe fn quit_app() {
    QUIT_REQUESTED.store(true, Relaxed);
    remove_tray_icon();
    // Ask the app to quit rather than closing the main window: `App::quit`
    // ends the whole application, so *every* window goes. The main window is
    // not the last one — the floating ball is a second top-level window — and
    // GPUI only auto-quits once its last window closes, so the old
    // `WM_CLOSE`-to-main-window route left the app (and the ball) running with
    // no tray icon and no window to get back to it.
    if send_tray_command(TrayCommand::Quit) {
        return;
    }
    // Nothing is listening (already shutting down, or the channel was never
    // wired): close the window the raw way so it still goes away.
    if let Some(&hwnd) = MAIN_HWND.get() {
        PostMessageW(hwnd, WM_CLOSE, 0, 0);
    }
}

unsafe fn call_previous(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize {
    let prev = PREV_WND_PROC.load(Relaxed) as isize;
    if prev == 0 {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    } else {
        CallWindowProcW(prev, hwnd, msg, wparam, lparam)
    }
}

fn write_wide_slot(slot: &mut [u16], text: &str) {
    for (i, ch) in text
        .encode_utf16()
        .chain(std::iter::once(0))
        .take(slot.len())
        .enumerate()
    {
        slot[i] = ch;
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tray menu selection has to actually arrive at the app.
    ///
    /// `async_channel::Sender::send` is a future: dropping it without polling
    /// (which `let _ = tx.send(cmd)` does, and which compiles without a
    /// warning) sends nothing, so 显示/隐藏悬浮窗 did nothing at all.
    ///
    /// 退出 must arrive for a second reason: it is the only code path that can
    /// quit the whole application, and the ball only goes away with it.
    #[test]
    fn tray_commands_reach_the_channel() {
        let (tx, rx) = async_channel::unbounded();
        if TRAY_CMD_TX.set(tx).is_err() {
            // The sender is a process-wide `OnceLock` and another test got
            // there first; there is no second slot to assert against.
            return;
        }

        assert!(send_tray_command(TrayCommand::Floating), "queued");
        assert!(send_tray_command(TrayCommand::Quit), "queued");

        assert!(
            matches!(rx.try_recv(), Ok(TrayCommand::Floating)),
            "the command must be enqueued, not dropped"
        );
        assert!(
            matches!(rx.try_recv(), Ok(TrayCommand::Quit)),
            "退出 must reach the app: closing the main window alone leaves the \
             floating ball — and the process — alive"
        );
    }

    /// With no channel at all the command cannot be delivered, and the caller
    /// has to be told so it can close the window the raw way instead.
    #[test]
    fn forwarding_reports_failure_when_there_is_no_sender() {
        assert!(!forward_command(None, TrayCommand::Quit));
    }

    /// A sender whose receiver is gone (the app already shut down) is reported
    /// as a failure too, not swallowed.
    #[test]
    fn forwarding_reports_failure_when_the_receiver_is_gone() {
        let (tx, rx) = async_channel::unbounded();
        drop(rx);
        assert!(!forward_command(Some(&tx), TrayCommand::Quit));
    }
}
