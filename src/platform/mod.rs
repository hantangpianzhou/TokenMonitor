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
/// The floating ball's Win32 surface: the shape it is clipped to, the geometry
/// it docks with, and the card size it publishes to the dock logic.
#[cfg(target_os = "windows")]
pub use windows::{
    client_origin_in_window, client_size_device, client_size_logical, dock_window_x,
    fit_floating_card_window, fitted_origin, frame_log, install_ball_drag, is_window_alive,
    monitor_work_area, move_window, open_window_region, region_box, resize_floating_window_plain,
    set_always_on_top, set_floating_card_frac, set_floating_refresh_hit, set_window_circle_region,
    set_window_region, shift_floating_window, show_window, snap_floating_window_to_edge,
    window_rect, window_scale, DockEdge, DOCK_GAP,
};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;
