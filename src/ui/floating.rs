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
//! theme. A linear gradient + a gradient sheen + a small specular dot give it
//! a rounded, spherical read. A soft, layered accent **halo** behind the ball
//! sells the "floating" glow — concentric translucent circles fade outward to
//! fake a radial glow (GPUI this revision has no `box_shadow` / image blur and
//! only two-stop gradients, so we stack rings instead). No animation.
//!
//! # Every layer is its own circle — nothing sits *inside* the ball
//!
//! The previous version nested the shading as children of the ball and relied
//! on `overflow_hidden()` + `rounded_full()` to keep them inside the circle.
//! **That does not work**: GPUI's `overflow_hidden` clips to the element's
//! *rectangle*, not to its rounded corners, so a child positioned near a corner
//! stays fully visible and pokes out of the round silhouette — the ball read as
//! a teardrop instead of a circle.
//!
//! So the ball is now a **stack of concentric circles that are all exactly the
//! same size and position**: base gradient, sheen, specular dot. They are
//! siblings of the halo rings at root level, each centered on the same rect.
//! Because they share one bounding circle they cannot alter the silhouette, and
//! no `overflow_hidden` is needed at all. The one element that is *smaller* than
//! the ball (the specular dot) is kept inside the inscribed circle by
//! [`SPECULAR`], and a unit test pins that invariant.
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
//!   the real window. Two properties matter:
//!   - a clip that merely *equals* the ball would shave its anti-aliased rim;
//!   - a region *larger* than the window makes `SetWindowRgn` degenerate into
//!     "no clip at all", and the square frame shows through.
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
/// Clear gap (px) the clip keeps around every *drawn* edge: ≥ this much of the
/// region is guaranteed empty beyond the glow. `SetWindowRgn` cuts with a hard,
/// non-anti-aliased edge, so if the region boundary landed on a drawn edge that
/// edge would look jagged instead of round.
const CLIP_PAD: f32 = 14.0;
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
const HALO_LAYERS: [(f32, f32); 4] = [(1.20, 0.05), (1.14, 0.08), (1.08, 0.12), (1.03, 0.18)];

/// Top sheen: a white wash anchored at the top edge fading to nothing, drawn as
/// a circle *the same diameter as the ball* so it cannot change the silhouette.
/// A gradient (not a hard-edged highlight disc) is what keeps the sphere read:
/// an offset opaque circle looks like a second, smaller circle glued on.
const SHEEN_ALPHA: f32 = 0.20;
/// Specular dot as `(center_x, center_y, radius)`, all fractions of the ball
/// diameter (0..1). Small and near-opaque: at this size it reads as a glint
/// rather than a disc. Kept strictly inside the ball's inscribed circle by
/// [`dot_is_inside_disc`], which [`tests`] verify.
const SPECULAR: (f32, f32, f32) = (0.36, 0.30, 0.075);
/// Specular opacity.
const SPECULAR_ALPHA: f32 = 0.55;

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

/// Diameter (px) of the outermost glow ring for a ball of `sphere_d` px.
fn glow_outer_diameter(sphere_d: f32) -> f32 {
    sphere_d * HALO_LAYERS[0].0
}

/// Clip-circle diameter (px) for a ball of `sphere_d` px inside a window whose
/// smaller side is `window` px.
///
/// Derived *from* the ball so it can never cut it: it covers the outermost glow
/// ring and keeps [`CLIP_PAD`] of genuinely empty space around *every* drawn
/// edge, so the region's hard (non-anti-aliased) cut always lands on fully
/// transparent pixels and can never jag the ball or the glow. It is then
/// clamped inside the window (a region larger than the window makes
/// `SetWindowRgn` degenerate into "no clip", which shows the square frame),
/// with the ball itself as the final floor — a ball that is visible-but-tight
/// beats a clip that severs it.
fn clip_diameter_for(sphere_d: f32, window: f32) -> f32 {
    let want = glow_outer_diameter(sphere_d).max(sphere_d) + 2.0 * CLIP_PAD;
    want.min(window - CLIP_MARGIN).max(sphere_d)
}

/// Cap (px) on the halo-ring diameters, for a ball of `sphere_d` px under a
/// clip of `clip_d` px.
///
/// Rings are shrunk to leave [`CLIP_PAD`] between the outermost ring and the
/// region boundary, so the region's hard edge never slices a visible ring into
/// a jagged arc. Never shrunk below the ball itself (a ring inside the ball
/// would be pointless, and if the window is pathologically small a ring flush
/// with the ball is still invisible under it).
fn ring_diameter_cap(sphere_d: f32, clip_d: f32) -> f32 {
    (clip_d - 2.0 * CLIP_PAD).max(sphere_d)
}

/// Whether a dot of radius `r` centred at `(cx, cy)` (all fractions of the ball
/// diameter) fits inside the ball's inscribed circle. Guards the specular dot:
/// it is the only drawn element smaller than the ball, so it is the only one
/// that could be placed outside the silhouette by mistake.
fn dot_is_inside_disc(cx: f32, cy: f32, r: f32) -> bool {
    let dx = cx - 0.5;
    let dy = cy - 0.5;
    (dx * dx + dy * dy).sqrt() + r <= 0.5
}

/// Specular dot as `(left, top, diameter)` in px for a ball of `sphere_d` px.
///
/// The dot is the **only** layer that is not a copy of the ball's own circle, so
/// it is the only one that could break the silhouette. If [`SPECULAR`] were ever
/// edited to a fraction that pokes past the rim, the centre is pulled back along
/// its own offset vector until the dot fits inside the inscribed circle — so a
/// bad constant degrades into a dot in a different spot, never into a teardrop
/// ball. [`tests::specular_geometry_never_escapes_the_ball`] pins this.
fn specular_geometry(sphere_d: f32) -> (f32, f32, f32) {
    let (cx, cy, r) = SPECULAR;
    let (mut dx, mut dy) = (cx - 0.5, cy - 0.5);
    if !dot_is_inside_disc(cx, cy, r) {
        let dist = (dx * dx + dy * dy).sqrt();
        let limit = (0.5 - r).max(0.0);
        if dist > f32::EPSILON {
            let k = limit / dist;
            dx *= k;
            dy *= k;
        }
    }
    let d = sphere_d * r * 2.0;
    (
        sphere_d * (0.5 + dx) - d / 2.0,
        sphere_d * (0.5 + dy) - d / 2.0,
        d,
    )
}

/// The `(left, top, diameter)` box shared by the ball and every full-size
/// shading layer.
///
/// All of them must use *this* box: identical geometry is what makes their
/// union a single circle instead of a composite blob. Centred per axis, so a
/// viewport that is not exactly square still keeps every layer concentric.
fn ball_box(win_w: f32, win_h: f32, sphere_d: f32) -> (f32, f32, f32) {
    ((win_w - sphere_d) / 2.0, (win_h - sphere_d) / 2.0, sphere_d)
}

/// Seed the window's circular clip from Win32, before the first `render` runs.
///
/// `render` re-applies the region every frame that actually changes, but the
/// very first frame can see an uninitialised client rect (and the region would
/// then be skipped), leaving the popup square until the next `notify`. Seeding
/// it here makes the window round from the first paint. The seeded circle is
/// deliberately generous ([`SEED_CLIP_FRACTION`]) so it can never clip the ball
/// on the frame before the real geometry is known.
pub fn seed_window_region(hwnd: isize) {
    set_window_circle_region(hwnd, WINDOW_SIZE * SEED_CLIP_FRACTION, WINDOW_SIZE);
}

pub struct FloatingView {
    hwnd: isize,
    total_tokens: u64,
    cost_micros: u64,
    hovered: bool,
    /// Last `(clip_diameter, window_size)` handed to `SetWindowRgn`. Re-applying
    /// an identical region forces the OS to recompute and repaint the whole
    /// window for nothing — and a needless re-cut of the rim can flicker — so
    /// the region is only pushed when the geometry actually moved.
    last_region: (f32, f32),
}

impl FloatingView {
    pub fn new(window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self {
            hwnd: hwnd_of(window),
            total_tokens: 0,
            cost_micros: 0,
            hovered: false,
            last_region: (0.0, 0.0),
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

/// Whether a region of `next` differs from `prev` enough to be worth re-cutting.
fn region_moved(prev: (f32, f32), next: (f32, f32)) -> bool {
    (prev.0 - next.0).abs() > 0.5 || (prev.1 - next.1).abs() > 0.5
}

impl Render for FloatingView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let hwnd = self.hwnd;
        let me = cx.entity();
        let accent = cx.theme().primary;

        // Shading colors derived from the theme accent.
        let lighter = Hsla {
            l: (accent.l + 0.18).min(1.0),
            ..accent
        };
        let darker = Hsla {
            l: (accent.l - 0.26).max(0.0),
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
        let ring_cap = ring_diameter_cap(sphere_d, clip_d);

        // Centre on each axis independently, so a viewport that is not exactly
        // square still keeps every layer concentric.
        let (ball_x, ball_y, _) = ball_box(win_w, win_h, sphere_d);
        let ball_x = px(ball_x);
        let ball_y = px(ball_y);

        // Clip the whole window to a circle concentric with the ball: kills the
        // square frame / shadow and lets desktop clicks pass through outside the
        // ball. `clip_d` encloses every drawn element with `CLIP_PAD` of empty
        // space to spare (so its hard, aliased edge never lands on a drawn
        // edge) and never exceeds the window (so `SetWindowRgn` cannot
        // degenerate). The reference passed alongside it is the measured window
        // size, so the region comes out as exactly this circle in the window's
        // own pixels.
        let region = (clip_d, win);
        if region_moved(self.last_region, region) {
            set_window_circle_region(hwnd, clip_d, win);
            self.last_region = region;
        }

        let tokens = format_int_grouped(self.total_tokens);
        let cost = format_cost_usd(self.cost_micros);
        // Keep the longest realistic count ("12,977,833" = 10 glyphs) inside the
        // ball rather than letting it run to the rim.
        let fs = px((sphere_d * 0.145).clamp(12.0, 30.0));
        let cost_fs = px((sphere_d * 0.082).clamp(9.0, 18.0));

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

        // --- 1. Soft, layered accent halo (static glow), painted first so the
        // ball sits on top. Every ring is larger than the ball and capped to
        // `ring_cap`, so a ring never covers the ball's rim and never reaches
        // the region's hard edge.
        for (i, &(scale, op)) in HALO_LAYERS.iter().enumerate() {
            let d = (sphere_d * scale).min(ring_cap);
            root = root.child(
                div()
                    .id(("glow", i))
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

        // --- 2. The sphere: base shading + sheen + specular. All three are
        // circles occupying the *same* box, so their union is exactly one
        // circle and the silhouette can never become a teardrop. No
        // `overflow_hidden` anywhere: it clips rectangles, not rounded corners,
        // so it would not have contained anything anyway.
        let ball_box = || div().absolute().top(ball_y).left(ball_x);
        root = root
            .child(
                ball_box()
                    .w(px(sphere_d))
                    .h(px(sphere_d))
                    .rounded_full()
                    .bg(gpui::linear_gradient(
                        155.0,
                        gpui::linear_color_stop(lighter, 0.0),
                        gpui::linear_color_stop(darker, 1.0),
                    )),
            )
            .child(
                ball_box()
                    .w(px(sphere_d))
                    .h(px(sphere_d))
                    .rounded_full()
                    .bg(gpui::linear_gradient(
                        180.0,
                        gpui::linear_color_stop(WHITE.opacity(SHEEN_ALPHA), 0.0),
                        gpui::linear_color_stop(WHITE.opacity(0.0), 1.0),
                    )),
            )
            .child({
                let (dot_x, dot_y, dot_d) = specular_geometry(sphere_d);
                div()
                    .absolute()
                    .top(px(dot_y))
                    .left(px(dot_x))
                    .w(px(dot_d))
                    .h(px(dot_d))
                    .rounded_full()
                    .bg(WHITE.opacity(SPECULAR_ALPHA))
            });

        // --- 3. Overlaid number + cost, centred in the same box as the ball.
        // A flex row would fight the absolute layers, so this is one absolutely
        // positioned column sized to the ball.
        root.child(
            div()
                .absolute()
                .top(ball_y)
                .left(ball_x)
                .w(px(sphere_d))
                .h(px(sphere_d))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
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

    /// **The regression that made the ball a teardrop.** The specular dot is
    /// the only drawn element that does not share the ball's bounding box, so it
    /// is the only one that can be placed outside the silhouette by a careless
    /// fraction. Pin it to the inscribed circle.
    #[test]
    fn specular_dot_stays_inside_the_ball_disc() {
        let (cx, cy, r) = SPECULAR;
        assert!(
            dot_is_inside_disc(cx, cy, r),
            "specular ({cx}, {cy}, r={r}) escapes the ball's inscribed circle"
        );
        // ...and the guard actually rejects a protruding dot (the shape of the
        // old inner-shadow bug: offset 0.26, radius 0.30 -> 0.56 > 0.5).
        assert!(!dot_is_inside_disc(0.60, 0.74, 0.30));
    }

    /// The specular helper must never place the dot outside the ball, for any
    /// ball size — and the *shipped* constants must already be inside, so the
    /// correction path (which would move the dot) never triggers in practice.
    #[test]
    fn specular_geometry_never_escapes_the_ball() {
        let (cx, cy, r) = SPECULAR;
        assert!(
            dot_is_inside_disc(cx, cy, r),
            "shipped SPECULAR ({cx}, {cy}, r={r}) already needs correcting"
        );
        for sphere in [
            MIN_SPHERE,
            150.0,
            200.0,
            MAX_SPHERE,
            MAX_SPHERE * HOVER_SCALE,
        ] {
            let (left, top, d) = specular_geometry(sphere);
            let r_px = d / 2.0;
            let cx_px = left + r_px;
            let cy_px = top + r_px;
            let ball_c = sphere / 2.0;
            let dist = ((cx_px - ball_c).powi(2) + (cy_px - ball_c).powi(2)).sqrt();
            assert!(
                dist + r_px <= ball_c + 0.01,
                "specular escapes: ball {sphere}, dot r {r_px} at dist {dist} > {}",
                ball_c
            );
            assert!(left >= 0.0 && top >= 0.0, "dot must stay in the ball's box");
            assert!(left + d <= sphere && top + d <= sphere);
        }
    }

    /// Every shading layer must be exactly the ball's size and position — that
    /// is what makes the union a circle rather than a composite blob. The ball
    /// box is the single source of that geometry, so this pins the two
    /// properties the silhouette depends on: the box fits the window, and a
    /// non-square viewport keeps it inside on both axes.
    #[test]
    fn shading_layers_share_the_ball_box() {
        for win_h in [150.0f32, 240.0, 288.0, 300.0, WINDOW_SIZE, 480.0] {
            // Deliberately non-square: the box must be centred per axis.
            for win_w in [win_h, win_h * 1.25] {
                let win = win_w.min(win_h);
                for tokens in [0u64, 1_000_000, 200_000_000] {
                    let sphere = sphere_diameter_for(tokens, false).min(win * MAX_BALL_FRACTION);
                    let (left, top, d) = ball_box(win_w, win_h, sphere);
                    assert_eq!(d, sphere, "the shared box must be the ball's size");
                    assert!(left >= 0.0 && top >= 0.0);
                    assert!(left + d <= win_w && top + d <= win_h);
                    // Concentric on both axes, not just the smaller one.
                    assert!((left - (win_w - sphere) / 2.0).abs() < 0.01);
                    assert!((top - (win_h - sphere) / 2.0).abs() < 0.01);
                }
            }
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
                        clip >= sphere,
                        "clip {clip} must never be smaller than ball {sphere} (win={win})"
                    );
                    assert!(
                        clip <= win,
                        "clip {clip} must never exceed window {win} (win={win})"
                    );
                    if sphere + 2.0 * CLIP_PAD <= win - CLIP_MARGIN {
                        assert!(
                            clip >= sphere + 2.0 * CLIP_PAD,
                            "clip {clip} must keep {CLIP_PAD}px clear of ball {sphere} \
                             (win={win}, tokens={tokens}, hovered={hovered})"
                        );
                    }
                    assert!(
                        sphere <= MAX_SPHERE * HOVER_SCALE,
                        "ball {sphere} exceeds the maximum (tokens={tokens})"
                    );
                }
            }
        }
    }

    /// **The other half of "not round":** `SetWindowRgn` cuts with a hard,
    /// non-anti-aliased edge. Wherever that edge lands must be fully
    /// transparent, so the clip must always keep a real gap beyond the
    /// outermost thing we draw — the glow. A region that merely touched the
    /// glow would slice it into a jagged arc, which reads as a polygon, not a
    /// circle.
    #[test]
    fn clip_keeps_clear_space_beyond_the_glow() {
        for win in [150.0f32, 240.0, 300.0, WINDOW_SIZE, 480.0] {
            for tokens in [0u64, 1_000_000, 200_000_000, u64::MAX / 2] {
                for hovered in [false, true] {
                    let sphere = sphere_diameter_for(tokens, hovered).min(win * MAX_BALL_FRACTION);
                    let clip = clip_diameter_for(sphere, win);
                    let ring = (glow_outer_diameter(sphere)).min(ring_diameter_cap(sphere, clip));
                    assert!(
                        clip - ring >= 2.0 * CLIP_PAD - 0.01 || ring <= sphere + 0.01,
                        "clip {clip} leaves only {}px beyond the glow {ring} \
                         (win={win}, tokens={tokens}, hovered={hovered})",
                        clip - ring
                    );
                }
            }
        }
    }

    /// Halo rings only ever add *around* the ball, and never reach the region
    /// boundary.
    #[test]
    fn halo_rings_sit_between_the_ball_and_the_clip() {
        for win in [150.0f32, 240.0, 300.0, WINDOW_SIZE, 480.0] {
            let sphere = sphere_diameter_for(200_000_000, false).min(win * MAX_BALL_FRACTION);
            let clip = clip_diameter_for(sphere, win);
            let cap = ring_diameter_cap(sphere, clip);
            for &(scale, op) in HALO_LAYERS.iter() {
                assert!(scale > 1.0, "ring {scale}x must be larger than the ball");
                assert!(op > 0.0 && op < 1.0);
                let d = (sphere * scale).min(cap);
                assert!(
                    d >= sphere,
                    "ring {d} must not shrink inside the ball {sphere}"
                );
                assert!(d <= clip, "ring {d} must stay inside the clip {clip}");
            }
            // The cap itself leaves pad room unless the window is too small.
            assert!(cap <= clip);
        }
    }

    /// The ball plus its glow must fit inside the nominal window at every usage
    /// — otherwise the clip would have to shrink to the ball and the glow would
    /// be sliced.
    #[test]
    fn ball_plus_glow_fits_the_window() {
        let sphere = sphere_diameter_for(u64::MAX / 2, true).min(WINDOW_SIZE * MAX_BALL_FRACTION);
        let glow = glow_outer_diameter(sphere);
        assert!(
            glow + 2.0 * CLIP_PAD <= WINDOW_SIZE - CLIP_MARGIN,
            "glow {glow} + pad must fit the {WINDOW_SIZE} px window"
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

    /// Re-cutting an identical region forces a full window repaint for nothing,
    /// and a needless re-cut can flicker the rim. Only real movement counts.
    #[test]
    fn identical_regions_are_not_reapplied() {
        let a = (300.0, 360.0);
        assert!(!region_moved(a, (300.2, 359.8)));
        assert!(region_moved(a, (320.0, 360.0)));
        assert!(region_moved(a, (300.0, 380.0)));
    }
}
