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
//! it a rounded, spherical read. A soft, layered accent **halo** behind the
//! ball sells the "floating" glow — concentric translucent circles fade
//! outward to fake a radial glow (GPUI this revision has no `box_shadow` /
//! image blur, so we stack rings instead). No animation: the glow is static.
//!
//! # Size model
//!
//! Three different diameters used to be conflated into one "drawn diameter",
//! which made the zero-usage ball only `SPHERE_FRACTION` (65%) of a 64 px
//! constant — a ~42 px ball wrapped in a 1.5× glow and carrying two
//! fixed-minimum text lines. That no longer reads as a circle. The model is
//! now explicit:
//!
//! * [`sphere_diameter_for`] — the **solid ball**, `MIN_SPHERE` px at zero
//!   usage growing to `MAX_SPHERE` px. This is the circle the user sees.
//! * [`clip_diameter_for`] — the **window region**, a circle concentric with
//!   the ball that encloses the glow and the hover pop but never exceeds the
//!   window.
//!
//! Keeping the clip strictly inside the window matters: `SetWindowRgn` with a
//! region larger than the window degenerates into "no clip at all", and the
//! square frame shows through — the other way this ball stops looking round.

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
/// **Solid-ball** diameter at zero usage (px). Deliberately an explicit
/// minimum: the ball stays a legible circle at zero usage instead of shrinking
/// until the glow and the overlay text dominate its silhouette.
const MIN_SPHERE: f32 = 96.0;
/// **Solid-ball** diameter at / above the reference usage (px).
const MAX_SPHERE: f32 = 168.0;
/// Usage (tokens) at which the ball reaches its maximum diameter.
const REF_TOKENS: f64 = 200_000_000.0;
/// Hover pop scale — the ball lifts / grows a touch when the mouse is over it.
const HOVER_SCALE: f32 = 1.06;
/// Slack (px) kept between the clip circle and the window edge, so integer
/// rounding when the region is built in device pixels can never push the
/// region past the window.
const CLIP_MARGIN: f32 = 2.0;

/// Glow rings behind the ball, as `(diameter×sphere, opacity)`. Every ring is
/// strictly larger than the ball (`> 1.0`), so the ball's own edge stays crisp
/// and the glow only ever adds around it. Outer rings are larger and fainter,
/// inner rings tighter and stronger, so the stack reads as a soft radial glow.
/// Static — no breathing / pulsing.
const HALO_LAYERS: [(f32, f32); 4] = [(1.30, 0.05), (1.22, 0.08), (1.13, 0.12), (1.06, 0.18)];

/// Solid-ball diameter (px) for a given usage, hover pop included.
///
/// Zero usage sits at `MIN_SPHERE` (never smaller); usage grows it along a
/// `sqrt` ramp so the ball's *area* is proportional to consumption, saturating
/// at `MAX_SPHERE` at `REF_TOKENS`.
fn sphere_diameter_for(total_tokens: u64, hovered: bool) -> f32 {
    let frac = if total_tokens == 0 {
        0.0
    } else {
        ((total_tokens as f64) / REF_TOKENS).sqrt().clamp(0.0, 1.0) as f32
    };
    let base = MIN_SPHERE + (MAX_SPHERE - MIN_SPHERE) * frac;
    base * if hovered { HOVER_SCALE } else { 1.0 }
}

/// Clip-circle diameter (px) for a ball of `sphere_d` px.
///
/// The clip must (a) be concentric with the ball, (b) enclose the whole glow
/// and the hover pop so nothing gets cut, and (c) stay inside the window —
/// a region larger than the window makes `SetWindowRgn` degenerate into "no
/// clip", which shows the square frame.
fn clip_diameter_for(sphere_d: f32) -> f32 {
    let glow_d = sphere_d * HALO_LAYERS[0].0;
    (glow_d * 1.04).max(sphere_d).min(WINDOW_SIZE - CLIP_MARGIN)
}

/// Clip diameter of the zero-usage (minimum) ball.
fn initial_clip_diameter() -> f32 {
    clip_diameter_for(sphere_diameter_for(0, false))
}

/// Seed the window's circular clip from Win32, before the first `render` runs.
///
/// `render` re-applies the region every frame, but the very first frame can
/// see an uninitialised client rect (in which case the region is skipped) and
/// the popup would stay square until the next `notify`. Seeding it here makes
/// the window round from the first paint.
pub fn seed_window_region(hwnd: isize) {
    set_window_circle_region(hwnd, initial_clip_diameter(), WINDOW_SIZE);
}

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

    /// Current solid-ball diameter (px), usage-scaled and hover-popped.
    fn sphere_diameter(&self) -> f32 {
        sphere_diameter_for(self.total_tokens, self.hovered)
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

        let sphere_d = self.sphere_diameter();
        let clip_d = clip_diameter_for(sphere_d);
        let off = px((WINDOW_SIZE - sphere_d) / 2.0);

        // Clip the whole window to a circle concentric with the ball: kills the
        // square frame / shadow and lets desktop clicks pass through outside
        // the ball. `clip_d` always encloses the ball (so it is never cut) and
        // never exceeds the window (so `SetWindowRgn` cannot degenerate).
        set_window_circle_region(hwnd, clip_d, WINDOW_SIZE);

        let tokens = format_int_grouped(self.total_tokens);
        let cost = format_cost_usd(self.cost_micros);
        let fs = px((sphere_d * 0.16).clamp(11.0, 30.0));
        let cost_fs = px((sphere_d * 0.08).clamp(8.0, 16.0));

        let mut root = div()
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
            });

        // Soft, layered accent halo (static glow) behind the ball. Each ring is
        // a centered translucent circle; painted first so the sphere sits on
        // top. Rings are clamped to `clip_d` so the clip can never slice one
        // into a visible straight edge.
        for (i, &(scale, op)) in HALO_LAYERS.iter().enumerate() {
            let d = (sphere_d * scale).min(clip_d);
            let o = px((WINDOW_SIZE - d) / 2.0);
            root = root.child(
                div()
                    .id(format!("glow-{i}"))
                    .absolute()
                    .top(o)
                    .left(o)
                    .w(px(d))
                    .h(px(d))
                    .rounded_full()
                    .bg(accent)
                    .opacity(op),
            );
        }

        // The sphere: theme-colored, gradient-shaded, draggable. The number +
        // cost are composited on top and re-centered on hover.
        // `overflow_hidden` keeps every child inside the circle's edge, so the
        // overlay can never bulge past the round silhouette.
        root.child(
            div()
                .id("ball")
                .absolute()
                .top(off)
                .left(off)
                .w(px(sphere_d))
                .h(px(sphere_d))
                .rounded_full()
                .overflow_hidden()
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
                                .child(tokens.clone()),
                        )
                        .child(
                            div()
                                .text_color(WHITE.opacity(0.82))
                                .text_size(cost_fs)
                                .child(cost.clone()),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The zero-usage ball must be the full minimum sphere — not a fraction of
    /// a glow-inclusive constant, which is what made it stop looking round.
    #[test]
    fn zero_usage_ball_is_the_minimum_diameter() {
        assert_eq!(sphere_diameter_for(0, false), MIN_SPHERE);
        assert!(MIN_SPHERE > 0.0);
    }

    /// Hover only ever grows the ball, and never past the window.
    #[test]
    fn hover_grows_the_ball() {
        for tokens in [0u64, 1_000_000, 100_000_000, 5_000_000_000] {
            let rest = sphere_diameter_for(tokens, false);
            let popped = sphere_diameter_for(tokens, true);
            assert!(popped > rest, "hover must grow the ball at {tokens} tokens");
        }
    }

    /// The clip must always enclose the ball (so it is never cut) and stay
    /// inside the window (so `SetWindowRgn` cannot degenerate to "no clip",
    /// which would show the square frame).
    #[test]
    fn clip_always_encloses_the_ball_and_fits_the_window() {
        let usages = [
            0u64,
            1,
            10_000,
            1_000_000,
            100_000_000,
            200_000_000,
            u64::MAX / 2,
        ];
        for tokens in usages {
            for hovered in [false, true] {
                let sphere = sphere_diameter_for(tokens, hovered);
                let clip = clip_diameter_for(sphere);
                assert!(
                    clip >= sphere,
                    "clip {clip} must cover ball {sphere} (tokens={tokens}, hovered={hovered})"
                );
                assert!(
                    clip <= WINDOW_SIZE - CLIP_MARGIN,
                    "clip {clip} must stay inside the window (tokens={tokens})"
                );
                assert!(
                    sphere <= MAX_SPHERE * HOVER_SCALE,
                    "ball {sphere} exceeds the maximum (tokens={tokens})"
                );
            }
        }
    }

    /// The ball must be small enough to leave room for its glow inside the
    /// window at every usage — otherwise part of the ball would be clipped.
    #[test]
    fn ball_plus_glow_fits_the_window() {
        let sphere = sphere_diameter_for(u64::MAX / 2, true);
        let glow = sphere * HALO_LAYERS[0].0;
        assert!(
            glow <= WINDOW_SIZE - CLIP_MARGIN,
            "glow {glow} must fit the {WINDOW_SIZE} px window"
        );
    }
}
