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
//!
//! Two calm, looping animations make the ball feel alive (both respect
//! `App::reduce_motion` — they freeze at the mid-breath frame when the user
//! asks the OS to reduce motion):
//!   * **breathing scale** — the sphere gently swells and settles (≈0.96×–1.0×)
//!     on a ~2.4 s sine cycle, driven by `with_animation` + `pulsating_between`.
//!   * **halo pulse** — the accent glow behind the ball breathes in opacity in
//!     the same phase, so the light and the ball move together.

use std::time::Duration;

use gpui::{
    div, px, Animation, AnimationExt, Context, Hsla, InteractiveElement, IntoElement, ParentElement,
    Render, StatefulInteractiveElement, Styled, Window, WindowControlArea, pulsating_between,
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
/// Breathing amplitude: the sphere scales between `(1 - BREATH_AMP)`× and `1.0`×.
const BREATH_AMP: f32 = 0.04;
/// Halo opacity at the trough / peak of its breathing pulse.
const GLOW_MIN: f32 = 0.08;
const GLOW_MAX: f32 = 0.20;
/// One full breathing cycle (swell → settle). Shared by the ball and the halo
/// via `repeat_synced` so they stay perfectly in phase.
const BREATH_PERIOD: Duration = Duration::from_millis(2400);

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
    /// Breathing is applied on top of this at paint time, so the window region
    /// is sized with extra headroom to keep the swelling ball from clipping.
    fn drawn_diameter(&self) -> f32 {
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

        // Region (incl. halo) diameter: usage-scaled + hover-popped, with
        // headroom for the breathing scale so the ball never hard-clips at the
        // OS edge. Both the ball and the halo are centered in this region.
        let drawn = self.drawn_diameter();
        let region = drawn * (1.0 + BREATH_AMP + 0.03);
        // Resting (un-breathing) sphere diameter — the animator scales around it.
        let sphere_d = drawn * SPHERE_FRACTION;
        // Halo sits just inside the region so its faint rim masks the hard clip edge.
        let glow_d = region * 0.98;

        // Clip the whole window to the drawn circle: kills the square frame /
        // shadow and lets desktop clicks pass through outside the ball.
        set_window_circle_region(hwnd, region, WINDOW_SIZE);

        let tokens = format_int_grouped(self.total_tokens);
        let cost = format_cost_usd(self.cost_micros);

        // One shared, phase-locked breathing cycle for the ball and the halo.
        let breath = Animation::new(BREATH_PERIOD)
            .repeat_synced()
            .with_easing(pulsating_between(0.0, 1.0));

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
            // Soft accent halo behind the ball — the "floating" feel. It breathes
            // in opacity, in phase with the ball, so the light and the sphere
            // pulse together.
            .child(
                div()
                    .id("glow")
                    .absolute()
                    .top(px((WINDOW_SIZE - glow_d) / 2.0))
                    .left(px((WINDOW_SIZE - glow_d) / 2.0))
                    .w(px(glow_d))
                    .h(px(glow_d))
                    .rounded_full()
                    .bg(accent)
                    .with_animation("glow-breath", breath.clone(), |el, delta| {
                        el.opacity(GLOW_MIN + (GLOW_MAX - GLOW_MIN) * delta)
                    }),
            )
            // The sphere: theme-colored, gradient-shaded, draggable, and it
            // gently breathes in size. Each frame we rebuild it at the current
            // breathing diameter and re-center it, so the number + cost stay
            // composited on top.
            .child(
                div()
                    .id("ball")
                    .rounded_full()
                    .bg(gpui::linear_gradient(
                        135.0,
                        gpui::linear_color_stop(lighter, 0.0),
                        gpui::linear_color_stop(darker, 1.0),
                    ))
                    .with_animation("ball-breath", breath, move |el, delta| {
                        let s = 1.0 - BREATH_AMP + BREATH_AMP * delta;
                        let sd = sphere_d * s;
                        let off = px((WINDOW_SIZE - sd) / 2.0);
                        let fs = px((sd * 0.16).clamp(11.0, 30.0));
                        let cost_fs = px((sd * 0.16 * 0.5).clamp(9.0, 16.0));
                        el.absolute()
                            .top(off)
                            .left(off)
                            .w(px(sd))
                            .h(px(sd))
                            // Bottom-right inner shadow for depth.
                            .child(
                                div()
                                    .absolute()
                                    .bottom(px(sd * 0.05))
                                    .right(px(sd * 0.10))
                                    .w(px(sd * 0.6))
                                    .h(px(sd * 0.42))
                                    .rounded_full()
                                    .bg(BLACK.opacity(0.20)),
                            )
                            // Top-left gloss highlight.
                            .child(
                                div()
                                    .absolute()
                                    .top(px(sd * 0.14))
                                    .left(px(sd * 0.16))
                                    .w(px(sd * 0.34))
                                    .h(px(sd * 0.34))
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
                                            .child(tokens.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_color(WHITE.opacity(0.82))
                                            .text_size(cost_fs)
                                            .child(cost.clone()),
                                    ),
                            )
                    }),
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
