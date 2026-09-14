//! Platform-specific helpers. Windows is the primary target; Linux supports
//! the TUI frontend; macOS is a placeholder to be completed later.

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::tray::close_window;
/// Install the Windows system-tray icon for the main window (no-op on other
/// platforms, where the symbol does not exist).
#[cfg(target_os = "windows")]
pub use windows::tray::{
    get_floating_hwnd, init_tray_commands, is_floating_visible, register_floating_hwnd,
    send_tray_command, set_floating_visible, start_tray, TrayCommand,
};
#[cfg(target_os = "windows")]
pub use windows::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;
