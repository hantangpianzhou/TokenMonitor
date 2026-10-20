//! Always-on-top floating "usage ball".
//!
//! A single sphere filled with the app's **theme accent** (`cx.theme().primary`),
//! drawn in a transparent, frameless popup window — so there is no square OS
//! frame or rectangular shadow, and clicks outside the ball pass through to the
//! desktop (`WM_NCHITTEST` answers `HTTRANSPARENT` beyond the ball; the
//! 360 / Thunder look). Its diameter grows with total token usage
//! (area ∝ usage) and it pops slightly on hover for a floating feel. The total
//! token count and the period it was aggregated over ("今日" / "本周" / …) are
//! overlaid on the sphere; data is pushed from `TokenMonitorApp`.
//!
//! Dropped against the left or right screen edge it **docks** into a rounded
//! square instead — see "Two forms, one card" below.
//!
//! The ball is drawn in code (not a PNG) so the color always follows the
//! theme. A linear gradient + a gradient sheen + a small specular dot give it
//! a rounded, spherical read. A soft accent **halo** behind the ball sells the
//! "floating" glow.
//!
//! # The number always says what it measures
//!
//! The pushed total is the dashboard's **active time-tab** aggregate, so the
//! same ball would mean "today" or "this year" depending on a tab living in
//! another window. A bare number therefore cannot be read on its own — so the
//! ball carries its period as a small label above the count.
//!
//! # The rules, and the bugs that produced them
//!
//! ## 1. The ball is laid out from the window's *own* rectangle
//!
//! Everything is derived from the window's real **client rect**
//! ([`crate::platform::client_size_logical`]) converted to logical px with the
//! window's own scale factor, so the ball is centred on the rectangle the
//! window actually occupies — concentric *by construction*, at any DPI.
//!
//! Using GPUI's `viewport_size()` instead was the bug behind "I can only see
//! part of the circle": the viewport is the size GPUI *believes* the window
//! has, which can disagree with the real client area. A ball centred on the
//! viewport inside a window of the real client size ends up off-centre, and the
//! window edge then severs one rim — leaving a hard, one-sided cut and no
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
//! ## 3. The window keeps real empty space beyond the glow
//!
//! The glow's gaussian tail has to fade below one 8-bit level **inside** the
//! window: nothing clips it any more (there is no `SetWindowRgn`), but the
//! window edge itself must never land on a pixel the glow still visibly
//! covers, or the transparent popup's rectangle would show as a faint square
//! against some desktops. [`card_pad`] keeps that empty ring around the glow
//! — and `clip_diameter_for`, the card's **fit**, is the smallest side that
//! holds the card plus that ring.
//!
//! The fit is derived from the **card**, never from the window. The window is
//! the fixed [`WINDOW_SIZE`] in every form — free and docked alike — which is
//! always comfortably larger than the fit (the invariant tests pin that); the
//! fixed window is what takes the resize machinery out of the picture entirely.
//!
//! ## 4. The glow is one blurred disc, never a stack of discs
//!
//! GPUI's [`gpui::BoxShadow`] is a real gaussian blur of the element's
//! rounded-rect coverage, so a single `shadow` on the ball is a *continuous*
//! radial falloff with no edges anywhere. That is the whole point: every other
//! way to fake a glow in this revision — stacking a few concentric translucent
//! discs — is built out of hard edges, because each disc's rim is snapped to
//! whole device pixels and anti-aliased over exactly one pixel. A handful of
//! discs therefore rings the ball with stair-stepped concentric bands; a
//! screenshot of the 4-disc version showed precisely that, four hard rings at
//! radii 84/88/91/96/101 px. A gaussian has no steps to begin with, at any size.
//!
//! The price is geometry: σ spreads the glow to `GLOW_TAIL_SIGMA * σ` beyond the
//! rim, and that — not the ball — is what the window and the clip must fit.
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
//!
//! # Two forms, one card
//!
//! Dropped against the left or right edge of the screen, the ball **docks**: it
//! morphs into a rounded **square** and the count switches to Chinese units
//! with 万 as the floor ("1.2千万" — see [`format_tokens_wan`]). Dropped
//! anywhere else it morphs back into the circle. Both the form and the position
//! come from the drop: the Win32 drag subclass snaps the window to the edge and
//! reports the decision here, and this view owns the *shape*.
//!
//! The morph is one `morph` value (0 = circle, 1 = square) that the entire
//! render reads — the corner radius of every shading layer, the card's **side**
//! and the label's unit. It is driven by GPUI's frame callback rather than
//! `with_animation`, because a GPUI animation element can only restyle its
//! **own** root: the ball is a stack of *sibling* layers (base, foot shade,
//! sheen, rim) and all of them have to move together.
//!
//! Two things beyond the rounding change when it docks, and both are derived
//! from the same `morph`:
//!
//! * The square is **smaller** than the ball (see [`DOCK_SIDE_RATIO`]). A filled
//!   square reads heavier than a circle of the same side, and an edge widget is
//!   a marker, not a panel. Everything scales with it — glow, corners, text
//!   budget (the inscribed square of whatever side the morph is at, which is
//!   inside a circle and a rounded square alike), the refresh icon.
//! * The card **glides to the edge** the snap left the window at. The window
//!   itself never moves or resizes for the morph; the docked rest position
//!   centres the card on the work-area edge, so the invisible half of the fixed
//!   window hangs off-screen — which is harmless, because the hit test answers
//!   only inside the card's circle and passes everything else through.
//!
//! # The window is fixed — on purpose
//!
//! The window is opened at [`WINDOW_SIZE`] and **never resized again**, in any
//! form, on any frame. Every earlier revision resized it: docked, the window
//! was shrunk to the card plus its glow; the hover pop resized it; the morph
//! pinned itself to an "envelope" size to keep the number of resizes down. Each
//! resize forces DWM to reallocate the window's surface, and the measured cost
//! was 20–33 ms per call — which turned a 240 ms morph into five visible
//! frames — and worse, every *landing* resize left a ~30 ms window in which the
//! compositor could sample the old frame inside the new geometry: the
//! quarter-arc shard of 2026-10-19, and after the region was retired the whole
//! class of settle artefacts that no nudge could fully heal (plan §9.24–§9.32).
//! With the surface constant, a morph is nothing but GPUI drawing — those DWM
//! failure modes have no substrate left.
//!
//! The price is geometry, and it is paid deliberately: the docked card sits in
//! the middle of a window that is much wider than it is, so half the window
//! hangs off the screen edge when docked. That margin is invisible, and it is
//! *inert by construction*: the Win32 hit test answers `HTCAPTION` only inside
//! the card's circle and `HTTRANSPARENT` everywhere else, so the empty half
//! cannot swallow hover or press events, and "where the widget is" for the user
//! is the card, not the rectangle.

use crate::platform::frame_log;
use gpui::{
    div, ease_in_out, percentage, px, Animation, AnimationExt, BoxShadow, Context, Font,
    FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, TextRun, Transformation, Window,
};
use gpui_component::{ActiveTheme, Icon, IconName, Sizable, Size, StyledExt};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::time::{Duration, Instant};

use crate::app::state::TimeTab;
use crate::format::{format_int_grouped, format_tokens_wan};
use crate::platform::{
    client_origin_in_window, client_size_device, monitor_work_area, move_window,
    set_floating_card_frac, set_floating_refresh_hit, snap_floating_window_to_edge, window_rect,
    DockEdge, DOCK_GAP,
};

/// Edge length (px) the floating window is **opened** at — and *stays* at for
/// its whole life. Must stay in sync with `FLOAT_WIN` in `app::app`. The window
/// is **never resized**: every form the ball takes (free circle, docked square,
/// hover pop, every step of the morph between them) is drawn inside this one
/// fixed rectangle, so the surface DWM has to recomposite never changes size —
/// the resize-driven shard/freeze class of failure has no substrate left (see
/// the module note on the fixed window).
const WINDOW_SIZE: f32 = 360.0;
/// **Solid-ball** diameter at zero usage (px). Deliberately an explicit
/// minimum: the resting ball keeps a legible, unmistakably-round silhouette
/// instead of shrinking until the halo and the overlay text dominate it.
/// `ball_is_large_at_every_usage` guards the floor, so it only moves when the
/// ball is deliberately resized — never as a side effect of another tweak.
const MIN_SPHERE: f32 = 112.0;
/// **Solid-ball** diameter at / above the reference usage (px).
///
/// A matched pair with `MIN_SPHERE`: whenever the ball's size is reviewed, *both*
/// ends are scaled by one factor — 144/232 → 128/200 → 112/175 so far. Scaling
/// both keeps `sphere_diameter_for`'s shape (the sqrt ramp, the hover pop, the
/// ball's proportions), so a size tweak only changes how big the ball is, never
/// how it grows with usage. The card's side drives the window (docked:
/// [`window_side_for`]), so the whole widget's footprint follows it.
const MAX_SPHERE: f32 = 175.0;
/// Usage (tokens) at which the ball reaches its maximum diameter.
const REF_TOKENS: f64 = 200_000_000.0;
/// Hover pop scale — the ball lifts / grows a touch when the mouse is over it.
const HOVER_SCALE: f32 = 1.06;
/// Clear gap (px) the fit keeps around the outermost *visible* glow pixel —
/// the invariant tests' bound. (The window itself no longer hugs the glow: it
/// is the fixed [`WINDOW_SIZE`], and this ring lives inside the fit.)
#[cfg(test)]
const CLIP_PAD: f32 = 12.0;
/// Blur radius of the glow, as a multiple of the ball diameter. This is the
/// gaussian's σ, so it sets how far the glow reaches and how soft it is: at
/// 0.085 a ball 175 px across carries a glow that is still worth 2% of its peak
/// a good 30 px out.
const GLOW_BLUR_RATIO: f32 = 0.085;
/// How many σ of the glow tail count as visible — the invariant tests' bound
/// for how much room the fit must leave.
#[cfg(test)]
const GLOW_TAIL_SIGMA: f32 = 2.0;
/// Composited opacity of the glow at the ball's rim, before the blur halves it.
/// A blurred disc covers its own edge with exactly half the source alpha, so
/// this is the strength the glow *peaks* at, right where it meets the ball.
const GLOW_PEAK_ALPHA: f32 = 0.24;
/// Below this, a measured client size is not believable (the client rect can
/// read 0 before the first layout) and the layout falls back to [`WINDOW_SIZE`]
/// rather than drawing into a degenerate rectangle.
const MIN_SANE_WINDOW: f32 = 32.0;

/// Corner roundness of the docked **square**, as the corner ellipse's diameter
/// over the card's side. `1.0` is the circle — the ellipse's diameter equals
/// the side, so the four corners meet — and `0.36` leaves an
/// 18%-of-side radius: a rounded square, not a hard-cornered tile.
const SQUARE_CORNER_RATIO: f32 = 0.36;
/// Side of the docked square, as a fraction of the ball's diameter.
///
/// The docked form is deliberately **smaller** than the circle it came from. A
/// filled square reads as much heavier than a circle of the same side — the
/// four corners are ink where the circle leaves air — and an edge widget is a
/// compact marker, not a panel. Every layer (glow, corners, text budget, icon)
/// is derived from this one side, so the whole widget shrinks together and the
/// overlay still fits: the text is fitted to the inscribed square of whatever
/// side the morph is at.
const DOCK_SIDE_RATIO: f32 = 0.62;
/// Duration of the circle ↔ square morph.
const MORPH_DURATION: Duration = Duration::from_millis(240);

/// Top sheen: a white wash anchored at the top edge fading to nothing, drawn as
/// a circle *the same diameter as the ball* so it cannot change the silhouette.
/// A gradient (not a hard-edged highlight disc) is what keeps the sphere read:
/// an offset opaque circle looks like a second, smaller circle glued on.
const SHEEN_ALPHA: f32 = 0.22;
/// Foot shade: a wash of the ball's own dark tone anchored at the *bottom* edge,
/// fading to nothing upward. The base gradient alone lights the sphere flatly;
/// pairing a bright cap with a dark foot is what makes it read as a lit sphere
/// rather than as a tinted disc.
const SHADE_ALPHA: f32 = 0.26;
/// Rim light: a hairline highlight along the ball's own edge. It crisps the
/// silhouette against the glow instead of letting the two smear into each other.
/// It is a `border`, so it is drawn *inside* the ball's bounds and cannot alter
/// the silhouette.
const RIM_ALPHA: f32 = 0.22;
/// Specular highlights as `(center_x, center_y, radius, opacity)`, all fractions
/// of the ball diameter (0..1). The first is the main glint; the second, smaller
/// and dimmer, is the companion reflection that makes the surface read as glass
/// rather than as one flat dot glued on. Each is kept strictly inside the ball's
/// inscribed circle by [`dot_is_inside_disc`].
///
/// Both sit in the **top-left band, clear of the centred text column**: the
/// overlay label ("今日") is a centred column roughly 0.35..0.65 of the diameter
/// wide, and a glint sliding under it washes the translucent white glyphs into a
/// blob (the "label melts into a dot" regression at zero usage, where the ball
/// is at its smallest and the two collide head-on). Keeping `cx + r <= 0.35`
/// pins every highlight to the left of the label no matter how the ball is
/// sized; [`speculars_stay_clear_of_the_label_column`] guards that bound.
const SPECULARS: [(f32, f32, f32, f32); 2] = [(0.24, 0.16, 0.05, 0.55), (0.46, 0.075, 0.024, 0.32)];

/// Fraction of the ball's inscribed square the overlay text may use. The
/// inscribed square is inside the circle by definition, so anything that fits it
/// cannot break the circular silhouette.
const TEXT_BOX_FRACTION: f32 = 0.92;
/// Ceiling on the number's font size, as a fraction of the ball diameter.
const NUM_CAP_RATIO: f32 = 0.26;
/// Ceiling on the period label's font size, as a fraction of the ball diameter.
/// Always held below [`NUM_CAP_RATIO`]: the count is what the ball is for, the
/// label only qualifies it.
const LABEL_CAP_RATIO: f32 = 0.15;
/// The period label is also capped at this fraction of the number's **fitted**
/// size — not of the ball. The number is scaled down to fit the ball, while two
/// CJK glyphs are far narrower than an 8-digit grouped count, so a label capped
/// only against the ball would keep its full (larger) size and end up towering
/// over the count it is supposed to qualify.
const LABEL_OF_NUM: f32 = 0.62;
/// Vertical centre of the refresh icon, as a fraction of the ball diameter.
/// Pinned near the bottom of the ball, below the count.
const ICON_CY_RATIO: f32 = 0.80;
/// Line box of a text line, in em units of its font size, as GPUI lays it out
/// here. Backed out of a screenshot of the broken zero-usage ball: the glyph
/// gap between label and count matched ~1.6em line boxes plus the flex gap.
const LINE_HEIGHT_EM: f32 = 1.6;
/// Distance from a line's *box bottom* to its glyph bottom, in em: half of the
/// leading (`(LINE_HEIGHT_EM - ~0.84) / 2`). Used to convert the centred
/// stack's box geometry into where the digits actually stop.
const GLYPH_DESCENT_EM: f32 = 0.38;
/// The flex gap between label and count (`gap_1`).
const STACK_GAP: f32 = 4.0;
/// Font size used to *probe* a string's width before scaling it to fit. Any
/// value works (width is proportional to size); 20px keeps rounding small.
const FIT_PROBE_PX: f32 = 20.0;
/// Conservative advance of one glyph, as a fraction of the font size, used when
/// the text system cannot be asked. Upper bounds for a semibold sans face.
const EM_DIGIT: f32 = 0.64;
const EM_OTHER: f32 = 0.36;
/// Advance of one **wide** (non-ASCII) glyph, in the same units. CJK glyphs are
/// near-square, i.e. about one em — roughly 3x the narrow ASCII glyphs above.
/// Estimating them as narrow would under-state the label's width on the
/// estimate path, which is exactly the path that has to be safe.
const EM_WIDE: f32 = 1.0;

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

/// Blur radius (px) — the gaussian's σ — of the glow for a ball of `sphere_d` px.
///
/// Proportional to the ball on purpose: a fixed σ would make the glow a tight
/// collar on a big ball and a diffuse fog around a small one, so the ball would
/// appear to change material as usage grows.
fn glow_blur(sphere_d: f32) -> f32 {
    sphere_d * GLOW_BLUR_RATIO
}

/// Radius (px) out to which the glow is still visible for a ball of `sphere_d`
/// px: the ball's own radius plus [`GLOW_TAIL_SIGMA`] σ of blur tail.
#[cfg(test)]
fn glow_visible_radius(sphere_d: f32) -> f32 {
    sphere_d / 2.0 + GLOW_TAIL_SIGMA * glow_blur(sphere_d)
}

/// Room (px) the fit keeps on each side of the card: the glow's visible tail,
/// plus [`CLIP_PAD`] of empty pixels — the invariant tests' bound.
#[cfg(test)]
fn card_pad(card_d: f32) -> f32 {
    glow_visible_radius(card_d) - card_d / 2.0 + CLIP_PAD
}

/// Diameter (px) of the card's **fit** — the smallest window side that holds a
/// card of `card_d` px plus its glow with [`card_pad`] to spare.
///
/// Derived *from the card*, so the fit can never come out too small for what is
/// drawn. It is the smallest side that holds the card plus the glow ring, and
/// the invariant tests' bound; the *window* is no longer sized from it — the
/// window is [`WINDOW_SIZE`] at every moment of every form (see the module
/// note on the fixed window).
#[cfg(test)]
fn clip_diameter_for(card_d: f32) -> f32 {
    card_d + 2.0 * card_pad(card_d)
}

/// Note on where the frame's rectangle comes from (this used to be a function of
/// its own, `measured_client`): the **client rect** is the only rectangle the
/// layout and `SetWindowRgn` can agree on, so it is the only thing trusted, and
/// it is read **once** — `fit_floating_card_window` measures it, hands it back,
/// and the layout box, `set_floating_card_frac` and the cut all read that one
/// value. Two earlier shapes are both gone on purpose:
///
/// * GPUI's `Window::viewport_size()` as a fallback. It is GPUI's *belief* about
///   the window, which can be twice the real client area while the OS is still
///   applying a resize it made behind the app's back (a `WM_DPICHANGED` rewrites
///   the window rect when the ball crosses the monitor boundary). Laying the card
///   out on one rectangle while the window really is another puts the ball's
///   centre on the client rect's own corner and slices a quarter off — a cut
///   along a *sharp* rectangle, which is how that one was told apart.
/// * A "does this match the size we asked for" sanity test. The measurement is
///   taken right after a synchronous `SetWindowPos`, so a mismatch means DPI
///   rounding (or a frame or two the OS has not painted yet), not a window that
///   is about to be replaced. Treating it as the latter threw away the real
///   rectangle — which is how a window that is *not* that size ends up with a
///   ball nowhere near the centre of the window it sits in.

/// Whether a dot of radius `r` centred at `(cx, cy)` (all fractions of the ball
/// diameter) fits inside the ball's inscribed circle. Guards the specular
/// highlights: they are the only drawn elements smaller than the ball, so they
/// are the only ones that could be placed outside the silhouette by mistake.
fn dot_is_inside_disc(cx: f32, cy: f32, r: f32) -> bool {
    let dx = cx - 0.5;
    let dy = cy - 0.5;
    (dx * dx + dy * dy).sqrt() + r <= 0.5
}

/// Corner roundness (corner-ellipse diameter / side) of the card at morph
/// progress `t`: `1.0` — the circle — at 0, [`SQUARE_CORNER_RATIO`] at 1.
///
/// One value drives the card's drawn corner radii, so the shape on screen is
/// always the one that belongs to the `t` the window side was read at.
fn corner_ratio_for(t: f32) -> f32 {
    1.0 + (SQUARE_CORNER_RATIO - 1.0) * t.clamp(0.0, 1.0)
}

/// Side (px) of the card at morph progress `t`: the ball's own diameter while
/// free (`t = 0`), [`DOCK_SIDE_RATIO`] of it once docked (`t = 1`).
///
/// The morph therefore moves two things at once — the corner radius (see
/// [`corner_ratio_for`]) and this side — and both are read from the same `t`, so
/// what is drawn is always one coherent shape. Linear in `t` on purpose:
/// [`corner_ratio_for`] is linear too, so the shrink and the rounding-up land
/// together instead of one arriving first.
fn card_side_for(sphere_d: f32, t: f32) -> f32 {
    sphere_d * (1.0 - (1.0 - DOCK_SIDE_RATIO) * t.clamp(0.0, 1.0))
}

/// Distance (px) from a rounded rect's centre to its farthest drawn point: the
/// outer edge of a corner arc, along the diagonal.
///
/// It is `side / 2` for a circle (`corner_ratio = 1`) and less than
/// `side / 2 * SQRT_2` for a rounded square — the quantity the window side has
/// to exceed so the card can never poke out of the popup. Only the invariant
/// test needs the number, but the model belongs next to the geometry it
/// describes.
#[cfg(test)]
fn rounded_reach(side: f32, corner_ratio: f32) -> f32 {
    let radius = side * corner_ratio.clamp(0.0, 1.0) / 2.0;
    (side / 2.0 - radius).max(0.0) * std::f32::consts::SQRT_2 + radius
}

/// One specular highlight as `(left, top, diameter)` in **window** px, for a ball
/// of `sphere_d` px whose box starts at `(ball_x, ball_y)`, from a
/// `(center_x, center_y, radius)` fraction triple.
///
/// The ball box offset is added **here**, and the result is an absolute window
/// position. That is deliberate: a specular is positioned from the ball's own
/// fractions, which are relative to the ball's box — not to the window. Returning
/// ball-relative numbers and placing them with `left`/`top` (as this used to)
/// draws the highlights wherever the ball box *starts*, i.e. in the window's
/// top-left corner, off the ball entirely. The two stray white discs the user saw
/// in that corner were exactly these two highlights (their composited alphas were
/// measurable as 0.582 / 0.342, matching `SPECULARS`' 0.58 / 0.34). Folding the
/// offset into the geometry makes forgetting it impossible.
///
/// A specular is the **only** layer that is not a copy of the ball's own circle,
/// so it is the only one that could break the silhouette. If a constant were
/// ever edited to a fraction that pokes past the rim, the centre is pulled back
/// along its own offset vector until the highlight fits inside the inscribed
/// circle — so a bad constant degrades into a highlight in a different spot,
/// never into a teardrop ball.
fn specular_geometry(
    ball_x: f32,
    ball_y: f32,
    sphere_d: f32,
    spec: (f32, f32, f32),
) -> (f32, f32, f32) {
    let (cx, cy, r) = spec;
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
        ball_x + sphere_d * (0.5 + dx) - d / 2.0,
        ball_y + sphere_d * (0.5 + dy) - d / 2.0,
        d,
    )
}

/// The `(left, top, diameter)` box shared by the ball, every full-size shading
/// layer and every halo ring: the [`ball_box_at`] call for a card centred in a
/// `win_w × win_h` box. Production layouts walk the centre (`ball_box_at`);
/// this centred form is the one the layer-share tests reason about.
#[cfg(test)]
fn ball_box(win_w: f32, win_h: f32, sphere_d: f32) -> (f32, f32, f32) {
    ((win_w - sphere_d) / 2.0, (win_h - sphere_d) / 2.0, sphere_d)
}

/// [`ball_box`] with the centre given explicitly rather than assumed at the
/// layout box's middle.
///
/// The render centres the card on the client rect it measured — always the
/// fixed window's middle — and everything downstream (the shading layers, the
/// speculars, the text stack, the refresh icon) derives from this one box, so
/// the layers stay concentric by construction.
fn ball_box_at(center_x: f32, center_y: f32, sphere_d: f32) -> (f32, f32, f32) {
    (
        center_x - sphere_d / 2.0,
        center_y - sphere_d / 2.0,
        sphere_d,
    )
}

/// `morph`'s value `elapsed` into a `from → target` transition.
///
/// Split out of [`FloatingView::advance_morph`] so the easing and the clamping are
/// testable without a window: together they decide how many distinct frames a
/// transition shows, which is the whole of what "smooth" means here.
fn morph_at(from: f32, target: f32, elapsed: Duration) -> f32 {
    let progress = (elapsed.as_secs_f32() / MORPH_DURATION.as_secs_f32()).clamp(0.0, 1.0);
    from + (target - from) * ease_in_out(progress)
}

/// Whether a transition started `elapsed` ago has arrived.
fn morph_arrived(elapsed: Duration) -> bool {
    elapsed >= MORPH_DURATION
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

/// Upward shift (px) for the centred label+count stack so the count clears the
/// refresh icon pinned near the bottom of the ball.
///
/// The stack is flex-centred in the ball, and the icon sits at
/// [`ICON_CY_RATIO`] of the diameter — at zero usage the ball is at
/// [`MIN_SPHERE`], the count's line box reaches ~0.77 of it, and the two used
/// to overlap head-on (the icon ring sliced through the bottom of the "0").
/// The stack is lifted by exactly the overlap, derived from the *fitted* font
/// sizes, so it stays clear at every usage and ball size. The caller applies
/// the shift as bottom padding of the centred column (padding moves a centred
/// child up by half itself, hence the doubling at the call site).
fn overlay_shift_up(sphere_d: f32, fs: f32, label_fs: f32, icon_d: f32) -> f32 {
    let stack_h = LINE_HEIGHT_EM * (label_fs + fs) + STACK_GAP;
    let glyph_bottom = sphere_d / 2.0 + stack_h / 2.0 - GLYPH_DESCENT_EM * fs;
    let icon_top = sphere_d * ICON_CY_RATIO - icon_d / 2.0;
    (glyph_bottom + STACK_GAP - icon_top).clamp(0.0, sphere_d * 0.14)
}

/// Conservative width estimate for `text` in font-size units (em), used when the
/// text system cannot be asked for a real measurement.
fn estimated_em(text: &str) -> f32 {
    text.chars()
        .map(|c| {
            if c.is_ascii_digit() {
                EM_DIGIT
            } else if c.is_ascii() {
                EM_OTHER
            } else {
                EM_WIDE
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

pub struct FloatingView {
    hwnd: isize,
    total_tokens: u64,
    /// The dashboard time tab the pushed total was aggregated over — shown as
    /// the ball's label ("今日" / "本周" / …). Carried as part of the value, not
    /// read from app state, so the label can never describe a different period
    /// than the number it sits on.
    time_tab: TimeTab,
    hovered: bool,
    /// Whether a provider scan is currently in flight. Driven from the main
    /// app's `scan_status` so the refresh icon spins while any scan runs (the
    /// ball's own refresh button routes through the very same scan path).
    refreshing: bool,
    /// Whether the ball is docked against a screen edge — the form it is
    /// *aiming* at. [`Self::morph`] is the animated progress toward it.
    docked: bool,
    /// Which screen edge the card is docked against, or `None` while it is free.
    /// It is what tells a resize which edge of the card to hold still on screen
    /// (`platform::fit_floating_card_window`): docked, the card's **outer edge**
    /// stays where the snap measured it against the screen edge, so the morph and
    /// the hover pop grow and shrink the card *against* that edge instead of
    /// sliding it along the edge.
    ///
    /// It deliberately outlives the dock: the return-to-circle animation is still
    /// anchored on the edge it is leaving, and it is only cleared once that
    /// expansion has landed ([`Self::morph_to`]) — a stale edge would otherwise
    /// make the next hover grow a free ball towards a screen edge it is nowhere
    /// near.
    edge: Option<DockEdge>,
    /// Morph progress: 0 = circle, 1 = square. Every layer of the render reads
    /// it, so the drawn shape is one shape at every point of the transition.
    morph: f32,
    /// Whether a morph is in flight — the window is held at its **envelope**
    /// while it runs (see [`morph_envelope`]), so a 240 ms transition costs two
    /// structural changes instead of fifteen.
    animating: bool,
    /// The in-flight morph's endpoints and start. Held as data rather than as a
    /// running task, because the transition is advanced by
    /// [`Self::advance_morph`] from inside `render` — the one place that already
    /// runs once per displayed frame.
    morph_from: f32,
    morph_target: f32,
    morph_started: Option<Instant>,
    /// One-frame-deferred re-snap owed by the frame a **docking** morph lands
    /// on (see [`Self::advance_morph`]). The drop-time snap has to estimate the
    /// card from the pre-morph fraction, and the landed card is
    /// `DOCK_SIDE_RATIO` of that — so the landing frame instead publishes the
    /// settled fraction and owes the *next* frame a re-snap, which measures the
    /// card that is really on screen and puts its outer edge exactly on
    /// `DOCK_GAP` ([`snap_floating_window_to_edge`]). One frame later is
    /// invisible; a gap-width off is not.
    ///
    /// A re-snap is a position-only move, which cannot clip anything — the
    /// content travels with the window — so deferring it costs no intermediate
    /// state.
    pending_snap: Option<DockEdge>,
    /// Logical client size of the last frame that *proved* it — the OS
    /// verifiably reported this client size when the frame went in.
    ///
    /// The fallback for a frame where no client rect can be read at all (the
    /// window is mid-creation or mid-destruction). The window is never resized,
    /// so in every normal case this is simply [`WINDOW_SIZE`]; the field exists
    /// so a frame that cannot measure still lays out on a rectangle that
    /// really exists.
    last_verified_client: (f32, f32),
    /// Cached fit of the token-count string.
    num_fit: Option<TextFit>,
    /// Cached fit of the period label.
    label_fit: Option<TextFit>,
}

impl FloatingView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // **The presentation heartbeat.** A resting ball draws no frames of its
        // own — the morph's `request_animation_frame` chain ends at landing,
        // and nothing wakes the window until an input event. If the window was
        // fully occluded mid-morph, GPUI/DWM can stop presenting and the surface
        // keeps stale pixels while the app's state stays perfectly healthy —
        // invisible from inside the app. Every 800 ms this watch invalidates
        // the whole client (forcing the next present to cover the window) and
        // wakes one render. It runs **between** frames, so it can only ever
        // observe the settled state, never a transition mid-flight.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(800))
                .await;
            let _ = this.update(cx, |view, cx| {
                if !crate::platform::is_window_alive(view.hwnd) {
                    // The window is gone; logging it once beats silently
                    // spinning forever and hiding a stale-hwnd situation.
                    frame_log(&format!(
                        "sentinel: hwnd {:#x} no longer alive — watch ends",
                        view.hwnd
                    ));
                    return;
                }
                crate::platform::invalidate_whole_window(view.hwnd);
                crate::platform::nudge_dwm_recomposition(view.hwnd);
                cx.notify();
            });
        })
        .detach();
        // Mark the log with this instance's hwnd: a second start line with a
        // different hwnd means the window was recreated, and every stale-hwnd
        // suspect becomes visible in the log.
        frame_log(&format!("floating-start hwnd={:#x}", hwnd_of(window)));
        Self {
            hwnd: hwnd_of(window),
            total_tokens: 0,
            time_tab: TimeTab::default(),
            hovered: false,
            refreshing: false,
            docked: false,
            edge: None,
            morph: 0.0,
            animating: false,
            morph_from: 0.0,
            morph_target: 0.0,
            morph_started: None,
            pending_snap: None,
            last_verified_client: (WINDOW_SIZE, WINDOW_SIZE),
            num_fit: None,
            label_fit: None,
        }
    }

    /// Current solid-ball diameter (px) before the window caps are applied.
    fn sphere_diameter(&self) -> f32 {
        sphere_diameter_for(self.total_tokens, self.hovered)
    }

    /// Push the latest usage (called from `TokenMonitorApp` on every scan).
    /// `time_tab` is the period the total was aggregated over; both travel
    /// together so the ball can label the number with its own scope.
    pub fn set_usage(&mut self, total_tokens: u64, time_tab: TimeTab) {
        self.total_tokens = total_tokens;
        self.time_tab = time_tab;
    }

    /// Show or hide the in-flight refresh spinner on the refresh icon.
    pub fn set_refreshing(&mut self, refreshing: bool) {
        self.refreshing = refreshing;
    }

    /// Dock the ball into its square form against `edge`, or release it back
    /// into the circle (`None`) — animated either way. Called from the app when
    /// the ball's Win32 drag subclass reports where it was dropped; the window
    /// has already been snapped to (or away from) the screen edge by then, and
    /// the morph glides the card to (or back from) the edge the snap left the
    /// window at. The window itself never resizes — see the module note.
    ///
    /// The edge is kept only while it is being *entered*: coming back out keeps
    /// the last one so the glide heads for the same side for the whole
    /// animation, and it is dropped once the expansion has landed (see
    /// [`Self::edge`]).
    pub fn set_docked(&mut self, edge: Option<DockEdge>, cx: &mut Context<Self>) {
        let docked = edge.is_some();
        // A repeat of the same report (or a free drop after a free drop) has
        // nothing to change — not even the edge, which an un-dock deliberately
        // leaves alone.
        let no_op = self.docked == docked && (edge.is_none() || edge == self.edge);
        // **Docked → docked on the *other* edge** runs no morph (the form does
        // not change), so nothing would ever move the card to the new side:
        // owe a re-snap right here. The next render's
        // `snap_floating_window_to_edge` measures the settled card and puts it
        // on the new edge's `DOCK_GAP` line.
        let re_snap_same_form = self.docked && docked && edge.is_some_and(|e| Some(e) != self.edge);
        if let Some(edge) = edge {
            self.edge = Some(edge);
        }
        // An un-dock cancels a re-snap that has not run yet: the edge it was
        // owed for is no longer the shape the ball is heading for, and sliding
        // it against that edge would fight the expansion that is starting.
        if edge.is_none() {
            self.pending_snap = None;
        }
        if re_snap_same_form {
            self.pending_snap = edge;
        }
        if no_op {
            return;
        }
        self.docked = docked;
        self.morph_to(if docked { 1.0 } else { 0.0 }, cx);
        // Notify straight away as well: the count switches to (or from) Chinese
        // units on `docked`, so the first frame shows the new text even before
        // the morph's first tick.
        cx.notify();
    }

    /// Animate [`Self::morph`] to `target` (0 = circle, 1 = square).
    ///
    /// Driven by GPUI's own frame callback rather than `with_animation`, because
    /// the shape is *several* sibling layers that all have to read the same
    /// progress — a GPUI animation element can only restyle its own root.
    ///
    /// All this does is **start** the transition: it records where it starts and
    /// asks for a repaint. The advancing itself happens in [`Render::render`],
    /// which is the only place that already runs once per displayed frame.
    fn morph_to(&mut self, target: f32, cx: &mut Context<Self>) {
        let from = self.morph;
        if (target - from).abs() < 0.001 {
            self.morph = target;
            self.animating = false;
            self.morph_started = None;
            return;
        }
        // Nothing else to snapshot: the window is fixed and the card is
        // centred in it every frame, so the transition is carried entirely by
        // `morph` and the frames the render loop is about to draw. A morph
        // reversed mid-flight simply turns around — the card is where the
        // last drawn frame put it, and the easing starts from the current
        // `morph` value.
        self.morph_from = from;
        self.morph_target = target;
        self.morph_started = Some(Instant::now());
        self.animating = true;
        cx.notify();
    }

    /// Advance the in-flight morph to the frame being drawn, and queue the next
    /// one if it has not arrived.
    ///
    /// The advance lives here, in `render`, rather than in a background timer,
    /// because of what the two cost. Measured on this widget: `render`'s own body
    /// is **0.9–2.0 ms** per frame at the fixed window size, while a 16 ms
    /// `background_executor` timer delivered
    /// frames **19–77 ms** apart. The timer is not slow — it wakes on time — but
    /// it wakes on *its own* schedule, which has no relation to the display's
    /// refresh: a wake-up that lands just after a vsync waits a whole refresh
    /// period for the next one, so the interval alternates 16.7 / 33.3 ms and
    /// averages ~30 ms. That is the jank, and it is invisible in the numbers you
    /// get from timing `render` alone.
    ///
    /// `request_animation_frame` is the same clock the window is composited on,
    /// so one callback is one displayed frame, and the 240 ms transition gets the
    /// ~14 frames it was always budgeted for.
    fn advance_morph(&mut self, window: &mut Window) {
        let Some(started) = self.morph_started else {
            return;
        };
        let elapsed = started.elapsed();
        self.morph = morph_at(self.morph_from, self.morph_target, elapsed);
        if !morph_arrived(elapsed) {
            // One callback per displayed frame. No closure needed: the callback
            // notifies whatever view is rendering, which is this one.
            window.request_animation_frame();
            return;
        }
        self.morph = self.morph_target;
        self.morph_started = None;
        self.animating = false;
        // **The landing is pure state.** The window did not move or resize
        // during the transition, so there is no geometry to settle. What is
        // owed is only the docked re-snap: the drop-time snap estimated the
        // card from the pre-morph fraction, and the settled card is
        // `DOCK_SIDE_RATIO` of that estimate. The next frame publishes the
        // settled fraction and re-snaps from it, putting the card's outer edge
        // exactly on `DOCK_GAP` (see `snap_floating_window_to_edge`).
        if self.morph_target == 1.0 {
            if let Some(edge) = self.edge {
                self.pending_snap = Some(edge);
                // Nothing repaints a ball at rest on its own: without this
                // the owed re-snap would never run.
                window.request_animation_frame();
            }
        }
        // Undock landing: let go of the edge. Nothing else to do — the card is
        // already centred in the fixed window, wherever the drag left it.
        if self.morph_target == 0.0 && !self.docked {
            self.edge = None;
        }
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
        // Advance the transition first: everything below — the corner radius,
        // the card's side, the label's unit — reads `self.morph`, and this frame
        // is the first one that knows its own timestamp.
        self.advance_morph(window);
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

        // Morph progress: 1 = docked square, 0 = free circle. One value drives
        // the roundness and the card's side, so the shape on screen is one
        // shape at every point of the transition.
        let morph = self.morph.clamp(0.0, 1.0);
        let card_d = card_side_for(self.sphere_diameter(), morph);
        let corner_ratio = corner_ratio_for(morph);

        // **The window is fixed — no `SetWindowPos` of any kind runs for
        // layout.** Not a resize, not an envelope pin, not an origin
        // compensation: the card is centred in the client area the window
        // actually has (for every normal frame the [`WINDOW_SIZE`] the window
        // was opened at), and the morph, the hover pop and the usage growth
        // are all carried by what this function *draws*, never by the window's
        // geometry. DWM never sees the surface change size, so the whole class
        // of resize-sampled artefacts has nothing left to sample.
        let scale = window.scale_factor();
        let sane = scale > 0.0 && scale.is_finite();
        let measured = if sane { client_size_device(hwnd) } else { None }
            .filter(|(w, h)| (*w as f32) >= MIN_SANE_WINDOW && (*h as f32) >= MIN_SANE_WINDOW);
        let (win_w, win_h) = match measured {
            Some((w, h)) => {
                let logical = (w as f32 / scale, h as f32 / scale);
                // A measured, sane client rect is the proof itself; the field
                // only carries that proof across a frame that cannot measure.
                self.last_verified_client = logical;
                logical
            }
            // No believable client rect: the window still has whatever geometry
            // the last verified frame gave it. Lay out in *that* rather than in
            // a size nothing vouches for.
            None => self.last_verified_client,
        };
        let win = win_w.min(win_h);

        // The card as a fraction of the client area it was actually laid out in.
        // The dock snap (`platform::windows::dock_and_snap`) turns "the card's
        // own edge" into screen px with it, so it is published every frame —
        // the drop-time snap and the post-landing re-snap both read it.
        set_floating_card_frac(card_d / win.max(1.0));

        // The layout centre: always the client rect's own middle. The fixed
        // window and the centred card are concentric by construction, at rest
        // and mid-morph alike — a transition changes what is *drawn*, never
        // where the card's centre sits.
        let (center_x, center_y) = (win_w / 2.0, win_h / 2.0);

        // One line per frame while `%TEMP%\tm-frame.on` exists: everything the
        // frame laid out, next to what the OS currently believes. The ball
        // sliced by a sharp rectangle is a layout box and a client rect
        // disagreeing; this is the only place both numbers are on the same line,
        // and the message side of the log (see `platform::frame_log`) is what
        // dates the disagreement to the `WM_DPICHANGED` that caused it.
        crate::platform::frame_log(&format!(
            "frame morph={morph:.3} anim={} edge={:?} docked={} hovered={} \
             sphere={:.1} card={card_d:.1} scale={scale:.4} \
             ratio={corner_ratio:.2} client={win_w:.1}x{win_h:.1} center={center_x:.1},{center_y:.1} \
             device={:?} snap={:?}",
            self.animating,
            self.edge,
            self.docked,
            self.hovered,
            self.sphere_diameter(),
            client_size_device(hwnd),
            self.pending_snap,
        ));

        // **Docked, aim the window so the card's outer edge stays on the dock
        // line.** The card is centred in the fixed window, so anything that
        // changes its size (the dock morph's shrink, the hover pop, usage
        // growth) would otherwise let its outer edge drift off `DOCK_GAP` —
        // the fix is a position-only move of the window, which cannot clip
        // anything (the surface content travels with the origin). No resize
        // ever runs here. Undocked there is no line to hold, and the card
        // simply grows around its centre.
        if sane && self.docked {
            if let Some(edge) = self.edge {
                let card_dev = card_d * scale;
                let line = match edge {
                    DockEdge::Right => monitor_work_area(hwnd)
                        .map(|(_, _, area_r, _)| area_r as f32 - DOCK_GAP * scale - card_dev / 2.0),
                    DockEdge::Left => monitor_work_area(hwnd)
                        .map(|(area_l, _, _, _)| area_l as f32 + DOCK_GAP * scale + card_dev / 2.0),
                };
                if let Some(line_x) = line {
                    let (ox, _) = client_origin_in_window(hwnd);
                    let centre_dev = ox as f32 + win_w / 2.0 * scale;
                    let x = (line_x - centre_dev).round() as i32;
                    if let Some((wx, wy, _, _)) = window_rect(hwnd) {
                        if x != wx {
                            move_window(hwnd, x, wy);
                        }
                    }
                }
            }
        }

        // The docking landing owes its re-snap to the *next* frame (see
        // `advance_morph`); this is that frame. The fraction published above now
        // describes the card that is actually on screen, so the snap puts its
        // outer edge exactly on `DOCK_GAP` instead of on the pre-morph estimate
        // (which came out ~7 px wide). A position-only move cannot clip anything
        // — content travels with the window — so it is safe to run
        // after every other geometry change of the frame, present included.
        if let Some(edge) = self.pending_snap.take() {
            snap_floating_window_to_edge(hwnd, edge);
        }

        // Centre on each axis independently, so a client area that is not exactly
        // square (a frame during window creation) still keeps every layer
        // concentric at the client's own centre.
        let (card_left, card_top, _) = ball_box_at(center_x, center_y, card_d);
        let ball_x = px(card_left);
        let ball_y = px(card_top);

        // Docked, the count is read as text rather than digit-by-digit, so it
        // switches to Chinese units with 万 as the floor ("1.2千万").
        let tokens = if self.docked {
            format_tokens_wan(self.total_tokens)
        } else {
            format_int_grouped(self.total_tokens)
        };
        let period = self.time_tab.label().to_string();
        // Fit both lines to the ball's inscribed square, so an ever-growing
        // number (0 → 999,999,999 and past it) can never run into the rim and
        // stop the whole thing reading as a circle.
        let budget = text_budget_side(card_d);
        let fs = Self::fitted_size(
            &mut self.num_fit,
            window,
            &tokens,
            budget,
            card_d * NUM_CAP_RATIO,
            FontWeight::SEMIBOLD,
        );
        // The label is capped against the *fitted* number (see `LABEL_OF_NUM`),
        // so the count stays the dominant line however long it grows.
        let label_cap = (fs * LABEL_OF_NUM).min(card_d * LABEL_CAP_RATIO);
        let label_fs = Self::fitted_size(
            &mut self.label_fit,
            window,
            &period,
            budget,
            label_cap,
            FontWeight::NORMAL,
        );

        let mut root = div()
            .id("floating-root")
            .size_full()
            .relative()
            // The ball is dragged with the native pointer entirely by the Win32
            // subclass in `platform::windows::mod`, which answers `WM_NCHITTEST`
            // with `HTCAPTION` itself. So we deliberately use **no**
            // `WindowControlArea::Drag` here: relying on GPUI's control area made
            // the press go through the OS *non-client* caption loop (black square
            // on this transparent frameless window) and left the drag broken,
            // because during that loop the OS sends `WM_NCMOUSEMOVE`, not
            // `WM_MOUSEMOVE`, which the tracker did not handle. Owning the hit test
            // in the subclass keeps the whole interaction in one place and off
            // GPUI's message plumbing.
            //
            // Clicks outside the ball still pass through to the desktop because
            // the drag subclass answers `WM_NCHITTEST` with `HTTRANSPARENT`
            // beyond the ball's own rect (the 360 / Thunder look); only hits on
            // the ball itself reach the window.
            .on_hover(move |hovered, _win, app| {
                me.update(app, |this, cx| {
                    this.hovered = *hovered;
                    cx.notify();
                });
            });

        // --- 1. The sphere, plus the glow that belongs to it.
        //
        // The glow is the ball's own *drop shadow* — a real gaussian blur of the
        // ball's rounded-rect coverage, painted by GPUI just before the ball's
        // background, so it is automatically behind every layer below and
        // concentric with the ball by construction. One blurred disc is a
        // continuous radial falloff; any stack of discs would be a stack of hard,
        // snapped, 1px-anti-aliased rims, i.e. the visible concentric bands.
        //
        // All the finish layers are the *same* rounded square occupying the
        // *same* box, so their union is exactly one silhouette and it can never
        // become a teardrop. No `overflow_hidden` anywhere: it clips rectangles,
        // not rounded corners, so it would not have contained anything anyway.
        // (`rounded_full` at rest and the docked corner ratio mid-morph are the
        // same thing here — `corner_ratio` IS the radius, as a fraction of the
        // side, times 2.)
        let corner_px = card_d * corner_ratio / 2.0;
        let ball_disc = || {
            div()
                .absolute()
                .top(ball_y)
                .left(ball_x)
                .w(px(card_d))
                .h(px(card_d))
                .rounded(px(corner_px))
        };
        root = root.child(
            ball_disc()
                .bg(gpui::linear_gradient(
                    155.0,
                    gpui::linear_color_stop(lighter, 0.0),
                    gpui::linear_color_stop(darker, 1.0),
                ))
                // The glow. Doubled because a gaussian covers a blurred edge with
                // exactly half the source alpha, so `2 * GLOW_PEAK_ALPHA` is what
                // makes the glow peak at `GLOW_PEAK_ALPHA` where it leaves the rim.
                .shadow(vec![BoxShadow::new(
                    px(0.),
                    px(0.),
                    accent.opacity(GLOW_PEAK_ALPHA * 2.0),
                )
                .blur_radius(px(glow_blur(card_d)))]),
        );
        root = root
            // Foot shade: the ball's own dark tone, clear at the top and heaviest
            // at the bottom, so the sphere reads as lit from above.
            .child(ball_disc().bg(gpui::linear_gradient(
                180.0,
                gpui::linear_color_stop(darker.opacity(0.0), 0.0),
                gpui::linear_color_stop(darker.opacity(SHADE_ALPHA), 1.0),
            )))
            // Top sheen: white at the top edge, fading out downward.
            .child(ball_disc().bg(gpui::linear_gradient(
                180.0,
                gpui::linear_color_stop(WHITE.opacity(SHEEN_ALPHA), 0.0),
                gpui::linear_color_stop(WHITE.opacity(0.0), 1.0),
            )));
        for (i, &(cx, cy, r, alpha)) in SPECULARS.iter().enumerate() {
            let (dot_x, dot_y, dot_d) = specular_geometry(card_left, card_top, card_d, (cx, cy, r));
            root = root.child(
                div()
                    .id(("specular", i))
                    .absolute()
                    .top(px(dot_y))
                    .left(px(dot_x))
                    .w(px(dot_d))
                    .h(px(dot_d))
                    .rounded_full()
                    .bg(WHITE.opacity(alpha)),
            );
        }
        // Rim light last, so no later layer washes it out. A `border` follows the
        // rounded corner radius — this is a hairline ring, not a disc — and it is
        // drawn inside the bounds, so it cannot touch the silhouette.
        root = root.child(
            ball_disc()
                .border_1()
                .border_color(WHITE.opacity(RIM_ALPHA)),
        );

        // --- 2. The overlaid period label + token count, centred in the same box
        // as the ball. A flex row would fight the absolute layers, so this is one
        // absolutely positioned column sized to the ball. It is also a drag
        // handle so the centre of the ball (under the text) is grabbable, not
        // just the rim.
        // --- hit zone for the refresh icon, pinned to the lower-centre of the
        // ball just below the token count. Purely visual here; the actual click
        // is routed by the Win32 subclass (`ball_drag_subclass`) from this rect
        // to `TrayCommand::Refresh`. The rect is in the window's logical px so
        // the subclass can map the screen-px press into the same space.
        // Shrunk on a deliberately tiny window, where a full-size icon would
        // not fit between the count and the bottom rim.
        let icon_d: f32 = 22.0_f32.min(card_d * 0.22);
        let icon_cx = card_left + card_d / 2.0;
        let icon_cy = card_top + card_d * ICON_CY_RATIO;
        let icon_l = icon_cx - icon_d / 2.0;
        let icon_t = icon_cy - icon_d / 2.0;
        // Lift the label+count stack so the count's glyphs stop above the icon
        // (see `overlay_shift_up`): at the minimum ball the centred stack used
        // to run straight into the icon ring.
        let overlay_up = overlay_shift_up(card_d, fs, label_fs, icon_d);
        set_floating_refresh_hit(Some((icon_l, icon_t, icon_l + icon_d, icon_t + icon_d)));

        root = root.child(
            div()
                .absolute()
                .top(ball_y)
                .left(ball_x)
                .w(px(card_d))
                .h(px(card_d))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                // Shift the stack up by `2 * overlay_up` so that a centred
                // layout lands `overlay_up` higher — clearing the refresh icon
                // below (see `overlay_shift_up`).
                .pb(px(2.0 * overlay_up))
                .gap_1()
                .child(
                    // The scope the number was aggregated over. Dimmer and
                    // smaller than the count, but always present: the ball is
                    // driven by the dashboard's active tab, so the same digits
                    // can mean "today" or "this year".
                    div()
                        .text_color(WHITE.opacity(0.72))
                        .text_size(px(label_fs))
                        .child(period),
                )
                .child(
                    div()
                        .text_color(WHITE)
                        .text_size(px(fs))
                        .font_semibold()
                        .child(tokens),
                ),
        );

        // --- 1.5 Refresh icon: a small button pinned to the lower-centre of the
        // ball, just **below the token count**. It is **purely visual here** — the
        // click is handled by the ball's Win32 subclass (`ball_drag_subclass`),
        // which reads the hit-rect we wrote above and routes a press on it to
        // `TrayCommand::Refresh` instead of a drag / double-click-open.
        // The refresh icon spins while a scan is in flight (see `refreshing`,
        // set from the main app's `scan_status`). A static `↻` otherwise.
        let refresh_icon = div()
            .id("refresh-icon")
            .absolute()
            .top(px(icon_t))
            .left(px(icon_l))
            .w(px(icon_d))
            .h(px(icon_d))
            .rounded_full()
            .bg(WHITE.opacity(if self.hovered { 0.34 } else { 0.18 }))
            .border_1()
            .border_color(WHITE.opacity(0.55))
            .flex()
            .items_center()
            .justify_center()
            .child({
                // ↻ refresh glyph. The rotating element must be an `Icon`, because
                // in this locked gpui version only `Icon` (not `Div`) exposes a
                // `transform`/`rotate` method — `Div::rotate` does not exist, which
                // is exactly why the previous `this.rotate(...)` failed to compile.
                // `LoaderCircle` is the lucide circular-arrow (refresh) icon.
                let glyph = Icon::new(IconName::LoaderCircle)
                    .text_color(WHITE)
                    .with_size(Size::Size(px(13.0)));
                if self.refreshing {
                    glyph
                        .with_animation(
                            "floating-refresh-spin",
                            Animation::new(Duration::from_millis(900)).repeat(),
                            |this, delta| this.transform(Transformation::rotate(percentage(delta))),
                        )
                        .into_any_element()
                } else {
                    glyph.into_any_element()
                }
            });
        root.child(refresh_icon)
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
    use crate::platform::dock_window_x;

    /// Ball diameters spanning the whole documented range, hover pop included:
    /// the floor, two mid-ramp sizes, the ceiling, and the ceiling popped. The
    /// **window** is no longer a free variable — it is derived from the card
    /// (`window_side_for`), so the tests that used to sweep window sizes sweep
    /// card sizes instead.
    const BALLS: [f32; 5] = [
        MIN_SPHERE,
        120.0,
        140.0,
        MAX_SPHERE,
        MAX_SPHERE * HOVER_SCALE,
    ];
    /// Morph progress: the free circle, the middle of the animation, the docked
    /// square — the three shapes the same geometry has to serve.
    const MORPHS: [f32; 3] = [0.0, 0.5, 1.0];
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
        // The resting ball's floor, as a named bound: a deliberate resize has to
        // lower it on purpose rather than let the size drift past it.
        assert!(
            MIN_SPHERE >= 110.0,
            "the resting ball must stay big enough to read as a circle (got {MIN_SPHERE})"
        );
        assert!(MAX_SPHERE > MIN_SPHERE);
        for tokens in USAGES {
            for hovered in [false, true] {
                let d = sphere_diameter_for(tokens, hovered);
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

    /// **The regression that made the ball a teardrop.** A specular is the only
    /// drawn element that does not share the ball's bounding box, so it is the
    /// only one that can be placed outside the silhouette by a careless fraction.
    /// Pin every highlight to the inscribed circle.
    #[test]
    fn specular_dots_stay_inside_the_ball_disc() {
        for &(cx, cy, r, _) in SPECULARS.iter() {
            assert!(
                dot_is_inside_disc(cx, cy, r),
                "specular ({cx}, {cy}, r={r}) escapes the ball's inscribed circle"
            );
        }
        // ...and the guard actually rejects a protruding dot (the shape of the
        // old inner-shadow bug: offset 0.26, radius 0.30 -> 0.56 > 0.5).
        assert!(!dot_is_inside_disc(0.60, 0.74, 0.30));
    }

    /// **The stray-white-disc regression.** A specular is positioned from the
    /// ball's own fractions, so its box has to be offset by the ball box origin.
    /// Using those ball-relative numbers directly as window positions drew both
    /// highlights in the window's top-left corner, off the ball — the two grey
    /// discs the user saw there. For every window × ball size the result must
    /// therefore be a box *on the ball*, inside the inscribed circle, and inside
    /// the window.
    #[test]
    fn specular_geometry_never_escapes_the_ball() {
        for ball in BALLS {
            for morph in MORPHS {
                let sphere = card_side_for(ball, morph);
                let win = WINDOW_SIZE;
                let (bx, by, _) = ball_box(win, win, sphere);
                for &(cx, cy, r, _) in SPECULARS.iter() {
                    let (left, top, d) = specular_geometry(bx, by, sphere, (cx, cy, r));
                    let r_px = d / 2.0;
                    let dist = ((left + r_px - (bx + sphere / 2.0)).powi(2)
                        + (top + r_px - (by + sphere / 2.0)).powi(2))
                    .sqrt();
                    assert!(
                        dist + r_px <= sphere / 2.0 + 0.01,
                        "specular escapes: ball {sphere} at ({bx},{by}), \
                         dot r {r_px} at dist {dist}"
                    );
                    // Drawn *over the ball*, never at the window's origin.
                    assert!(
                        left >= bx && top >= by,
                        "specular ({left},{top}) is off the ball box ({bx},{by}, d={sphere})"
                    );
                    assert!(left + d <= bx + sphere && top + d <= by + sphere);
                    assert!(left + d <= win && top + d <= win);
                }
            }
        }
    }

    /// Every shading layer shares the ball's box, so their union is one circle.
    /// Also pins per-axis centring, so a non-square client area cannot shift a
    /// layer off the ball.
    #[test]
    fn shading_layers_share_the_ball_box() {
        for ball in BALLS {
            for morph in MORPHS {
                let card = card_side_for(ball, morph);
                // The window is fixed and square; a non-square client area
                // exists only on a window-creation frame, and the per-axis
                // centring has to survive even that.
                for (win_w, win_h) in [
                    (WINDOW_SIZE, WINDOW_SIZE),
                    (WINDOW_SIZE + 40.0, WINDOW_SIZE),
                ] {
                    let (left, top, d) = ball_box(win_w, win_h, card);
                    assert_eq!(d, card, "the shared box must be the card's size");
                    assert!(left >= 0.0 && top >= 0.0);
                    assert!(left + d <= win_w && top + d <= win_h);
                    assert!((left - (win_w - card) / 2.0).abs() < 0.01);
                    assert!((top - (win_h - card) / 2.0).abs() < 0.01);
                }
            }
        }
    }

    /// **The fixed window always has room for the card.** For every usage and
    /// every step of the morph, [`WINDOW_SIZE`] has to hold the card plus its
    /// glow with [`CLIP_PAD`] to spare — the fit is the smallest side that
    /// does, and the fixed window is comfortably larger in every form.
    #[test]
    fn the_fixed_window_wraps_the_card_and_its_glow() {
        for tokens in USAGES {
            for hovered in [false, true] {
                let ball = sphere_diameter_for(tokens, hovered);
                for morph in MORPHS {
                    let card = card_side_for(ball, morph);
                    let fit = clip_diameter_for(card);
                    let tail = GLOW_TAIL_SIGMA * glow_blur(card);
                    let pad = card_pad(card);
                    assert!(fit > card, "the fit leaves no room for the glow");
                    assert!(
                        (pad - (tail + CLIP_PAD)).abs() < 0.01,
                        "the room per side is {pad}px, not the glow's tail \
                         ({tail}px) plus CLIP_PAD ({CLIP_PAD}px) — card {card}, \
                         tokens {tokens}, t={morph}"
                    );
                    // The fit's edge sits `CLIP_PAD` beyond the glow's visible
                    // tail: still empty, or the edge would land on a visible pixel.
                    assert!(
                        pad >= tail + 1.0,
                        "the fit's edge ({pad}px) reaches the glow tail ({tail}px)"
                    );
                    // The fixed window clears the fit — a side smaller than the
                    // fit would shave the rim.
                    assert!(
                        WINDOW_SIZE >= fit - 0.01,
                        "the fixed window ({WINDOW_SIZE}) is smaller than the fit \
                         it has to hold ({fit}) — card {card}, t={morph}"
                    );
                }
            }
        }
    }

    /// **The window is never resized — in any form.** The card is centred on
    /// the client rect, so as long as the window stays at [`WINDOW_SIZE`] the
    /// ball is concentric with the window *by construction*, free or docked,
    /// at rest or mid-morph. Resizing it was the substrate of every shard and
    /// settle artefact on record.
    #[test]
    fn the_window_stays_fixed_in_every_form() {
        for tokens in USAGES {
            for hovered in [false, true] {
                for morph in MORPHS {
                    let card = card_side_for(sphere_diameter_for(tokens, hovered), morph);
                    // ...and the fit is comfortably inside the window — the
                    // glow plus CLIP_PAD, in the docked form as much as the free.
                    let fit = clip_diameter_for(card);
                    assert!(
                        fit < WINDOW_SIZE,
                        "a fit as big as the window leaves the glow no empty ring"
                    );
                }
            }
        }
    }

    /// **The banding test.** A glow built from a handful of translucent discs is
    /// invisible-*looking* in code but stair-stepped on screen: every disc rim is
    /// snapped to whole device pixels and anti-aliased over exactly one pixel, so
    /// each rim is a hard circle. The glow therefore has to be a single gaussian
    /// blur, which is smooth at every radius by construction.
    ///
    /// Guard the properties that make it a glow rather than a smudge or a ring:
    /// it starts at the ball's rim (never inside it, where it would be wasted
    /// behind the opaque ball), it is wide enough to read as light spilling off
    /// the ball, and it is capped so the clip and the window always have room for
    /// its tail.
    #[test]
    fn glow_is_a_gaussian_blur_not_a_stack_of_discs() {
        // A blur of `GLOW_BLUR_RATIO * d` with a 2σ tail has to clear the ball
        // and still read as a glow, not as a hairline.
        assert!((0.05..=0.15).contains(&GLOW_BLUR_RATIO));
        assert!(GLOW_TAIL_SIGMA >= 2.0, "a 1σ tail would cut a visible edge");
        assert!((0.05..=0.5).contains(&GLOW_PEAK_ALPHA));

        for sphere in [MIN_SPHERE, 170.0, MAX_SPHERE, MAX_SPHERE * HOVER_SCALE] {
            let blur = glow_blur(sphere);
            let visible = glow_visible_radius(sphere);
            assert!(blur > 0.0 && visible > sphere / 2.0);
            // Wide enough to be a glow (at least ~10% of the ball's radius), and
            // the blur grows with the ball so the material does not change with
            // usage.
            assert!(
                visible - sphere / 2.0 >= sphere * 0.05,
                "glow tail {}px is too thin to read as a glow",
                visible - sphere / 2.0
            );
            // The blur costs window space (the fit is the card plus the tail),
            // so a glow reaching too far would outgrow even the fixed window.
            assert!(
                clip_diameter_for(MAX_SPHERE * HOVER_SCALE) <= WINDOW_SIZE,
                "the glow's tail outgrows the fixed window"
            );
        }
        // Blur tracks the ball: the ratio is what stays constant.
        assert!(
            (glow_blur(2.0 * MIN_SPHERE) / glow_blur(MIN_SPHERE) - 2.0).abs() < 1e-4,
            "the glow must scale with the ball"
        );
    }

    /// **The text must never break the circle either.** A 9-digit count is wider
    /// than the ball; the fit has to keep it inside the ball's inscribed square —
    /// and the estimate path (used when shaping is unavailable) must be just as
    /// safe, since that is what runs before the text system is ready.
    #[test]
    fn nine_digit_count_fits_the_text_budget() {
        let text = "999,999,999";
        assert_eq!(text.chars().filter(char::is_ascii_digit).count(), 9);
        for ball in BALLS {
            for morph in MORPHS {
                let sphere = card_side_for(ball, morph);
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
                     (budget {budget}, size {size})"
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

    /// The text must also fit *vertically* inside the ball: the ball only
    /// guarantees an inscribed square, so a font big enough to make the line
    /// boxes taller than that square would bulge past the rim even though each
    /// string is narrow enough. Guards the label + count stack and the gap
    /// between them. (Both sizes are taken at their *caps*, which bounds the
    /// real, width-fitted sizes from above.)
    #[test]
    fn two_line_stack_fits_vertically() {
        for ball in BALLS {
            // The docked card is the tightest case: smaller than the ball it came
            // from, with the same two lines to carry.
            let sphere = card_side_for(ball, 1.0);
            let num = sphere * NUM_CAP_RATIO;
            let label = (num * LABEL_OF_NUM).min(sphere * LABEL_CAP_RATIO);
            // Line boxes at ~1.3x the font size, plus the 4px gap of `gap_1`.
            let height = num * 1.3 + label * 1.3 + 4.0;
            let budget = text_budget_side(sphere) * std::f32::consts::SQRT_2;
            assert!(
                height <= budget + 0.01,
                "the label+count stack is {height}px tall in a {sphere}px ball (budget {budget})"
            );
        }
    }

    /// **The period label.** The ball shows the tab its number came from, so the
    /// digits can no longer silently change meaning with the dashboard tab. Two
    /// properties matter: the label must fit the ball's inscribed square like
    /// everything else, and it must never come out larger than the count it
    /// qualifies.
    ///
    /// Runs on the estimate path, which is what `fit_font_size` falls back to
    /// before the text system is ready — and which is only conservative if a CJK
    /// glyph counts as a full em, so gauge that too.
    #[test]
    fn period_label_fits_and_stays_under_the_count() {
        let tokens = format_int_grouped(999_999_999);
        for ball in BALLS {
            let sphere = card_side_for(ball, 1.0);
            let budget = text_budget_side(sphere);
            let cap = sphere * NUM_CAP_RATIO;
            let probe = FIT_PROBE_PX.min(cap);
            let fs = fit_size_from_probe(estimated_em(&tokens) * probe, probe, budget, cap);
            assert!(fs > 0.0, "the count vanished in a {sphere}px ball");
            let label_cap = (fs * LABEL_OF_NUM).min(sphere * LABEL_CAP_RATIO);
            for tab in TimeTab::ALL {
                let label = tab.label();
                // Near-square: an estimate below one em per glyph is unsafe.
                assert!(
                    estimated_em(label) >= label.chars().count() as f32 * 0.9,
                    "{label} is estimated too narrow to be safe"
                );
                let lp = FIT_PROBE_PX.min(label_cap);
                let lfs = fit_size_from_probe(estimated_em(label) * lp, lp, budget, label_cap);
                assert!(lfs > 0.0, "{label} vanished in a {sphere}px ball");
                assert!(
                    estimated_em(label) * lfs <= budget + 0.01,
                    "{label} would be {}px wide (budget {budget})",
                    estimated_em(label) * lfs
                );
                assert!(
                    lfs <= fs + 0.01,
                    "{label} ({lfs}px) outgrew the count ({fs}px) in a {sphere}px ball"
                );
            }
        }
    }

    /// **The "label melts into a glint" regression.** The overlay label is a
    /// centred column ~0.30 of the diameter wide (two CJK glyphs at the
    /// [`LABEL_CAP_RATIO`] cap), i.e. it spans 0.35..0.65 across. A specular
    /// sliding under it washes the translucent white glyphs into a blob — most
    /// visible at zero usage, where the ball is smallest. Pin every highlight
    /// to the left of that column.
    #[test]
    fn speculars_stay_clear_of_the_label_column() {
        // Worst case for how high the lifted label can reach: its *glyph* top
        // at the minimum ball, from the unshifted line top plus the leading,
        // minus the lift the icon clearance actually applies (see
        // `count_clears_the_refresh_icon_after_the_shift` for the exact model).
        let label_glyph_top = 0.125;
        for &(cx, cy, r, _) in SPECULARS.iter() {
            let label_left = 0.5 - LABEL_CAP_RATIO;
            let left_of_column = cx + r <= label_left + 1e-4;
            let above_the_band = cy + r <= label_glyph_top;
            assert!(
                left_of_column || above_the_band,
                "specular ({cx}, {cy}, r={r}) reaches into the label zone \
                 (column left edge {label_left}, band top {label_glyph_top})"
            );
            assert!(dot_is_inside_disc(cx, cy, r));
        }
    }

    /// **The "icon ring slices the 0" regression.** At zero usage the ball sits
    /// at [`MIN_SPHERE`] and the centred count used to run straight into the
    /// refresh icon pinned at [`ICON_CY_RATIO`]. After the shift the count's
    /// glyphs must stop a gap above the icon's top edge, at every plausible
    /// ball size and both text-size extremes (width-fitted long numbers and
    /// cap-sized short ones).
    #[test]
    fn count_clears_the_refresh_icon_after_the_shift() {
        for ball in BALLS {
            for morph in MORPHS {
                let sphere = card_side_for(ball, morph);
                for fs in [sphere * NUM_CAP_RATIO, 8.0] {
                    let label_fs = (fs * LABEL_OF_NUM).min(sphere * LABEL_CAP_RATIO);
                    let icon_d = 22.0_f32.min(sphere * 0.22);
                    let up = overlay_shift_up(sphere, fs, label_fs, icon_d);
                    let stack_h = LINE_HEIGHT_EM * (label_fs + fs) + STACK_GAP;
                    let glyph_bottom = sphere / 2.0 + stack_h / 2.0 - GLYPH_DESCENT_EM * fs - up;
                    let icon_top = sphere * ICON_CY_RATIO - icon_d / 2.0;
                    assert!(
                        glyph_bottom + STACK_GAP <= icon_top + 0.01,
                        "count glyph bottom {glyph_bottom} collides with icon top \
                         {icon_top} (card {sphere}, fs {fs})"
                    );
                    // The lift must not push the label through the top rim.
                    let label_top = sphere / 2.0 - stack_h / 2.0 - up;
                    assert!(label_top >= 0.0, "label lifted past the top rim");
                }
            }
        }
    }

    /// **The pinning must actually pin.** The window is fixed, so a transition
    /// is carried *entirely* by what the render draws; the only per-frame
    /// geometry left is the corner ratio, and it has to be a pure function of
    /// `morph` — monotone in `t` and clamped at both ends, so no frame of a
    /// transition can overshoot the shape.
    #[test]
    fn a_morph_draws_one_shape_per_progress() {
        let mut prev = corner_ratio_for(0.0);
        for step in 0..=8 {
            let t = step as f32 / 8.0;
            let ratio = corner_ratio_for(t);
            assert!(ratio <= prev + 1e-6, "corner ratio went backwards at {t}");
            assert!(
                (SQUARE_CORNER_RATIO..=1.0).contains(&ratio),
                "corner ratio {ratio} left the two forms' span at {t}"
            );
            prev = ratio;
        }
        // Clamped beyond both ends.
        assert_eq!(corner_ratio_for(-0.5), corner_ratio_for(0.0));
        assert_eq!(corner_ratio_for(1.5), corner_ratio_for(1.0));
    }

    /// **The re-snap must land the card that actually exists on the gap.** The
    /// drop-time snap estimates the card from the pre-morph fraction, and the
    /// landed card is `DOCK_SIDE_RATIO` of it — the estimate made the rest gap
    /// come out ~7 px wide of `DOCK_GAP`. The landing frame therefore
    /// publishes the settled fraction and the next frame re-snaps from it
    /// (`platform::snap_floating_window_to_edge`). This pins the closed loop:
    /// the published fraction describes the settled card, and snapping from it
    /// puts the card's outer edge exactly `DOCK_GAP` inside the work area on
    /// either side.
    #[test]
    fn the_resnap_lands_the_settled_card_on_the_gap() {
        // The gap the platform snaps to (`platform::windows::DOCK_GAP`).
        const GAP: f32 = 10.0;
        const WORK_LEFT: f32 = 0.0;
        const WORK_RIGHT: f32 = 1366.0;
        for ball in BALLS {
            let card = card_side_for(ball, 1.0);
            // The window is fixed, so the fraction the landing frame publishes
            // is the settled card over the fixed [`WINDOW_SIZE`].
            let want = WINDOW_SIZE;
            let frac = card / want;
            let snapped = want * frac;
            assert!(
                (snapped - card).abs() < 0.5,
                "the published fraction must describe the settled card"
            );
            // Window-left → card-left inset (idealised: frame origin 0).
            let margin = (want - snapped) / 2.0;
            let x = dock_window_x(DockEdge::Right, WORK_LEFT, WORK_RIGHT, snapped, margin, GAP);
            let outer = x as f32 + margin + snapped;
            assert!(
                (outer - (WORK_RIGHT - GAP)).abs() <= 0.51,
                "right re-snap lands the card's outer edge at {outer}, not {}",
                WORK_RIGHT - GAP
            );
            let x = dock_window_x(DockEdge::Left, WORK_LEFT, WORK_RIGHT, snapped, margin, GAP);
            let outer_left = x as f32 + margin;
            assert!(
                (outer_left - (WORK_LEFT + GAP)).abs() <= 0.51,
                "left re-snap lands the card's outer edge at {outer_left}, not {}",
                WORK_LEFT + GAP
            );
        }
    }

    /// **The transition must be a transition.** `morph_at` is what decides how
    /// many distinct frames a 240 ms morph shows: monotonic, easing-shaped, and
    /// landing exactly on the target. A step function here is indistinguishable
    /// from a dropped frame.
    #[test]
    fn the_morph_advances_monotonically_to_its_target() {
        let ms = Duration::from_millis;
        assert!((morph_at(0.0, 1.0, ms(0)) - 0.0).abs() < 1e-6);
        assert!((morph_at(0.0, 1.0, MORPH_DURATION) - 1.0).abs() < 1e-6);
        // Overshooting the duration must clamp, not run past the target.
        assert!((morph_at(0.0, 1.0, MORPH_DURATION * 4) - 1.0).abs() < 1e-6);
        // And the reverse direction.
        assert!((morph_at(1.0, 0.0, MORPH_DURATION) - 0.0).abs() < 1e-6);

        let steps = 240usize;
        let mut prev = morph_at(0.0, 1.0, ms(0));
        let mut distinct = 1;
        for i in 1..=steps {
            let v = morph_at(0.0, 1.0, ms(i as u64));
            assert!(
                v >= prev - 1e-6,
                "morph went backwards at {i}ms: {prev} -> {v}"
            );
            if (v - prev).abs() > 1e-4 {
                distinct += 1;
            }
            prev = v;
        }
        // A 60 Hz transition of MORPH_DURATION shows ~14 frames; anything close
        // to a handful means frames are being dropped, which is the regression
        // this guards.
        assert!(
            distinct >= 12,
            "only {distinct} distinct values across the morph — frames are being \
             dropped"
        );
        assert!(!morph_arrived(ms(0)));
        assert!(!morph_arrived(MORPH_DURATION - ms(1)));
        assert!(morph_arrived(MORPH_DURATION));
    }

    /// The two forms are the ends of one ramp: fully round at rest, the docked
    /// ratio once docked, and nothing beyond either end (a morph task that
    /// overruns its last frame must not overshoot the shape).
    #[test]
    fn the_morph_runs_between_two_roundnesses() {
        assert!((corner_ratio_for(0.0) - 1.0).abs() < 1e-6);
        assert!((corner_ratio_for(1.0) - SQUARE_CORNER_RATIO).abs() < 1e-6);
        assert!((corner_ratio_for(-1.0) - 1.0).abs() < 1e-6);
        assert!((corner_ratio_for(2.0) - SQUARE_CORNER_RATIO).abs() < 1e-6);
        assert!(
            SQUARE_CORNER_RATIO > 0.0 && SQUARE_CORNER_RATIO < 1.0,
            "the docked form has to be a rounded square: neither a tile nor a circle"
        );
    }

    /// **The docked square is not a circle.** The roundness has to change the
    /// silhouette enough to be seen, while still leaving a *rounded* square —
    /// and it has to do so at the *docked* side, which is the one actually
    /// drawn.
    #[test]
    fn the_docked_corner_is_much_tighter_than_a_circle() {
        for ball in [MIN_SPHERE, MAX_SPHERE] {
            let side = card_side_for(ball, 1.0);
            let circle_radius = ball * corner_ratio_for(0.0) / 2.0;
            let square_radius = side * corner_ratio_for(1.0) / 2.0;
            assert!(
                (circle_radius - ball / 2.0).abs() < 0.01,
                "the resting form must be fully round"
            );
            assert!(
                square_radius < circle_radius * 0.5,
                "the docked corner ({square_radius}) is barely tighter than the \
                 circle's ({circle_radius}) — the form would not read as a square"
            );
            assert!(
                square_radius > 1.0,
                "the docked square needs *some* rounding"
            );
        }
    }

    /// **The docked form is smaller than the ball it came from.** A filled square
    /// reads heavier than a circle of the same side, so docking has to take a
    /// real bite out of the size — while staying big enough for the overlay.
    #[test]
    fn the_docked_card_is_smaller_than_the_ball() {
        assert!(
            (0.45..=0.8).contains(&DOCK_SIDE_RATIO),
            "a docked square of {DOCK_SIDE_RATIO} of the ball is either no smaller \
             at all or too small to carry the count"
        );
        for ball in [MIN_SPHERE, 140.0, MAX_SPHERE, MAX_SPHERE * HOVER_SCALE] {
            let docked = card_side_for(ball, 1.0);
            assert!((docked - ball * DOCK_SIDE_RATIO).abs() < 0.01);
            assert!(docked < ball, "docking must shrink the card, not grow it");
            // One continuous ramp: the free form is exactly the ball, and the
            // side falls off monotonically from there to the docked one.
            assert!((card_side_for(ball, 0.0) - ball).abs() < 0.01);
            let mut prev = f32::INFINITY;
            for step in 0..=8 {
                let side = card_side_for(ball, step as f32 / 8.0);
                assert!(side <= prev + 0.001, "the side must only ever shrink");
                prev = side;
            }
            // Still room for a legible count: the inscribed square of the docked
            // card has to hold at least a "999万"-ish string.
            assert!(
                text_budget_side(docked) >= 25.0,
                "the docked card leaves only {}px of text budget",
                text_budget_side(docked)
            );
        }
    }

    /// **The window must not clip the card.** The card sits centred in its
    /// window at the frame's own corner ratio, so the window side has to stay
    /// outside the card, and outside the glow, which reaches past it, at every
    /// step of the morph. The corner radius growing with the side is what makes
    /// that hold at any size.
    #[test]
    fn the_window_contains_the_card_and_its_glow() {
        for tokens in USAGES {
            for hovered in [false, true] {
                let ball = sphere_diameter_for(tokens, hovered);
                for step in 0..=4 {
                    let t = step as f32 / 4.0;
                    let ratio = corner_ratio_for(t);
                    let card = card_side_for(ball, t);
                    let win = WINDOW_SIZE;
                    let needed = rounded_reach(card, ratio) + GLOW_TAIL_SIGMA * glow_blur(card);
                    let available = rounded_reach(win, ratio);
                    assert!(
                        needed <= available + 0.01,
                        "the window clips the card mid-morph: card {card}, window \
                         {win}, ratio {ratio}, needs {needed}px, has \
                         {available}px (tokens {tokens}, t={t})"
                    );
                    assert!(win > 0.0 && win <= WINDOW_SIZE);
                }
            }
        }
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
