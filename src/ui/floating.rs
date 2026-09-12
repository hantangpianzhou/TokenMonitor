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
//! Two diameters drive everything, and they are deliberately explicit:
//!
//! * [`sphere_diameter_for`] — the **solid ball**, `MIN_SPHERE` px at zero
//!   usage growing to `MAX_SPHERE` px. This is the circle the user sees. It is
//!   the *whole* ball, not a fraction of some glow-inclusive constant; earlier
//!   versions conflated the two, which shrank the zero-usage ball to ~42 px and
//!   made it stop reading as a circle.
//! * [`clip_diameter_for`] — the **window region**, a circle concentric with
//!   the ball that encloses the glow and the hover pop, but never exceeds the
//!   window. Keeping the clip strictly inside the window matters: `SetWindowRgn`
//!   with a region *larger* than the window degenerates into "no clip at all",
//!   and the square frame shows through — the other way this ball stops looking
//!   round.
//!
//! The clip is always derived *from* the ball (`clip = max(ball, glow) + small
//! margin`), never the reverse, so the ball can never be sliced. The Win32 side
//! then applies it as a **fraction of the window**, so a client rect that does
//! not match our logical assumption (DPI, rounding, a window that came out
//! larger than requested) can never shrink the circle below the ball.

use gpui::{
    div, px, Context, Hsla, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window, WindowControlArea,
};
use gpui_component::{ActiveTheme, StyledExt};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::format::{format_cost_usd, format_int_grouped};
use crate::platform::set_window_circle_region;

/// Edge length of the square transparent window that hosts the ball (px).
///
/// Must stay in sync with `FLOAT_WIN` in `app::app`, which sizes the window.
const WINDOW_SIZE: f32 = 360.0;
/// **Solid-ball** diameter at zero usage (px). Deliberately an explicit
/// minimum: the ball stays a large, legible circle at zero usage instead of
/// shrinking until the glow and the overlay text dominate its silhouette.
const MIN_SPHERE: f32 = 144.0;
/// **Solid-ball** diameter at / above the reference usage (px).
const MAX_SPHERE: f32 = 232.0;
/// Usage (tokens) at which the ball reaches its maximum diameter.
const REF_TOKENS: f64 = 200_000_000.0;
/// Hover pop scale — the ball lifts / grows a touch when the mouse is over it.
const HOVER_SCALE: f32 = 1.06;
/// Slack (px) kept between the clip circle and the window edge, so integer
/// rounding when the region is built in device pixels can never push the
/// region past the window.
const CLIP_MARGIN: f32 = 4.0;
/// Breathing room (px) the clip keeps around the solid ball itself, so the
/// ball's anti-aliased rim is never touched even if the glow is disabled.
const CLIP_PAD: f32 = 12.0;
/// How much bigger than the outermost glow ring the clip is. Small — the glow
/// should reach close to the edge so the ball+glow read as one glowing object.
const CLIP_GLOW_SLACK: f32 = 1.05;

/// Glow rings behind the ball, as `(diameter×sphere, opacity)`. Every ring is
/// strictly larger than the ball (`> 1.0`), so the ball's own edge stays crisp
/// and the glow only ever adds around it. Outer rings are larger and fainter,
/// inner rings tighter and stronger, so the stack reads as a soft radial glow.
/// Kept tight (≤1.20×) so the solid ball stays the dominant element instead of
/// dissolving into a wide washed-out disc. Static — no breathing / pulsing.
const HALO_LAYERS: [(f32, f32); 4] = [(1.20, 0.06), (1.14, 0.09), (1.08, 0.13), (1.03, 0.20)];

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
/// Derived *from* the ball so it can never cut it: it covers the outermost
/// glow ring with a little slack, keeps at least [`CLIP_PAD`] around the solid
/// ball, and is clamped inside the window (a region larger than the window
/// makes `SetWindowRgn` degenerate into "no clip", which shows the square
/// frame).
fn clip_diameter_for(sphere_d: f32) -> f32 {
    let glow_d = sphere_d * HALO_LAYERS[0].0 * CLIP_GLOW_SLACK;
    glow_d
        .max(sphere_d + CLIP_PAD)
        .min(WINDOW_SIZE - CLIP_MARGIN)
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
        let fs = px((sphere_d * 0.16).clamp(12.0, 34.0));
        let cost_fs = px((sphere_d * 0.085).clamp(9.0, 18.0));

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

    /// The ball has to be big enough to actually read as a circle — the whole
    /// point of the size bump. Guards against a future tweak quietly shrinking
    /// it back to a dot.
    #[test]
    fn ball_is_large_at_every_usage() {
        assert!(
            MIN_SPHERE >= 128.0,
            "the resting ball must stay comfortably large (got {MIN_SPHERE})"
        );
        assert!(MAX_SPHERE > MIN_SPHERE);
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

    /// The whole point of the fix: the clip must enclose the ball *with room
    /// to spare* — a clip that merely equals the ball would shave its rim.
    #[test]
    fn clip_always_encloses_the_ball_with_margin() {
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
                    clip >= sphere + CLIP_PAD,
                    "clip {clip} must leave {CLIP_PAD}px around ball {sphere} \
                     (tokens={tokens}, hovered={hovered})"
                );
                assert!(
                    clip >= sphere * HALO_LAYERS[0].0,
                    "clip {clip} must cover the glow of ball {sphere} (tokens={tokens})"
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

    /// The ball plus its glow must fit inside the window at every usage —
    /// otherwise part of it would be clipped away.
    #[test]
    fn ball_plus_glow_fits_the_window() {
        let sphere = sphere_diameter_for(u64::MAX / 2, true);
        let glow = sphere * HALO_LAYERS[0].0;
        assert!(
            glow <= WINDOW_SIZE - CLIP_MARGIN,
            "glow {glow} must fit the {WINDOW_SIZE} px window"
        );
    }

    /// The first frame (zero usage, not hovered) must already be clipped to a
    /// circle that encloses a full ball.
    #[test]
    fn seeded_clip_encloses_the_first_ball() {
        let sphere = sphere_diameter_for(0, false);
        let clip = initial_clip_diameter();
        assert!(clip >= sphere + CLIP_PAD);
        assert!(clip <= WINDOW_SIZE - CLIP_MARGIN);
    }
}
