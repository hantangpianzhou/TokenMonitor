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
//! a rounded, spherical read. A soft accent **halo** behind the ball sells the
//! "floating" glow.
//!
//! # Three rules, and the bugs that produced them
//!
//! ## 1. The ball and the clip are laid out from the *same* rectangle
//!
//! Everything is derived from the window's real **client rect**
//! ([`crate::platform::client_size_logical`]) — the same rectangle
//! `SetWindowRgn` clips in — converted to logical px with the window's own
//! scale factor. Both the ball's offset and the region are then centred on that
//! one rectangle, so they are concentric *by construction*, at any DPI.
//!
//! Using GPUI's `viewport_size()` instead was the bug behind "I can only see
//! part of the circle": the viewport is the size GPUI *believes* the window
//! has, which can disagree with the real client area. A ball centred on the
//! viewport inside a clip centred on the client rect ends up off-centre, and
//! the clip then severs one rim — leaving a hard, aliased, one-sided cut and no
//! glow on that side, which is exactly what a pixel dump of the broken build
//! showed.
//!
//! ## 2. Every layer is its own circle — nothing sits *inside* the ball
//!
//! The ball is a stack of concentric circles sharing one bounding box (base
//! gradient, sheen, specular dot), and so are the halo rings; all are siblings
//! centred on the same rect. Their union is one circle, so the silhouette
//! cannot deform.
//!
//! Nesting shading *inside* the ball and relying on `overflow_hidden()` does
//! not work: GPUI clips it to the element's **rectangle**, not its rounded
//! corners, so a child near a corner stays fully visible and pokes out — the
//! ball read as a teardrop. `overflow_hidden` is not used here at all.
//!
//! ## 3. The clip always keeps real empty space beyond the glow
//!
//! [`max_sphere_for_window`] caps the ball so that
//! `glow + 2 * CLIP_PAD <= win - CLIP_MARGIN` holds for *every* window size,
//! which makes the region's hard, non-anti-aliased edge always land on fully
//! transparent pixels. A region that touched a drawn edge would jag it, and a
//! region *larger* than the window makes `SetWindowRgn` degenerate into "no
//! clip at all".
//!
//! The halo itself is a smooth ladder of many faint rings rather than a few
//! strong ones: each ring adds only [`HALO_RING_ALPHA`] (~1% alpha), so the
//! steps between rings are below the eye's threshold and the glow reads as a
//! continuous radial falloff instead of a set of visible concentric bands.
//!
//! # The overlay text always fits
//!
//! The number grows with usage — from `0` to `999,999,999` and beyond — and a
//! number wider than the ball would break the circular read just as badly as a
//! clipped rim. So the font size is **fitted to the ball**: the string is shaped
//! with the window's own text system ([`fit_font_size`]) and scaled down until
//! it fits the ball's **inscribed square** ([`text_budget_side`]), which is
//! inside the circle by definition. A conservative per-glyph estimate is used
//! whenever shaping is unavailable, so the size is always safe.

use gpui::{
    div, px, Context, Font, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement,
    Render, StatefulInteractiveElement, Styled, TextRun, Window, WindowControlArea,
};
use gpui_component::{ActiveTheme, StyledExt};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::format::{format_cost_usd, format_int_grouped};
use crate::platform::{client_size_logical, set_window_circle_region};

/// Nominal edge length of the square transparent window that hosts the ball.
/// Must stay in sync with `FLOAT_WIN` in `app::app`. Only the *design*
/// reference and the fallback when the client rect is not available yet —
/// placement always follows the measured client rect.
const WINDOW_SIZE: f32 = 360.0;
/// **Solid-ball** diameter at zero usage (px). Deliberately an explicit
/// minimum: the ball stays a large, legible circle at zero usage instead of
/// shrinking until the halo and the overlay text dominate its silhouette.
const MIN_SPHERE: f32 = 144.0;
/// **Solid-ball** diameter at / above the reference usage (px).
const MAX_SPHERE: f32 = 232.0;
/// Ceiling on the ball as a fraction of the window's smaller side.
const MAX_BALL_FRACTION: f32 = 0.70;
/// Usage (tokens) at which the ball reaches its maximum diameter.
const REF_TOKENS: f64 = 200_000_000.0;
/// Hover pop scale — the ball lifts / grows a touch when the mouse is over it.
const HOVER_SCALE: f32 = 1.06;
/// Slack (px) between the clip circle and the window edge, so rounding when the
/// region is built in device pixels can never push it past the window.
const CLIP_MARGIN: f32 = 4.0;
/// Clear gap (px) the clip keeps around the outermost drawn pixel. The region is
/// cut with a hard, non-anti-aliased edge, so this much of it must be empty —
/// otherwise the cut jags whatever it lands on.
const CLIP_PAD: f32 = 12.0;
/// Number of rings stacked to fake a radial glow. Many rings × a tiny per-ring
/// alpha ⇒ the steps between rings are ~1% alpha each, i.e. invisible, so the
/// glow has no visible banding. (This revision of GPUI has no `box_shadow` and
/// no radial gradient, and only two-stop linear gradients.)
const HALO_RINGS: usize = 18;
/// Diameter of the innermost halo ring, as a multiple of the ball.
const HALO_INNER_SCALE: f32 = 1.0;
/// Diameter of the outermost halo ring, as a multiple of the ball.
const HALO_OUTER_SCALE: f32 = 1.22;
/// Alpha of each individual halo ring. The rings composite, so the glow's
/// strength at the rim is `1 - (1 - alpha) ^ (HALO_RINGS - 1)` ≈ 0.17.
const HALO_RING_ALPHA: f32 = 0.011;
/// Fraction of the window the *seeded* (first-frame) clip uses. Large on
/// purpose: a first frame that is too generous merely looks like a bigger
/// click-through circle for one frame, while a first frame that is too tight
/// would visibly shave the ball.
const SEED_CLIP_FRACTION: f32 = 0.98;
/// Below this, a measured size is not believable (the client rect can read 0
/// before the first layout) and we fall back to the nominal design size rather
/// than drawing a sub-pixel ball into a degenerate region.
const MIN_SANE_WINDOW: f32 = 32.0;

/// Top sheen: a white wash anchored at the top edge fading to nothing, drawn as
/// a circle *the same diameter as the ball* so it cannot change the silhouette.
/// A gradient (not a hard-edged highlight disc) is what keeps the sphere read:
/// an offset opaque circle looks like a second, smaller circle glued on.
const SHEEN_ALPHA: f32 = 0.20;
/// Specular dot as `(center_x, center_y, radius)`, all fractions of the ball
/// diameter (0..1). Small and near-opaque: at this size it reads as a glint
/// rather than a disc. Kept strictly inside the ball's inscribed circle by
/// [`dot_is_inside_disc`].
const SPECULAR: (f32, f32, f32) = (0.36, 0.30, 0.075);
/// Specular opacity.
const SPECULAR_ALPHA: f32 = 0.55;

/// Fraction of the ball's inscribed square the overlay text may use. The
/// inscribed square is inside the circle by definition, so anything that fits it
/// cannot break the circular silhouette.
const TEXT_BOX_FRACTION: f32 = 0.92;
/// Ceiling on the number's font size, as a fraction of the ball diameter.
const NUM_CAP_RATIO: f32 = 0.26;
/// Ceiling on the cost line's font size, as a fraction of the ball diameter.
const COST_CAP_RATIO: f32 = 0.13;
/// Font size used to *probe* a string's width before scaling it to fit. Any
/// value works (width is proportional to size); 20px keeps rounding small.
const FIT_PROBE_PX: f32 = 20.0;
/// Conservative advance of one glyph, as a fraction of the font size, used when
/// the text system cannot be asked. Upper bounds for a semibold sans face.
const EM_DIGIT: f32 = 0.64;
const EM_OTHER: f32 = 0.36;

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

/// Diameter (px) of the outermost halo ring for a ball of `sphere_d` px.
fn glow_outer_diameter(sphere_d: f32) -> f32 {
    sphere_d * HALO_OUTER_SCALE
}

/// Diameter (px) of halo ring `i` (`0` = innermost) for a ball of `sphere_d` px.
fn halo_ring_diameter(sphere_d: f32, i: usize) -> f32 {
    let t = if HALO_RINGS <= 1 {
        0.0
    } else {
        i as f32 / (HALO_RINGS - 1) as f32
    };
    sphere_d * (HALO_INNER_SCALE + (HALO_OUTER_SCALE - HALO_INNER_SCALE) * t)
}

/// Largest ball (px) that still leaves the clip room to clear the glow inside a
/// window whose smaller side is `win` px.
///
/// This is what makes "the clip keeps empty space beyond every drawn pixel" hold
/// for *any* window size instead of only the nominal one: capping the ball is
/// the only lever that keeps `glow + 2 * CLIP_PAD <= win - CLIP_MARGIN` true when
/// the window comes out smaller than requested.
fn max_sphere_for_window(win: f32) -> f32 {
    ((win - CLIP_MARGIN - 2.0 * CLIP_PAD) / HALO_OUTER_SCALE).max(0.0)
}

/// Clip-circle diameter (px) for a ball of `sphere_d` px inside a window whose
/// smaller side is `win` px.
///
/// Derived *from* the ball so it can never cut it: it covers the outermost halo
/// ring and keeps [`CLIP_PAD`] of genuinely empty space around *every* drawn
/// edge, so the region's hard (non-anti-aliased) cut lands on fully transparent
/// pixels. It is clamped inside the window because a region larger than the
/// window makes `SetWindowRgn` degenerate into "no clip".
fn clip_diameter_for(sphere_d: f32, win: f32) -> f32 {
    (glow_outer_diameter(sphere_d) + 2.0 * CLIP_PAD).min(win - CLIP_MARGIN)
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
/// ball.
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

/// The `(left, top, diameter)` box shared by the ball, every full-size shading
/// layer and every halo ring.
///
/// All of them must use *this* box: identical geometry is what makes their union
/// a single circle instead of a composite blob. Centred per axis, so a client
/// area that is not exactly square still keeps every layer concentric.
fn ball_box(win_w: f32, win_h: f32, sphere_d: f32) -> (f32, f32, f32) {
    ((win_w - sphere_d) / 2.0, (win_h - sphere_d) / 2.0, sphere_d)
}

/// Width (px) the overlay text may occupy inside a ball of `sphere_d` px: the
/// ball's **inscribed square**, minus [`TEXT_BOX_FRACTION`]'s worth of slack.
///
/// Using the inscribed square (rather than the diameter) is what makes the text
/// safe at *any* string length: a box inscribed in the circle stays inside it,
/// so text that fits the box cannot touch the rim and break the circular
/// silhouette.
fn text_budget_side(sphere_d: f32) -> f32 {
    sphere_d / std::f32::consts::SQRT_2 * TEXT_BOX_FRACTION
}

/// Conservative width estimate for `text` in font-size units (em), used when the
/// text system cannot be asked for a real measurement.
fn estimated_em(text: &str) -> f32 {
    text.chars()
        .map(|c| {
            if c.is_ascii_digit() {
                EM_DIGIT
            } else {
                EM_OTHER
            }
        })
        .sum()
}

/// Width (px) of `text` at `size` px and `weight`, as measured by the window's
/// own text system. Returns 0 if it could not be measured.
fn measure_text(window: &Window, text: &str, size: f32, weight: FontWeight) -> f32 {
    if text.is_empty() || !(size > 0.0) {
        return 0.0;
    }
    let style = window.text_style();
    let run = TextRun {
        len: text.len(),
        font: Font {
            family: style.font_family.clone(),
            weight,
            ..Default::default()
        },
        color: WHITE,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let width = window
        .text_system()
        .shape_line(text.to_string().into(), px(size), &[run], None)
        .width()
        .as_f32();
    if width.is_finite() && width > 0.0 {
        width
    } else {
        0.0
    }
}

/// Width (px) of `text` at `size` px, from the text system when it can be asked
/// and from the conservative [`estimated_em`] otherwise.
fn width_at(window: &Window, text: &str, size: f32, weight: FontWeight) -> f32 {
    let measured = measure_text(window, text, size, weight);
    if measured > 0.0 {
        measured
    } else {
        estimated_em(text) * size
    }
}

/// Largest font size whose width fits, from a single probe measurement.
///
/// `probe_w` is the string's width at `probe` px. Width is proportional to size,
/// so `probe * max_w / probe_w` is exactly the size at which the string is
/// `max_w` wide. Pure arithmetic, so the "never overflows" property can be tested
/// without a window.
fn fit_size_from_probe(probe_w: f32, probe: f32, max_w: f32, cap: f32) -> f32 {
    if !(probe_w > 0.0) || !(max_w > 0.0) || !(cap > 0.0) {
        return 0.0;
    }
    (probe * max_w / probe_w).min(cap)
}

/// Largest font size (px) at which `text` fits `max_w`, never above `cap`.
///
/// Fitting wins over legibility: a size that overflows would let the number
/// touch the rim and break the circular silhouette, so there is deliberately no
/// minimum size — if a window were ever too small for the count to be readable,
/// the count shrinks rather than the circle breaking.
///
/// A second measurement corrects for shaping not being perfectly linear across
/// sizes (hinting, rounding), so the returned size never overflows the budget.
fn fit_font_size(window: &Window, text: &str, max_w: f32, cap: f32, weight: FontWeight) -> f32 {
    if text.is_empty() || !(max_w > 0.0) || !(cap > 0.0) {
        return 0.0;
    }
    let probe = FIT_PROBE_PX.min(cap);
    let probe_w = width_at(window, text, probe, weight);
    let mut size = fit_size_from_probe(probe_w, probe, max_w, cap);
    if !(size > 0.0) {
        return 0.0;
    }
    let actual = width_at(window, text, size, weight);
    if actual > max_w {
        size = (size * max_w / actual).min(size).max(1.0);
    }
    size
}

/// Cached result of [`fit_font_size`] for one string + budget, so the text is
/// shaped once per geometry change rather than on every render.
#[derive(Debug, Clone, PartialEq)]
struct TextFit {
    text: String,
    max_w: i32,
    cap: i32,
    size: f32,
}

impl TextFit {
    fn new(text: &str, max_w: f32, cap: f32, size: f32) -> Self {
        Self {
            text: text.to_string(),
            max_w: quantize(max_w),
            cap: quantize(cap),
            size,
        }
    }

    fn matches(&self, text: &str, max_w: f32, cap: f32) -> bool {
        self.text == text && self.max_w == quantize(max_w) && self.cap == quantize(cap)
    }
}

/// Quantize a px value to 1/100 px so tiny float noise does not invalidate the
/// fit cache.
fn quantize(v: f32) -> i32 {
    (v * 100.0).round() as i32
}

/// Seed the window's circular clip from Win32, before the first `render` runs.
///
/// `render` re-applies the region, but the very first frame can see an
/// uninitialised client rect (and the region would then be skipped), leaving the
/// popup square until the next `notify`. Seeding it here makes the window round
/// from the first paint. The seeded circle is deliberately generous
/// ([`SEED_CLIP_FRACTION`]) so it can never clip the ball on the frame before the
/// real geometry is known.
pub fn seed_window_region(hwnd: isize) {
    set_window_circle_region(hwnd, WINDOW_SIZE * SEED_CLIP_FRACTION, WINDOW_SIZE);
}

/// Whether a region of `next` differs from `prev` enough to be worth re-cutting.
fn region_moved(prev: (f32, f32), next: (f32, f32)) -> bool {
    (prev.0 - next.0).abs() > 0.5 || (prev.1 - next.1).abs() > 0.5
}

pub struct FloatingView {
    hwnd: isize,
    total_tokens: u64,
    cost_micros: u64,
    hovered: bool,
    /// Last `(clip_diameter, window_size)` handed to `SetWindowRgn`. Re-applying
    /// an identical region forces the OS to recompute and repaint the whole
    /// window for nothing — and a needless re-cut of the rim can flicker — so the
    /// region is only pushed when the geometry actually moved.
    last_region: (f32, f32),
    /// Cached fit of the token count / cost strings.
    num_fit: Option<TextFit>,
    cost_fit: Option<TextFit>,
}

impl FloatingView {
    pub fn new(window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self {
            hwnd: hwnd_of(window),
            total_tokens: 0,
            cost_micros: 0,
            hovered: false,
            last_region: (0.0, 0.0),
            num_fit: None,
            cost_fit: None,
        }
    }

    /// Current solid-ball diameter (px) before the window caps are applied.
    fn sphere_diameter(&self) -> f32 {
        sphere_diameter_for(self.total_tokens, self.hovered)
    }

    /// Push the latest totals (called from `TokenMonitorApp` on every scan).
    pub fn set_totals(&mut self, total_tokens: u64, cost_micros: u64) {
        self.total_tokens = total_tokens;
        self.cost_micros = cost_micros;
    }

    /// Font size (px) that keeps `text` inside the ball, cached per string and
    /// budget.
    fn fitted_size(
        cache: &mut Option<TextFit>,
        window: &Window,
        text: &str,
        max_w: f32,
        cap: f32,
        weight: FontWeight,
    ) -> f32 {
        if let Some(hit) = cache.as_ref() {
            if hit.matches(text, max_w, cap) {
                return hit.size;
            }
        }
        let size = fit_font_size(window, text, max_w, cap, weight);
        *cache = Some(TextFit::new(text, max_w, cap, size));
        size
    }
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

        // The **client rect** is the one rectangle both this code and
        // `SetWindowRgn` can agree on, so every dimension below comes from it.
        // `viewport_size()` is only a fallback for the frames before the client
        // rect exists: it is GPUI's belief about the window, which can differ
        // from the real client area — and laying the ball out on one rectangle
        // while clipping it on another is what severs a rim.
        let scale = window.scale_factor();
        let (win_w, win_h) = match client_size_logical(hwnd, scale) {
            Some((w, h)) if w.min(h) >= MIN_SANE_WINDOW => (w, h),
            _ => {
                let v = window.viewport_size();
                let (w, h) = (v.width.as_f32(), v.height.as_f32());
                if w.min(h) >= MIN_SANE_WINDOW {
                    (w, h)
                } else {
                    (WINDOW_SIZE, WINDOW_SIZE)
                }
            }
        };
        let win = win_w.min(win_h);

        // Cap the ball three ways: usage ramp, the window, and — the one that
        // makes the clip's guaranteed padding true at every window size — what
        // still leaves room for the glow plus the pad.
        let sphere_d = self
            .sphere_diameter()
            .min(win * MAX_BALL_FRACTION)
            .min(max_sphere_for_window(win));
        let clip_d = clip_diameter_for(sphere_d, win);

        // Centre on each axis independently, so a client area that is not
        // exactly square still keeps every layer concentric.
        let (ball_x, ball_y, _) = ball_box(win_w, win_h, sphere_d);
        let ball_x = px(ball_x);
        let ball_y = px(ball_y);

        // Clip the window to a circle concentric with the ball: kills the square
        // frame / shadow and lets desktop clicks pass through outside the ball.
        // `clip_d` encloses the glow with `CLIP_PAD` of empty space to spare (so
        // its hard, aliased edge never lands on a drawn pixel) and never exceeds
        // the window (so `SetWindowRgn` cannot degenerate). The reference size is
        // the measured client size, so the region comes out as exactly this
        // circle in the window's own pixels.
        let region = (clip_d, win);
        if region_moved(self.last_region, region) {
            set_window_circle_region(hwnd, clip_d, win);
            self.last_region = region;
        }

        let tokens = format_int_grouped(self.total_tokens);
        let cost = format_cost_usd(self.cost_micros);
        // Fit both strings to the ball's inscribed square, so an ever-growing
        // count (0 → 999,999,999 and past it) can never run into the rim and
        // stop the whole thing reading as a circle.
        let budget = text_budget_side(sphere_d);
        let fs = Self::fitted_size(
            &mut self.num_fit,
            window,
            &tokens,
            budget,
            sphere_d * NUM_CAP_RATIO,
            FontWeight::SEMIBOLD,
        );
        let cost_fs = Self::fitted_size(
            &mut self.cost_fit,
            window,
            &cost,
            budget,
            sphere_d * COST_CAP_RATIO,
            FontWeight::NORMAL,
        );

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

        // --- 1. Halo (static glow), painted first so the ball sits on top.
        // Outermost ring first: each smaller ring composites on top of the
        // larger ones, so alpha accumulates smoothly inward instead of forming
        // discrete bands. Every ring is centred on the same box as the ball.
        for i in (0..HALO_RINGS).rev() {
            let d = halo_ring_diameter(sphere_d, i);
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
                    .opacity(HALO_RING_ALPHA),
            );
        }

        // --- 2. The sphere: base shading + sheen + specular. All three are
        // circles occupying the *same* box, so their union is exactly one circle
        // and the silhouette can never become a teardrop. No `overflow_hidden`
        // anywhere: it clips rectangles, not rounded corners, so it would not
        // have contained anything anyway.
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
                        .text_size(px(fs))
                        .font_semibold()
                        .child(tokens),
                )
                .child(
                    div()
                        .text_color(WHITE.opacity(0.82))
                        .text_size(px(cost_fs))
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

    /// Windows the popup might really come out as — including ones much smaller
    /// than the nominal design size, where the caps have to do the work.
    const WINDOWS: [f32; 7] = [120.0, 150.0, 200.0, 240.0, 300.0, WINDOW_SIZE, 480.0];
    /// Usage levels spanning the whole documented range: nothing, sub-thousand,
    /// 7, 8 and 9 digits, and past the saturation point.
    const USAGES: [u64; 8] = [
        0,
        1,
        999,
        1_000_000,
        16_777_343,
        100_000_000,
        999_999_999,
        u64::MAX / 2,
    ];

    /// The zero-usage ball must be the full minimum sphere — not a fraction of a
    /// glow-inclusive constant, which is what made it stop looking round.
    #[test]
    fn zero_usage_ball_is_the_minimum_diameter() {
        assert_eq!(sphere_diameter_for(0, false), MIN_SPHERE);
    }

    /// The ball has to be big enough to actually read as a circle at any usage
    /// the nominal window can host.
    #[test]
    fn ball_is_large_at_every_usage() {
        assert!(
            MIN_SPHERE >= 128.0,
            "the resting ball must stay comfortably large (got {MIN_SPHERE})"
        );
        assert!(MAX_SPHERE > MIN_SPHERE);
        for tokens in USAGES {
            for hovered in [false, true] {
                let d = sphere_diameter_for(tokens, hovered)
                    .min(WINDOW_SIZE * MAX_BALL_FRACTION)
                    .min(max_sphere_for_window(WINDOW_SIZE));
                assert!(d >= MIN_SPHERE, "ball shrank to {d} at {tokens} tokens");
            }
        }
    }

    /// Hover only ever grows the ball.
    #[test]
    fn hover_grows_the_ball() {
        for tokens in USAGES {
            let rest = sphere_diameter_for(tokens, false);
            let popped = sphere_diameter_for(tokens, true);
            assert!(popped > rest, "hover must grow the ball at {tokens} tokens");
        }
    }

    /// **The regression that made the ball a teardrop.** The specular dot is the
    /// only drawn element that does not share the ball's bounding box, so it is
    /// the only one that can be placed outside the silhouette by a careless
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
    /// ball size.
    #[test]
    fn specular_geometry_never_escapes_the_ball() {
        for sphere in [
            MIN_SPHERE,
            150.0,
            200.0,
            MAX_SPHERE,
            MAX_SPHERE * HOVER_SCALE,
        ] {
            let (left, top, d) = specular_geometry(sphere);
            let r_px = d / 2.0;
            let dist =
                ((left + r_px - sphere / 2.0).powi(2) + (top + r_px - sphere / 2.0).powi(2)).sqrt();
            assert!(
                dist + r_px <= sphere / 2.0 + 0.01,
                "specular escapes: ball {sphere}, dot r {r_px} at dist {dist}"
            );
            assert!(left >= 0.0 && top >= 0.0, "dot must stay in the ball's box");
            assert!(left + d <= sphere && top + d <= sphere);
        }
    }

    /// Every shading layer shares the ball's box, so their union is one circle.
    /// Also pins per-axis centring, so a non-square client area cannot shift a
    /// layer off the ball.
    #[test]
    fn shading_layers_share_the_ball_box() {
        for win_h in WINDOWS {
            for win_w in [win_h, win_h * 1.25] {
                let win = win_w.min(win_h);
                for tokens in USAGES {
                    let sphere = sphere_diameter_for(tokens, false)
                        .min(win * MAX_BALL_FRACTION)
                        .min(max_sphere_for_window(win));
                    let (left, top, d) = ball_box(win_w, win_h, sphere);
                    assert_eq!(d, sphere, "the shared box must be the ball's size");
                    assert!(left >= 0.0 && top >= 0.0);
                    assert!(left + d <= win_w && top + d <= win_h);
                    assert!((left - (win_w - sphere) / 2.0).abs() < 0.01);
                    assert!((top - (win_h - sphere) / 2.0).abs() < 0.01);
                }
            }
        }
    }

    /// **The whole point of the fix.** For every usage × every window, the clip
    /// must enclose the ball *and* the glow with [`CLIP_PAD`] of empty space to
    /// spare, and still fit inside the window. A clip that merely equals the ball
    /// shaves its anti-aliased rim; a clip that lands on the glow slices it into
    /// a jagged arc; a clip larger than the window stops clipping at all.
    #[test]
    fn clip_encloses_ball_and_glow_with_pad_at_every_size() {
        for win in WINDOWS {
            for tokens in USAGES {
                for hovered in [false, true] {
                    let sphere = sphere_diameter_for(tokens, hovered)
                        .min(win * MAX_BALL_FRACTION)
                        .min(max_sphere_for_window(win));
                    assert!(sphere > 0.0, "no ball at all (win={win})");
                    let clip = clip_diameter_for(sphere, win);
                    let glow = glow_outer_diameter(sphere);
                    assert!(
                        clip >= sphere,
                        "clip {clip} cuts the ball {sphere} (win={win})"
                    );
                    assert!(
                        clip <= win - CLIP_MARGIN + 0.01,
                        "clip {clip} exceeds window {win}"
                    );
                    assert!(
                        clip + 0.01 >= glow + 2.0 * CLIP_PAD,
                        "clip {clip} leaves only {}px beyond the glow {glow} \
                         (win={win}, tokens={tokens}, hovered={hovered})",
                        clip - glow
                    );
                }
            }
        }
    }

    /// The halo fades outward in many small steps — the property that makes it
    /// read as a glow rather than as a set of concentric bands — and never
    /// shrinks inside the ball.
    #[test]
    fn halo_rings_fade_outward_smoothly() {
        assert!(HALO_RINGS >= 12, "too few rings to hide the banding");
        assert!(
            HALO_RING_ALPHA * HALO_RINGS as f32 <= 0.25,
            "per-ring alpha must stay tiny"
        );
        assert!(HALO_INNER_SCALE >= 1.0 && HALO_OUTER_SCALE > HALO_INNER_SCALE);
        for sphere in [MIN_SPHERE, 180.0, MAX_SPHERE * HOVER_SCALE] {
            let mut prev = 0.0;
            for i in 0..HALO_RINGS {
                let d = halo_ring_diameter(sphere, i);
                assert!(d >= sphere - 0.01, "ring {i} ({d}) sits inside {sphere}");
                assert!(d > prev, "ring {i} ({d}) does not grow outward");
                prev = d;
            }
            assert!(
                (halo_ring_diameter(sphere, HALO_RINGS - 1) - glow_outer_diameter(sphere)).abs()
                    < 0.01
            );
            // Consecutive ring diameters differ by well under a pixel for small
            // balls — that is the smoothness.
            let step = (glow_outer_diameter(sphere) - sphere) / (HALO_RINGS - 1) as f32;
            assert!(step < sphere * 0.03, "ring steps of {step}px are visible");
        }
    }

    /// **The text must never break the circle either.** A 9-digit count is wider
    /// than the ball; the fit has to keep it inside the ball's inscribed square —
    /// and the estimate path (used when shaping is unavailable) must be just as
    /// safe, since that is what runs before the text system is ready.
    #[test]
    fn nine_digit_count_fits_the_text_budget() {
        let text = "999,999,999";
        assert_eq!(text.chars().filter(char::is_ascii_digit).count(), 9);
        for win in WINDOWS {
            for tokens in USAGES {
                let sphere = sphere_diameter_for(tokens, false)
                    .min(win * MAX_BALL_FRACTION)
                    .min(max_sphere_for_window(win));
                let budget = text_budget_side(sphere);
                let cap = sphere * NUM_CAP_RATIO;
                let probe = FIT_PROBE_PX.min(cap);
                // The estimate path: the probe width is the conservative bound,
                // so this is exactly what `fit_font_size` computes when the text
                // system gives nothing back.
                let probe_w = estimated_em(text) * probe;
                let size = fit_size_from_probe(probe_w, probe, budget, cap);
                let drawn_w = estimated_em(text) * size;
                assert!(
                    drawn_w <= budget + 0.01,
                    "{text} would be {drawn_w}px wide in a {sphere}px ball \
                     (budget {budget}, size {size}, win={win})"
                );
                // ...and the budget is inside the circle: it is the inscribed
                // square, scaled down by TEXT_BOX_FRACTION.
                assert!(budget * std::f32::consts::SQRT_2 <= sphere + 0.01);
            }
        }
    }

    /// The estimate must be an upper bound for the strings we actually format,
    /// otherwise the fallback path could overflow.
    #[test]
    fn glyph_estimate_is_conservative() {
        assert!(EM_DIGIT > EM_OTHER, "digits are the wide glyphs");
        // A comma is the only separator: "0" and "1,000,000,000" both estimate.
        for text in ["0", "16,777,343", "999,999,999", "1,000,000,000"] {
            let digits = text.chars().filter(char::is_ascii_digit).count() as f32;
            let separators = text.chars().filter(|c| *c == ',').count() as f32;
            assert_eq!(
                (digits + separators) as usize,
                text.chars().count(),
                "{text} has a glyph the estimate does not account for"
            );
            assert!(
                estimated_em(text) >= digits * 0.55,
                "under-estimates {text}"
            );
        }
    }

    /// Number + cost must also fit *vertically* inside the ball, or the two-line
    /// stack would bulge past the rim even with each line narrow enough.
    #[test]
    fn two_line_stack_fits_vertically() {
        for win in WINDOWS {
            let sphere = sphere_diameter_for(999_999_999, false)
                .min(win * MAX_BALL_FRACTION)
                .min(max_sphere_for_window(win));
            let num = sphere * NUM_CAP_RATIO;
            let cost = sphere * COST_CAP_RATIO;
            // Line boxes at ~1.3x the font size, plus the 4px gap of `gap_1`.
            let height = num * 1.3 + cost * 1.3 + 4.0;
            let budget = text_budget_side(sphere) * std::f32::consts::SQRT_2;
            assert!(
                height <= budget + 0.01,
                "two lines are {height}px tall in a {sphere}px ball (budget {budget})"
            );
        }
    }

    /// Even on the smallest plausible window the ball stays positive and inside
    /// it, and the caps only ever shrink it.
    #[test]
    fn ball_is_capped_by_the_real_window() {
        let huge = sphere_diameter_for(u64::MAX / 2, true);
        for win in WINDOWS {
            let capped = huge
                .min(win * MAX_BALL_FRACTION)
                .min(max_sphere_for_window(win));
            assert!(capped <= huge);
            assert!(capped <= win);
            assert!(capped > 0.0);
        }
        // At the nominal size nothing is capped away below MAX_SPHERE.
        assert_eq!(
            huge.min(WINDOW_SIZE * MAX_BALL_FRACTION)
                .min(max_sphere_for_window(WINDOW_SIZE)),
            huge
        );
    }

    /// The first frame must be clipped to a circle that comfortably encloses a
    /// full ball, whatever the window's real size is.
    #[test]
    fn seeded_clip_is_generous_enough_for_any_window() {
        assert!(SEED_CLIP_FRACTION > 0.0 && SEED_CLIP_FRACTION <= 1.0);
        for win in WINDOWS {
            let seeded = win * SEED_CLIP_FRACTION;
            let sphere = sphere_diameter_for(0, false)
                .min(win * MAX_BALL_FRACTION)
                .min(max_sphere_for_window(win));
            assert!(
                seeded > sphere,
                "seeded clip {seeded} must enclose the first-frame ball {sphere} (win={win})"
            );
        }
    }

    /// Re-cutting an identical region forces a full window repaint for nothing,
    /// and a needless re-cut can flicker the rim.
    #[test]
    fn identical_regions_are_not_reapplied() {
        let a = (300.0, 360.0);
        assert!(!region_moved(a, (300.2, 359.8)));
        assert!(region_moved(a, (320.0, 360.0)));
        assert!(region_moved(a, (300.0, 380.0)));
    }

    /// The fit cache must only be reused for the same string *and* budget, so a
    /// stale size can never be applied to a longer number.
    #[test]
    fn fit_cache_is_keyed_by_text_and_budget() {
        let f = TextFit::new("999,999,999", 100.0, 30.0, 12.0);
        assert!(f.matches("999,999,999", 100.0, 30.0));
        assert!(!f.matches("999,999,999", 100.0, 31.0));
        assert!(!f.matches("1,999,999,999", 100.0, 30.0));
        assert!(!f.matches("999,999,999", 101.0, 30.0));
    }
}
