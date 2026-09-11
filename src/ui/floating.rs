//! Always-on-top floating "usage ball".
//!
//! A single sphere filled with the app's **theme accent** (`cx.theme().primary`),
//! drawn in a transparent, frameless popup window that is clipped to a circle
//! via `SetWindowRgn` — so there is no square OS frame or rectangular shadow,
//! and clicks outside the ball pass through to the desktop (the 360 / Thunder
//! style). Its diameter grows with total token usage (area ∝ usage) and it
//! pops slightly on hover for a floating feel. The full token count and cost
//! are overlaid on the sphere; data is pushed from `TokenMonitorApp`.
//!
//! The ball is drawn in code (not a PNG) so the color always follows the
//! theme. A linear gradient + a gloss highlight + a bottom inner shadow give
//! it a rounded, spherical read; a soft accent halo behind it sells the
//! "floating" look. GPUI this revision has no radial gradient / image tint, so
//! a code-drawn sphere is the only way to stay on-theme.

use gpui::{
    div, px, Context, Hsla, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window, WindowControlArea,
};
use gpui_component::{ActiveTheme, StyledExt};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::format::{format_cost_usd, format_int_grouped};
use crate::platform::set_window_circle_region;

/// Edge length of the square transparent window that hosts the ball (px).
const WINDOW_SIZE: f32 = 240.0;
/// Drawn (region) diameter at zero usage (px) — includes the glow margin.
const MIN_DIAMETER: f32 = 64.0;
/// Drawn (region) diameter at / above the reference usage (px).
const MAX_DIAMETER: f32 = 232.0;
/// Usage (tokens) at which the ball reaches its maximum diameter.
const REF_TOKENS: f64 = 200_000_000.0;
/// Fraction of the drawn region that is the solid sphere (rest is the glow).
const SPHERE_FRACTION: f32 = 0.65;
/// Hover pop scale — the ball lifts / grows a touch when the mouse is over it.
const HOVER_SCALE: f32 = 1.06;

pub struct FloatingView {
    hwnd: isize,
    total_tokens: u64,
    cost_micros: u64,
    hovered: bool,
}

impl FloatingView {
    pub fn new(window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self {
            hwnd: hwnd_of(window),
            total_tokens: 0,
            cost_micros: 0,
            hovered: false,
        }
    }

    /// Drawn diameter (region, glow included), usage-scaled and hover-popped.
    fn diameter(&self) -> f32 {
        let base = if self.total_tokens == 0 {
            MIN_DIAMETER
        } else {
            let frac = ((self.total_tokens as f64) / REF_TOKENS)
                .sqrt()
                .clamp(0.0, 1.0);
            MIN_DIAMETER + (MAX_DIAMETER - MIN_DIAMETER) * frac as f32
        };
        base * if self.hovered { HOVER_SCALE } else { 1.0 }
    }

    /// Push the latest totals (called from `TokenMonitorApp` on every scan).
    pub fn set_totals(&mut self, total_tokens: u64, cost_micros: u64) {
        self.total_tokens = total_tokens;
        self.cost_micros = cost_micros;
    }
}

impl Render for FloatingView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let d = self.diameter();
        let hwnd = self.hwnd;
        let me = cx.entity();
        let accent = cx.theme().primary;

        // Shading colors derived from the theme accent.
        let lighter = Hsla {
            l: (accent.l + 0.16).min(1.0),
            ..accent
        };
        let darker = Hsla {
            l: (accent.l - 0.20).max(0.0),
            ..accent
        };

        let sphere_d = d * SPHERE_FRACTION;
        let ball_off = px((WINDOW_SIZE - sphere_d) / 2.0);
        let glow_d = d - 4.0; // a hair smaller than the region hides the hard clip edge
        let glow_off = px((WINDOW_SIZE - glow_d) / 2.0);

        // Clip the whole window to the drawn circle: kills the square frame /
        // shadow and lets desktop clicks pass through outside the ball.
        set_window_circle_region(hwnd, d, WINDOW_SIZE);

        let tokens = format_int_grouped(self.total_tokens);
        let cost = format_cost_usd(self.cost_micros);
        let fs = px((sphere_d * 0.16).clamp(11.0, 30.0));
        let cost_fs = px((sphere_d * 0.16 * 0.5).clamp(9.0, 16.0));

        div()
            .id("floating-root")
            .size_full()
            .relative()
            // Mark the whole window as a GPUI drag region. GPUI's WndProc
            // answers `WM_NCHITTEST` with `HTCAPTION` for this region and then
            // lets `DefWindowProc` start the OS move loop — this is GPUI's
            // native frameless-drag path (no manual `PostMessageW` needed).
            // Because the window is clipped to a circle via `SetWindowRgn`,
            // only hits inside the ball resolve to `HTCAPTION`; outside the
            // circle the OS returns `HTTRANSPARENT` and clicks pass through to
            // the desktop.
            //
            // IMPORTANT: do NOT attach an `on_mouse_down` handler to this
            // region. GPUI's `handle_nc_mouse_down_msg` dispatches the
            // `WM_NCLBUTTONDOWN` to the element; if a handler consumes it the
            // message is swallowed and `DefWindowProc` never starts the drag.
            .window_control_area(WindowControlArea::Drag)
            .on_hover(move |hovered, _win, app| {
                me.update(app, |this, cx| {
                    this.hovered = *hovered;
                    cx.notify();
                });
            })
            // Soft accent halo behind the ball — the "floating" feel. Drawn
            // just inside the clip so its faint rim masks the hard OS edge.
            .child(
                div()
                    .absolute()
                    .top(glow_off)
                    .left(glow_off)
                    .w(px(glow_d))
                    .h(px(glow_d))
                    .rounded_full()
                    .bg(accent.opacity(0.14)),
            )
            // The sphere: theme-colored, gradient-shaded, draggable.
            .child(
                div()
                    .id("ball")
                    .absolute()
                    .top(ball_off)
                    .left(ball_off)
                    .w(px(sphere_d))
                    .h(px(sphere_d))
                    .rounded_full()
                    .bg(gpui::linear_gradient(
                        135.0,
                        gpui::linear_color_stop(lighter, 0.0),
                        gpui::linear_color_stop(darker, 1.0),
                    ))
                    // Bottom-right inner shadow for depth.
                    .child(
                        div()
                            .absolute()
                            .bottom(px(sphere_d * 0.05))
                            .right(px(sphere_d * 0.10))
                            .w(px(sphere_d * 0.6))
                            .h(px(sphere_d * 0.42))
                            .rounded_full()
                            .bg(BLACK.opacity(0.20)),
                    )
                    // Top-left gloss highlight.
                    .child(
                        div()
                            .absolute()
                            .top(px(sphere_d * 0.14))
                            .left(px(sphere_d * 0.16))
                            .w(px(sphere_d * 0.34))
                            .h(px(sphere_d * 0.34))
                            .rounded_full()
                            .bg(WHITE.opacity(0.28)),
                    )
                    // Overlaid number + cost.
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .w_full()
                            .h_full()
                            .gap_1()
                            .child(
                                div()
                                    .text_color(WHITE)
                                    .text_size(fs)
                                    .font_semibold()
                                    .child(tokens),
                            )
                            .child(
                                div()
                                    .text_color(WHITE.opacity(0.82))
                                    .text_size(cost_fs)
                                    .child(cost),
                            ),
                    ),
            )
    }
}

/// Extract the native `HWND` from a GPUI window (0 if unavailable).
fn hwnd_of(window: &Window) -> isize {
    if let Ok(h) = HasWindowHandle::window_handle(&*window) {
        if let RawWindowHandle::Win32(win) = h.as_raw() {
            return win.hwnd.get();
        }
    }
    0
}

const WHITE: Hsla = Hsla {
    h: 0.0,
    s: 0.0,
    l: 1.0,
    a: 1.0,
};
const BLACK: Hsla = Hsla {
    h: 0.0,
    s: 0.0,
    l: 0.0,
    a: 1.0,
};
