use gpui::Action;

/// Quit the application.
#[derive(Action, Clone, PartialEq, Eq)]
#[action(namespace = tokenmonitor, no_json)]
pub struct Quit;

/// Toggle the always-on-top floating usage ball.
#[derive(Action, Clone, PartialEq, Eq)]
#[action(namespace = tokenmonitor, no_json)]
pub struct ToggleFloatingWindow;
