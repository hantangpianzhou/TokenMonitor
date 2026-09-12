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
//! # Geometry: everything is measured against the *real* window
//!
//! [`WINDOW_SIZE`] is only the **nominal design size** passed to
//! `open_window`. The popup's real logical size is `physical / scale_factor`,
//! so on a scaled display (or after the OS rounds the requested size) it can
//! differ from the nominal constant. Positioning the ball with the constant
//! while clipping to a circle derived from the real window put the ball
//! *off-centre* relative to the concentric clip — and the clip then shaved the
//! ball's right/bottom rim, which is exactly the "I can only see part of the
//! circle" symptom.
//!
//! So every dimension is derived from `Window::viewport_size()` at render time:
//!
//! * [`sphere_diameter_for`] — the **solid ball**, `MIN_SPHERE` px at zero
//!   usage growing to `MAX_SPHERE` px, hard-capped to
//!   [`MAX_BALL_FRACTION`] of the real window.
//! * [`clip_diameter_for`] — the **window region**, derived *from* the ball so
//!   it always encloses it (glow + [`CLIP_PAD`] of slack), then clamped inside
//!   the real window. Keeping the clip strictly inside the window matters:
//!   `SetWindowRgn` with a region *larger* than the window degenerates into
//!   "no clip at all", and the square frame shows through.
//!
//! Ball and clip are then centred on the same rect — per axis, so a non-square
//! viewport cannot shift them apart — which makes it geometrically impossible
//! for the clip to cut the ball, at any DPI and any window size.

use gpui::{
    div, px, Context, Hsla, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window, WindowControlArea,
};
use gpui_component::{ActiveTheme, StyledExt};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::format::{format_cost_usd, format_int_grouped};
use crate::platform::set_window_circle_region;

/// Nominal edge length of the square transparent window that hosts the ball.
/// Must stay in sync with `FLOAT_WIN` in `app::app`. Only used as the *design*
/// reference and to seed the first frame's clip — placement always follows the
/// window's real size.
const WINDOW_SIZE: f32 = 360.0;
/// **Solid-ball** diameter at zero usage (px). Deliberately an explicit
/// minimum: the ball stays a large, legible circle at zero usage instead of
/// shrinking until the glow and the overlay text dominate its silhouette.
const MIN_SPHERE: f32 = 144.0;
/// **Solid-ball** diameter at / above the reference usage (px).
const MAX_SPHERE: f32 = 232.0;
/// Hard ceiling on the ball, as a fraction of the window's smaller side. Leaves
/// room for the glow *and* the clip inside the window even if the window came
/// out smaller than requested.
const MAX_BALL_FRACTION: f32 = 0.70;
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
/// Fraction of the window the *seeded* (first-frame) clip uses. Large on
/// purpose: a first frame that is too generous merely looks like a bigger
/// click-through circle for one frame, while a first frame that is too tight
/// would visibly shave the ball. A pure fraction, so it is correct whatever the
/// window's real size turns out to be.
const SEED_CLIP_FRACTION: f32 = 0.98;
/// Below this, the measured viewport is not believable (it can read as 0 before
/// the first layout) and we fall back to the nominal design size rather than
/// drawing a sub-pixel ball into a degenerate region.
const MIN_SANE_WINDOW: f32 = 32.0;

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

/// Clip-circle diameter (px) for a ball of `sphere_d` px inside a window whose
/// smaller side is `window` px.
///
/// Derived *from* the ball so it can never cut it: it covers the outermost glow
/// ring with a little slack, keeps at least [`CLIP_PAD`] around the solid ball,
/// and is clamped inside the window (a region larger than the window makes
/// `SetWindowRgn` degenerate into "no clip", which shows the square frame). The
/// final `max(sphere_d)` wins over the window clamp, because a ball that is
/// visible-but-tight beats a clip that severs it.
fn clip_diameter_for(sphere_d: f32, window: f32) -> f32 {
    let glow_d = sphere_d * HALO_LAYERS[0].0 * CLIP_GLOW_SLACK;
    glow_d
        .max(sphere_d + CLIP_PAD)
        .min(window - CLIP_MARGIN)
        .max(sphere_d)
}

/// Seed the window's circular clip from Win32, before the first `render` runs.
///
/// `render` re-applies the region every frame, but the very first frame can
/// see an uninitialised client rect (and the region would then be skipped),
/// leaving the popup square until the next `notify`. Seeding it here makes the
/// window round from the first paint. The seeded circle is deliberately
/// generous ([`SEED_CLIP_FRACTION`]) so it can never clip the ball on the frame
/// before the real geometry is known.
pub fn seed_window_region(hwnd: isize) {
    set_window_circle_region(hwnd, WINDOW_SIZE * SEED_CLIP_FRACTION, WINDOW_SIZE);
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

    /// Current solid-ball diameter (px) before the window cap is applied.
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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

        // The *real* logical size of the content area. Never trust the nominal
        // `WINDOW_SIZE`: the popup's logical size is `physical / scale_factor`,
        // so on a scaled display it differs from what we asked for — and a ball
        // placed with the nominal constant ends up off-centre under a clip that
        // *is* centred, which severs its rim.
        let viewport = window.viewport_size();
        let (win_w, win_h, win) = {
            let w = viewport.width.as_f32();
            let h = viewport.height.as_f32();
            if w.min(h) >= MIN_SANE_WINDOW {
                (w, h, w.min(h))
            } else {
                // Not laid out yet — design against the nominal size for this
                // frame; the next render measures the real one.
                (WINDOW_SIZE, WINDOW_SIZE, WINDOW_SIZE)
            }
        };

        let sphere_d = self.sphere_diameter().min(win * MAX_BALL_FRACTION);
        let clip_d = clip_diameter_for(sphere_d, win);

        // Centre on each axis independently, so a viewport that is not exactly
        // square still keeps ball and clip concentric.
        let off_x = px((win_w - sphere_d) / 2.0);
        let off_y = px((win_h - sphere_d) / 2.0);

        // Clip the whole window to a circle concentric with the ball: kills the
        // square frame / shadow and lets desktop clicks pass through outside
        // the ball. `clip_d` always encloses the ball (so it is never cut) and
        // never exceeds the window (so `SetWindowRgn` cannot degenerate). The
        // reference passed alongside it is the measured window size, so the
        // region comes out as exactly this circle in the window's own pixels.
        set_window_circle_region(hwnd, clip_d, win);

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
            root = root.child(
                div()
                    .id(format!("glow-{i}"))
                    .absolute()
                    .top(px((win_h - d) / 2.0))
                    .left(px((win_w - d) / 2.0))
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
                .top(off_y)
                .left(off_x)
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

    /// The whole point of the fix: the clip must enclose the ball *with room to
    /// spare* — a clip that merely equals the ball would shave its rim.
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
        // Windows the popup might actually come out as, including ones much
        // smaller than the nominal design size.
        for win in [150.0f32, 240.0, 300.0, WINDOW_SIZE, 480.0] {
            for tokens in usages {
                for hovered in [false, true] {
                    let sphere = sphere_diameter_for(tokens, hovered).min(win * MAX_BALL_FRACTION);
                    let clip = clip_diameter_for(sphere, win);
                    assert!(
                        clip >= sphere + CLIP_PAD,
                        "clip {clip} must leave {CLIP_PAD}px around ball {sphere} \
                         (win={win}, tokens={tokens}, hovered={hovered})"
                    );
                    assert!(
                        clip >= sphere * HALO_LAYERS[0].0,
                        "clip {clip} must cover the glow of ball {sphere} (win={win})"
                    );
                    assert!(
                        clip >= sphere,
                        "clip {clip} must never be smaller than ball {sphere} (win={win})"
                    );
                    if sphere + CLIP_MARGIN <= win {
                        assert!(
                            clip <= win - CLIP_MARGIN,
                            "clip {clip} must stay inside window {win} \
                             (tokens={tokens}, hovered={hovered})"
                        );
                    }
                    assert!(
                        clip <= win,
                        "clip {clip} must never exceed window {win} (win={win})"
                    );
                    assert!(
                        sphere <= MAX_SPHERE * HOVER_SCALE,
                        "ball {sphere} exceeds the maximum (tokens={tokens})"
                    );
                }
            }
        }
    }

    /// The ball plus its glow must fit inside the nominal window at every usage
    /// — otherwise the clip would have to shrink to the ball and the glow would
    /// be sliced.
    #[test]
    fn ball_plus_glow_fits_the_window() {
        let sphere = sphere_diameter_for(u64::MAX / 2, true);
        let glow = sphere * HALO_LAYERS[0].0;
        assert!(
            glow <= WINDOW_SIZE - CLIP_MARGIN,
            "glow {glow} must fit the {WINDOW_SIZE} px window"
        );
    }

    /// Even on the smallest plausible window the ball stays large, and the cap
    /// only ever shrinks it (never grows it past `MAX_SPHERE`).
    #[test]
    fn ball_is_capped_by_the_real_window() {
        let huge = sphere_diameter_for(u64::MAX / 2, true);
        for win in [150.0f32, 240.0, 300.0, WINDOW_SIZE] {
            let capped = huge.min(win * MAX_BALL_FRACTION);
            assert!(capped <= huge);
            assert!(capped <= win);
        }
        // At the nominal size nothing is capped away.
        assert_eq!(huge.min(WINDOW_SIZE * MAX_BALL_FRACTION), huge);
    }

    /// The first frame must be clipped to a circle that comfortably encloses a
    /// full ball, whatever the window's real size is — the seed is a pure
    /// fraction, so this holds for any window.
    #[test]
    fn seeded_clip_is_generous_enough_for_any_window() {
        assert!(SEED_CLIP_FRACTION > 0.0 && SEED_CLIP_FRACTION <= 1.0);
        for win in [150.0f32, 240.0, 300.0, WINDOW_SIZE, 480.0] {
            let seeded = win * SEED_CLIP_FRACTION;
            let sphere = sphere_diameter_for(0, false).min(win * MAX_BALL_FRACTION);
            assert!(
                seeded > sphere,
                "seeded clip {seeded} must enclose the first-frame ball {sphere} (win={win})"
            );
        }
    }
}
