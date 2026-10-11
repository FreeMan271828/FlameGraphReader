//! Viewport culling and level-of-detail geometry generation.
//!
//! This module is deliberately free of any GUI dependency: it turns a scene plus
//! a pan/zoom transform into a list of screen-space quads. That makes the hot
//! path unit-testable and benchmarkable without opening a window, and keeps the
//! renderer a dumb consumer.
//!
//! The cost of one call is O(visible rows * log n + emitted quads): frames are
//! never touched unless they intersect the viewport, and sub-pixel frames are
//! coalesced into single quads so the vertex count can never explode.

use crate::scene::Scene;

/// Total quads the LOD may emit in one frame. Above roughly one quad per screen
/// pixel per row the extra geometry is invisible anyway.
pub const DEFAULT_BUDGET: usize = 120_000;

/// Frames narrower than this (in device pixels) are merged into a run.
const MIN_RUN_PX: f32 = 0.34;

/// Rows thinner than this are drawn every k-th row.
const MIN_ROW_PX: f32 = 1.2;

/// Never merge a row down to fewer quads than this.
const MIN_PER_ROW: usize = 24;

/// A label is only worth laying out when the rect can actually show it.
const LABEL_MIN_W: f32 = 26.0;
const LABEL_MIN_H: f32 = 9.0;
/// Labels laid out per frame, spread over the visible rows.
pub const LABEL_BUDGET: usize = 400;

/// A row narrower than this is unreadable, so fitting is clamped to keep rows
/// legible even for extremely wide graphs.
pub const MIN_FIT_ROW_PX: f32 = 3.0;

/// Row separation, in screen pixels. Rows thinner than the first constant are
/// left alone (insetting them would erase them); thicker ones give up at most a
/// pixel on each side.
const BAND_GAP_MIN_BAND: f32 = 4.0;
const BAND_GAP_RATIO: f32 = 0.15;
const BAND_GAP_MIN: f32 = 0.6;
const BAND_GAP_MAX: f32 = 1.0;

/// Column separation, in screen pixels per side. Generators tile a row exactly,
/// and it is the rounded corners (rx="2") in the original document that separate
/// one frame from the next. The viewer draws square rectangles because that is
/// what keeps the vertex count bounded, so without a seam two adjacent frames of
/// the same colour merge into one uninterrupted bar.
const COLUMN_SEAM_MIN_W: f32 = 3.0;
const COLUMN_SEAM_RATIO: f32 = 0.05;
const COLUMN_SEAM_MIN: f32 = 0.3;
const COLUMN_SEAM_MAX: f32 = 0.5;

/// Screen-space inset applied to each side of a frame, so neighbouring frames in
/// a row stay visually separate at every zoom level.
#[inline]
pub fn column_inset(screen_w: f32) -> f32 {
    if screen_w > COLUMN_SEAM_MIN_W {
        (screen_w * COLUMN_SEAM_RATIO).clamp(COLUMN_SEAM_MIN, COLUMN_SEAM_MAX)
    } else {
        0.0
    }
}

/// A rectangular region of the graph: everything outside it is not drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub x0: f64,
    pub x1: f64,
    pub y0: f32,
    pub y1: f32,
}

impl Bounds {
    pub fn scene(scene: &Scene) -> Bounds {
        Bounds {
            x0: scene.min_x,
            x1: scene.max_x,
            y0: scene.min_y,
            y1: scene.max_y,
        }
    }

    #[inline]
    pub fn width(&self) -> f64 {
        self.x1 - self.x0
    }

    #[inline]
    pub fn height(&self) -> f32 {
        self.y1 - self.y0
    }
}

/// A focused subtree: only this frame and its descendants are drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Focus {
    pub frame: u32,
    pub x0: f64,
    pub x1: f64,
    pub y0: f32,
    pub level: usize,
}

impl Focus {
    pub fn of(scene: &Scene, frame: u32) -> Focus {
        let f = scene.frames[frame as usize];
        Focus {
            frame,
            x0: f.x,
            x1: f.x + f.w,
            y0: f.y,
            level: scene.level_of(frame),
        }
    }

    /// The region a click should frame: the calls below the focused frame *and*
    /// the whole path from the root down to it.
    ///
    /// The vertical extent ends at the subtree's real depth, not at the graph's
    /// global depth. Reserving rows the subtree never reaches pushed it into a
    /// corner of an otherwise empty viewport, and starting at the focused row
    /// meant a click made the context that explains it vanish.
    pub fn bounds(&self, scene: &Scene) -> Bounds {
        Bounds {
            x0: self.x0,
            x1: self.x1,
            y0: scene.min_y,
            y1: scene.subtree_max_y(self.frame),
        }
    }
}

/// Whether a frame is part of the focused view.
///
/// After focusing, the only frames on screen are the path from the root down to
/// the focused frame (faded) and the subtree below it (full strength). Hit
/// testing has to agree with that exactly, otherwise the empty space around a
/// focused subtree still lights up frames that were culled away - the sibling
/// rows above the focus used to be hoverable because "above the focus row" was
/// mistaken for "on the path".
pub fn is_focused_visible(scene: &Scene, focus: Option<Focus>, path: &[u32], frame: u32) -> bool {
    let Some(f) = focus else {
        return true;
    };
    if frame == f.frame || path.contains(&frame) {
        return true;
    }
    let hf = scene.frames[frame as usize];
    // Inside the subtree: at or below the focus row, overlapping its x-interval.
    // A subtree occupies exactly that interval on every row, so this is the same
    // test the bright pass uses.
    hf.y >= f.y0 && hf.x < f.x1 && hf.x + hf.w > f.x0
}

/// Pan/zoom transform. Graph x is f64 and graph y is f32; screen space is f32,
/// because pixels never need more.
#[derive(Clone, Copy, Debug)]
pub struct Viewport {
    pub pan_x: f64,
    pub pan_y: f64,
    pub scale: f64,
    /// Draw the rows in reverse, so the widest row (the root) ends up at the
    /// bottom, the way browsers and flamegraph.pl present a profile.
    pub flip_y: bool,
}

impl Default for Viewport {
    fn default() -> Self {
        Viewport {
            pan_x: 0.0,
            pan_y: 0.0,
            scale: 1.0,
            flip_y: false,
        }
    }
}

impl Viewport {
    /// Graph x -> screen x.
    #[inline]
    pub fn sx(&self, gx: f64) -> f32 {
        ((gx + self.pan_x) * self.scale) as f32
    }

    #[inline]
    pub fn sy(&self, gy: f32) -> f32 {
        let y = if self.flip_y { -(gy as f64) } else { gy as f64 };
        ((y + self.pan_y) * self.scale) as f32
    }

    #[inline]
    pub fn gx(&self, sx: f32) -> f64 {
        sx as f64 / self.scale - self.pan_x
    }

    #[inline]
    pub fn gy(&self, sy: f32) -> f64 {
        let v = sy as f64 / self.scale - self.pan_y;
        if self.flip_y {
            -v
        } else {
            v
        }
    }

    /// Screen-space inset to apply to a row band so neighbouring rows stay
    /// visually separate.
    ///
    /// Documents that use the row pitch as the frame height tile their rows
    /// exactly, and even a genuine one-unit gap can be far below a pixel when
    /// zoomed out. Without an enforced separation the anti-aliased edges of
    /// adjacent rows blend into each other and the whole graph smears.
    #[inline]
    pub fn band_inset(&self, graph_h: f32) -> f32 {
        let band = (graph_h as f64 * self.scale).abs() as f32;
        if band > BAND_GAP_MIN_BAND {
            (band * BAND_GAP_RATIO).clamp(BAND_GAP_MIN, BAND_GAP_MAX)
        } else {
            0.0
        }
    }

    /// Zoom around a fixed screen point, so the graph point under the cursor
    /// stays put. This is what makes wheel-zoom feel immediate.
    pub fn zoom_at(&mut self, screen_x: f32, screen_y: f32, factor: f64) {
        let (gx, gy) = (self.gx(screen_x), self.gy(screen_y));
        self.scale = (self.scale * factor).clamp(1e-12, 1e12);
        let dir = if self.flip_y { -1.0 } else { 1.0 };
        self.pan_x = screen_x as f64 / self.scale - gx;
        self.pan_y = screen_y as f64 / self.scale - dir * gy;
    }
}

/// Rect in screen space.
#[derive(Clone, Copy, Debug)]
pub struct ScreenRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl ScreenRect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        ScreenRect { x, y, w, h }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quad {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    pub color: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct LabelReq {
    pub frame: u32,
    pub x: f32,
    pub center_y: f32,
    /// True for the labels on the faded path above a focused frame: they are
    /// drawn subdued rather than dropped.
    pub faded: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stats {
    pub rows_visited: u32,
    pub rows_drawn: u32,
    pub row_step: u32,
    pub frames_considered: u64,
    pub frames_drawn: u64,
    pub frames_merged: u64,
    pub quads: u32,
    pub labels: u32,
    /// Quads dropped because the budget was hit (should stay 0).
    pub dropped: u32,
}

/// Compute the transform that fits the given bounds into a viewport.
///
/// Very wide profiles (many millions of frames) would otherwise open as a one
/// pixel tall sliver: fitting the width there makes every row sub-pixel. When
/// that happens the view is zoomed in just enough for the rows to be legible,
/// anchored at the edge the graph is read from, which is where the root is.
pub fn fit(scene: &Scene, view: ScreenRect, margin: f32, bounds: Bounds, flip_y: bool) -> Viewport {
    let w = bounds.width().max(1e-9);
    let h = (bounds.height() as f64).max(1e-9);
    let avail_w = ((view.w - margin * 2.0) as f64).max(1.0);
    let avail_h = ((view.h - margin * 2.0) as f64).max(1.0);
    let scale = (avail_w / w).min(avail_h / h).max(1e-12);

    // The viewport rect is in absolute screen coordinates and normally does not
    // start at the screen origin (there are toolbars above it), so the graph
    // origin has to be placed relative to the rect itself.
    let origin_x = view.x as f64 + margin as f64;
    let origin_y = view.y as f64 + margin as f64;
    let dir = if flip_y { -1.0 } else { 1.0 };
    let y_first = if flip_y { bounds.y1 } else { bounds.y0 };

    let mut vp = Viewport {
        pan_x: origin_x / scale - bounds.x0,
        pan_y: origin_y / scale - dir * y_first as f64,
        scale,
        flip_y,
    };

    let row_h = scene
        .levels
        .first()
        .map(|l| l.h as f64)
        .unwrap_or(16.0)
        .max(1e-9);
    let legible = MIN_FIT_ROW_PX as f64 / row_h;
    if vp.scale < legible {
        // Re-place rather than zoom about a point: the fitted graph can be only
        // a few pixels tall here, so there is no on-graph point that would leave
        // the rows in view. Instead pin the root row to the edge the rows grow
        // away from (top when unflipped, bottom when flipped) and the graph's
        // left edge to the left margin.
        vp.scale = legible;
        let root_y = if flip_y {
            view.y as f64 + view.h as f64 - margin as f64
        } else {
            origin_y
        };
        vp.pan_x = origin_x / vp.scale - bounds.x0;
        vp.pan_y = root_y / vp.scale - dir * bounds.y0 as f64;
    }
    vp
}

/// Zoom so that a single frame fills the viewport width.
pub fn focus_frame(scene: &Scene, frame: u32, view: ScreenRect, flip_y: bool) -> Viewport {
    let f = scene.frames[frame as usize];
    // Filling the viewport width with a narrow frame would push every call below
    // it off screen, which defeats the point of zooming to it.
    let row_h = scene
        .levels
        .first()
        .map(|l| l.h as f64)
        .unwrap_or(16.0)
        .max(1e-9);
    let context_cap = ((view.h as f64) / (MIN_VISIBLE_ROWS * row_h)).max(1e-9);
    let scale = (((view.w - 40.0).max(16.0) as f64) / f.w.max(1e-9))
        .clamp(1e-12, 1e12)
        .min(context_cap);
    let dir = if flip_y { -1.0 } else { 1.0 };
    let y_target = view.y as f64 + view.h as f64 * 0.35;
    Viewport {
        // Put the frame's left edge 20px in, and its row near the top.
        pan_x: (view.x as f64 + 20.0) / scale - f.x,
        pan_y: y_target / scale - dir * f.y as f64,
        scale,
        flip_y,
    }
}

/// At least this many rows stay on screen when zooming to a single frame, so
/// the calls stacked underneath it do not vanish.
const MIN_VISIBLE_ROWS: f64 = 6.0;

/// Everything the renderer needs for one frame.
#[derive(Default)]
pub struct Geometry {
    /// Full strength frames: the focused subtree, or the whole graph.
    pub quads: Vec<Quad>,
    /// The path from the root down to a focused frame, drawn faded. Only the
    /// frames the subtree actually depends on are kept: drawing every whole row
    /// above produced a large dark slab that buried the subtree.
    pub faded: Vec<Quad>,
    pub labels: Vec<LabelReq>,
    pub stats: Stats,
}

impl Geometry {
    pub fn clear(&mut self) {
        self.quads.clear();
        self.faded.clear();
        self.labels.clear();
        self.stats = Stats::default();
    }

    /// Total quads emitted for this frame, across both passes.
    pub fn len(&self) -> usize {
        self.quads.len() + self.faded.len()
    }
}

struct RowBudget {
    min_px: f32,
    label_cap: usize,
}

/// Build the geometry for one frame of interaction.
///
/// With no focus this is a single viewport pass. With a focused subtree there
/// are two passes:
///
/// * the path from the root down to the focused frame, drawn faded, so the user
///   keeps their bearings without the context competing with the answer. Only
///   the frames the subtree actually depends on are kept - drawing every whole
///   row above produced a large dark slab that buried the subtree;
/// * the focused subtree itself, at full strength, culled to the one contiguous
///   x-interval the subtree occupies on each row.
///
/// The first pass is O(depth), the second is bounded by the viewport rather than
/// by the size of the graph.
pub fn build(
    scene: &Scene,
    vp: &Viewport,
    view: ScreenRect,
    budget: usize,
    name_colors: Option<&[u32]>,
    focus: Option<Focus>,
    out: &mut Geometry,
) {
    out.clear();
    if scene.is_empty() || vp.scale <= 0.0 || !vp.scale.is_finite() {
        return;
    }

    let mut gx0 = vp.gx(view.x);
    let mut gx1 = vp.gx(view.x + view.w);
    if gx1 < gx0 {
        std::mem::swap(&mut gx0, &mut gx1);
    }
    let gy_a = vp.gy(view.y) as f32;
    let gy_b = vp.gy(view.y + view.h) as f32;
    let (gy_lo, gy_hi) = if gy_a <= gy_b {
        (gy_a, gy_b)
    } else {
        (gy_b, gy_a)
    };

    let visible = scene.levels_in_range(gy_lo, gy_hi);
    if visible.is_empty() {
        return;
    }

    let (bright_lo, cx0, cx1) = match focus {
        Some(f) => {
            emit_ancestors(scene, vp, view, f.frame, name_colors, out);
            (visible.start.max(f.level), gx0.max(f.x0), gx1.min(f.x1))
        }
        None => (visible.start, gx0, gx1),
    };

    if cx1 > cx0 && bright_lo < visible.end {
        let bright_budget = budget.saturating_sub(out.faded.len()).max(MIN_PER_ROW * 2);
        let limit = out.quads.len() + bright_budget;
        emit_levels(
            scene,
            vp,
            view,
            bright_lo..visible.end,
            cx0,
            cx1,
            limit,
            name_colors,
            &mut out.quads,
            Some(&mut out.labels),
            &mut out.stats,
        );
    }

    out.stats.quads = out.len() as u32;
    out.stats.labels = out.labels.len() as u32;
}

/// Emit the faded path from the root down to (but excluding) the focused frame.
///
/// O(depth), not O(rows): a call tree is at most a few dozen frames deep, so the
/// context costs a handful of quads.
fn emit_ancestors(
    scene: &Scene,
    vp: &Viewport,
    view: ScreenRect,
    frame: u32,
    name_colors: Option<&[u32]>,
    out: &mut Geometry,
) {
    // Keep the labels short enough that a very deep path cannot flood the layout.
    const MAX_ANCESTOR_LABELS: usize = 40;
    let mut labels = 0usize;

    for a in scene.ancestry(frame) {
        if a == frame {
            // The focused frame itself is drawn at full strength below.
            continue;
        }
        let f = scene.frames[a as usize];
        let sx0 = vp.sx(f.x);
        let sx1 = vp.sx(f.x + f.w);
        let sy0 = vp.sy(f.y);
        let sy1 = vp.sy(f.y + f.h);
        let (qx0, qx1) = (sx0.min(sx1), sx0.max(sx1));
        let (qy0, qy1) = (sy0.min(sy1), sy0.max(sy1));
        if qx1 < view.x || qx0 > view.x + view.w || qy1 < view.y || qy0 > view.y + view.h {
            continue;
        }
        let color = match name_colors {
            Some(t) => t.get(f.name as usize).copied().unwrap_or(f.color),
            None => f.color,
        };
        out.faded.push(Quad {
            x0: qx0,
            y0: qy0,
            x1: qx1,
            y1: qy1,
            color,
        });
        out.stats.frames_considered += 1;

        // The path keeps its names: without them the faded frames cannot be
        // identified and the context is useless.
        if labels < MAX_ANCESTOR_LABELS && (qx1 - qx0) >= LABEL_MIN_W && (qy1 - qy0) >= LABEL_MIN_H
        {
            labels += 1;
            out.labels.push(LabelReq {
                frame: a,
                // An ancestor is usually wider than the focused subtree, so its
                // own left edge can be off screen. Anchor the name at the
                // visible left edge instead of losing it to the clip.
                x: qx0.max(view.x) + 2.0,
                center_y: (qy0 + qy1) * 0.5,
                faded: true,
            });
        }
    }
}

/// Emit one viewport pass over a range of rows, honouring the vertical LOD and
/// the absolute quad limit.
#[allow(clippy::too_many_arguments)]
fn emit_levels(
    scene: &Scene,
    vp: &Viewport,
    view: ScreenRect,
    levels: std::ops::Range<usize>,
    gx0: f64,
    gx1: f64,
    limit: usize,
    name_colors: Option<&[u32]>,
    quads: &mut Vec<Quad>,
    mut labels: Option<&mut Vec<LabelReq>>,
    stats: &mut Stats,
) {
    stats.rows_visited += levels.len() as u32;
    if levels.is_empty() || gx1 <= gx0 {
        return;
    }

    // Vertical LOD: when rows are thinner than a pixel, step over them.
    let row_h = scene.levels[levels.start].h.max(1e-6);
    let row_px = row_h as f64 * vp.scale;
    let step = if row_px >= MIN_ROW_PX as f64 {
        1usize
    } else {
        ((MIN_ROW_PX as f64) / row_px).ceil().max(1.0) as usize
    };
    stats.row_step = stats.row_step.max(step as u32);

    let drawn_rows = levels.len().div_ceil(step).max(1);
    // Merged stretches can add up to one segment per few pixels on top of the
    // individually drawn frames, so reserve room for them before deciding how
    // much of the budget the drawn frames may use.
    let merged_reserve = (view.w / MERGED_SEGMENT_PX).ceil() as usize + 1;
    // Guard against a pathological zoom-out where every row is sub-pixel.
    let per_row = (limit.saturating_sub(quads.len()) / drawn_rows)
        .saturating_sub(merged_reserve)
        .max(MIN_PER_ROW);
    let label_cap = (LABEL_BUDGET / drawn_rows).max(4);

    let mut li = levels.start;
    while li < levels.end {
        let level = &scene.levels[li];
        let sy0 = vp.sy(level.y);
        let sy1 = vp.sy(level.y + level.h);
        let (mut top, mut bottom) = if sy0 <= sy1 { (sy0, sy1) } else { (sy1, sy0) };
        if bottom < view.y - 1.0 || top > view.y + view.h + 1.0 {
            li += step;
            continue;
        }
        stats.rows_drawn += 1;

        // Give every row a visible gap, so a zoomed-out graph reads as rows
        // rather than one smeared block.
        let inset = vp.band_inset(level.h);
        if inset > 0.0 && bottom - top > 2.0 * inset + 1.0 {
            top += inset;
            bottom -= inset;
        }

        let range = scene.frames_in_level(li, gx0, gx1);
        let count = range.len();
        stats.frames_considered += count as u64;

        let rb = RowBudget {
            min_px: if count > per_row {
                (view.w / per_row as f32).max(MIN_RUN_PX)
            } else {
                MIN_RUN_PX
            },
            label_cap,
        };

        emit_row(
            scene,
            vp,
            range,
            top,
            bottom,
            &rb,
            name_colors,
            limit,
            quads,
            labels.as_deref_mut(),
            stats,
        );
        li += step;
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_row(
    scene: &Scene,
    vp: &Viewport,
    range: std::ops::Range<usize>,
    sy0: f32,
    sy1: f32,
    rb: &RowBudget,
    name_colors: Option<&[u32]>,
    limit: usize,
    quads: &mut Vec<Quad>,
    mut labels: Option<&mut Vec<LabelReq>>,
    stats: &mut Stats,
) {
    let frames = &scene.frames;
    let mut run: Option<Run> = None;
    let mut row_labels = 0usize;

    for idx in range {
        let f = &frames[idx];
        let sx0 = vp.sx(f.x);
        let sx1 = vp.sx(f.x + f.w);
        let wpx = sx1 - sx0;
        let color = match name_colors {
            Some(t) => t.get(f.name as usize).copied().unwrap_or(f.color),
            None => f.color,
        };

        if wpx < rb.min_px {
            // A merge must not erase the structure of a dense row. Dropping a
            // whole stretch into one quad - coloured by whichever frame happened
            // to be widest - turned hundreds of frames into what looked like a
            // single flat node, and zooming in only "fixed" it because the frames
            // stopped needing to be merged. So a colour change starts a new
            // segment, but only once the current one is a few pixels wide, which
            // caps the extra quads at roughly one per few pixels.
            let start_new = run
                .as_ref()
                .is_some_and(|r| r.first_color != color && r.x1 - r.x0 >= MERGED_SEGMENT_PX);
            if start_new {
                flush_run(&mut run, sy0, sy1, limit, quads, stats);
            }
            match &mut run {
                Some(r) => {
                    r.x1 = sx1;
                    if wpx > r.widest {
                        r.color = color;
                        r.widest = wpx;
                    }
                }
                None => {
                    run = Some(Run {
                        x0: sx0,
                        x1: sx1,
                        first_color: color,
                        color,
                        widest: wpx,
                    });
                }
            }
            stats.frames_merged += 1;
            continue;
        }

        let (qx0, qx1) = seam(sx0, sx1);

        flush_run(&mut run, sy0, sy1, limit, quads, stats);
        if quads.len() < limit {
            quads.push(Quad {
                x0: qx0,
                y0: sy0,
                x1: qx1,
                y1: sy1,
                color,
            });
            stats.frames_drawn += 1;
        } else {
            stats.dropped += 1;
            continue;
        }

        if let Some(labels) = labels.as_deref_mut() {
            if wpx >= LABEL_MIN_W && (sy1 - sy0) >= LABEL_MIN_H && row_labels < rb.label_cap {
                row_labels += 1;
                labels.push(LabelReq {
                    frame: idx as u32,
                    x: sx0 + 2.0,
                    center_y: (sy0 + sy1) * 0.5,
                    faded: false,
                });
            }
        }
    }

    flush_run(&mut run, sy0, sy1, limit, quads, stats);
}

/// A stretch of frames too narrow to draw individually.
///
/// The first field is what the segment started as, the second what it currently
/// shows, and the last the widest frame seen so far.
struct Run {
    x0: f32,
    x1: f32,
    first_color: u32,
    color: u32,
    widest: f32,
}

/// A merged segment is allowed to grow this wide before a colour change starts a
/// new one.
const MERGED_SEGMENT_PX: f32 = 3.0;

#[inline]
fn flush_run(
    run: &mut Option<Run>,
    sy0: f32,
    sy1: f32,
    limit: usize,
    quads: &mut Vec<Quad>,
    stats: &mut Stats,
) {
    if let Some(r) = run.take() {
        let (x0, x1) = seam(r.x0, r.x1);
        if quads.len() < limit {
            quads.push(Quad {
                x0,
                y0: sy0,
                x1,
                y1: sy1,
                color: r.color,
            });
        } else {
            stats.dropped += 1;
        }
    }
}

/// Shrink a horizontal span by the column seam, leaving the centre put.
#[inline]
fn seam(x0: f32, x1: f32) -> (f32, f32) {
    let inset = column_inset(x1 - x0);
    if inset > 0.0 && x1 - x0 > 2.0 * inset + 0.5 {
        (x0 + inset, x1 - inset)
    } else {
        (x0, x1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::Frame;

    fn synthetic(frames: usize, per_row: usize) -> Scene {
        synthetic_wide(frames, per_row, 1000.0)
    }

    /// A graph whose coordinate space is far wider than it is tall, which is
    /// what large profiles look like and what triggers the legibility clamp.
    fn synthetic_wide(frames: usize, per_row: usize, width: f64) -> Scene {
        let mut v = Vec::with_capacity(frames);
        let mut made = 0usize;
        let mut row = 0usize;
        while made < frames {
            let w = width / per_row as f64;
            for i in 0..per_row {
                if made >= frames {
                    break;
                }
                v.push(Frame {
                    x: i as f64 * w,
                    w,
                    y: row as f32 * 15.0,
                    h: 15.0,
                    color: 0x80_40_20,
                    name: (i % 8) as u32,
                });
                made += 1;
            }
            row += 1;
        }
        let names = (0..8).map(|i| format!("fn_{i}")).collect();
        Scene::build(v, names, "t".into(), "mem".into(), 0, 0.0, [0.0, 0.0])
    }

    fn geom(scene: &Scene, vp: &Viewport, view: ScreenRect, focus: Option<Focus>) -> Geometry {
        let mut g = Geometry::default();
        build(scene, vp, view, DEFAULT_BUDGET, None, focus, &mut g);
        g
    }

    /// The row of a frame, in graph units.
    fn row_of(scene: &Scene, frame: u32) -> f32 {
        scene.frames[frame as usize].y
    }

    /// A tree where one branch stops early while the graph keeps going: the case
    /// where using the graph's global depth for the fit corners the content.
    fn uneven_tree() -> Scene {
        fn row(v: &mut Vec<Frame>, x: f64, w: f64, r: f32) {
            v.push(Frame {
                x,
                w,
                y: r * 15.0,
                h: 15.0,
                color: 0x80_40_20,
                name: 0,
            });
        }
        let mut v = Vec::new();
        row(&mut v, 0.0, 500.0, 0.0); // A
        row(&mut v, 500.0, 500.0, 0.0); // B
        row(&mut v, 0.0, 10.0, 1.0); // A1, narrow and about to be focused
        row(&mut v, 10.0, 490.0, 1.0); // A2
        row(&mut v, 500.0, 500.0, 1.0); // B1
        for r in 2..6 {
            row(&mut v, 500.0, 500.0, r as f32); // only B keeps going deeper
        }
        Scene::build(
            v,
            vec!["fn".into()],
            "t".into(),
            "mem".into(),
            0,
            0.0,
            [0.0, 0.0],
        )
    }

    #[test]
    fn a_merged_run_keeps_its_colour_structure() {
        // 4000 sub-pixel frames in one row, alternating colours. Collapsing them
        // into a single quad made a dense row look like one flat node.
        let mut v = Vec::with_capacity(4000);
        for i in 0..4000 {
            v.push(Frame {
                x: i as f64 * 0.25,
                w: 0.25,
                y: 0.0,
                h: 15.0,
                color: if i % 2 == 0 { 0x00_ff_00 } else { 0xff_00_00 },
                name: 0,
            });
        }
        let scene = Scene::build(
            v,
            vec!["fn".into()],
            "t".into(),
            "mem".into(),
            0,
            0.0,
            [0.0, 0.0],
        );
        let view = ScreenRect::new(0.0, 0.0, 1200.0, 800.0);
        let vp = fit(&scene, view, 14.0, Bounds::scene(&scene), false);
        let g = geom(&scene, &vp, view, None);

        assert!(g.stats.frames_merged > 0, "the fixture should need merging");
        assert!(
            g.quads.len() > 1,
            "a merged run collapsed into {} flat bar(s)",
            g.quads.len()
        );
        // The split is bounded: roughly one quad per few pixels, not one per
        // frame.
        assert!(
            g.quads.len() < 600,
            "the split emitted {} quads",
            g.quads.len()
        );
        assert_eq!(g.stats.dropped, 0, "the split blew the quad budget");
    }

    #[test]
    fn adjacent_frames_in_a_row_keep_a_seam() {
        // The fixture tiles each row exactly: consecutive frames share an edge,
        // and same-coloured neighbours used to merge into one unbroken bar.
        let scene = synthetic(200, 20);
        let view = ScreenRect::new(0.0, 0.0, 1200.0, 800.0);
        let vp = fit(&scene, view, 14.0, Bounds::scene(&scene), false);
        let g = geom(&scene, &vp, view, None);

        let first_row_y = g.quads.first().map(|q| q.y0).expect("geometry");
        let mut row: Vec<(f32, f32)> = g
            .quads
            .iter()
            .filter(|q| (q.y0 - first_row_y).abs() < 0.01)
            .map(|q| (q.x0, q.x1))
            .collect();
        row.sort_by(|a, b| a.0.total_cmp(&b.0));
        assert!(
            row.len() >= 3,
            "expected a populated row, got {}",
            row.len()
        );
        for w in row.windows(2) {
            assert!(
                w[1].0 > w[0].1,
                "frames {:?} and {:?} touch: they merge into one bar",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn adjacent_rows_keep_a_visible_gap() {
        // The fixture tiles its rows exactly (frame height == row pitch), which
        // is what generators that write height == pitch produce. Zoomed out,
        // those rows used to blend into a single smeared block.
        let scene = synthetic(200, 20);
        let view = ScreenRect::new(0.0, 0.0, 1200.0, 800.0);
        let vp = fit(&scene, view, 14.0, Bounds::scene(&scene), false);
        let g = geom(&scene, &vp, view, None);

        let mut bands: Vec<(f32, f32)> = g.quads.iter().map(|q| (q.y0, q.y1)).collect();
        bands.sort_by(|a, b| a.0.total_cmp(&b.0));
        bands.dedup_by(|a, b| (a.0 - b.0).abs() < 0.01);
        assert!(
            bands.len() >= 3,
            "expected several rows, got {}",
            bands.len()
        );
        for w in bands.windows(2) {
            let gap = w[1].0 - w[0].1;
            assert!(
                gap > 0.0,
                "rows {:?} and {:?} touch: the graph will smear",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn hit_testing_agrees_with_what_is_drawn() {
        let scene = uneven_tree();
        let target = scene
            .frames
            .iter()
            .position(|f| f.y == 15.0 && f.x == 0.0)
            .expect("A1 exists") as u32;
        let focus = Focus::of(&scene, target);
        let path = scene.ancestry(target);

        // The focused frame and every frame on the path above it are hittable.
        assert!(is_focused_visible(&scene, Some(focus), &path, target));
        for a in &path {
            assert!(
                is_focused_visible(&scene, Some(focus), &path, *a),
                "path frame {a} is not hittable"
            );
        }

        // A sibling sharing a row with an ancestor was culled away and never
        // drawn. Hovering the empty space around the subtree must not find it -
        // this is exactly the bug the predicate exists for.
        let sibling = scene
            .frames
            .iter()
            .position(|f| f.y == 0.0 && f.x == 500.0)
            .expect("B exists") as u32;
        assert!(
            !is_focused_visible(&scene, Some(focus), &path, sibling),
            "a culled away sibling is still hittable"
        );

        // Nor is anything from the branch that keeps going deeper.
        let elsewhere = scene
            .frames
            .iter()
            .position(|f| f.y == 60.0)
            .expect("deep row exists") as u32;
        assert!(!is_focused_visible(&scene, Some(focus), &path, elsewhere));

        // Without a focus every frame is hittable.
        assert!(is_focused_visible(&scene, None, &[], sibling));
    }

    #[test]
    fn focus_frames_the_subtree_and_its_path_together() {
        let scene = uneven_tree();
        let view = ScreenRect::new(0.0, 0.0, 1200.0, 800.0);

        let target = scene
            .frames
            .iter()
            .position(|f| f.y == 15.0 && f.x == 0.0)
            .expect("A1 exists") as u32;
        let focus = Focus::of(&scene, target);
        assert_eq!(focus.level, 1);
        // A1 has no calls below it, while the graph runs four more rows deep.
        assert_eq!(scene.subtree_max_y(target), 30.0);

        let vp = fit(&scene, view, 14.0, focus.bounds(&scene), false);
        let g = geom(&scene, &vp, view, Some(focus));

        // Everything drawn - the faded path and the bright subtree - has to fill
        // the viewport instead of huddling in a corner.
        let ys: Vec<f32> = g
            .quads
            .iter()
            .chain(g.faded.iter())
            .flat_map(|q| [q.y0, q.y1])
            .collect();
        assert!(!ys.is_empty());
        let top = ys.iter().copied().fold(f32::MAX, f32::min);
        let bottom = ys.iter().copied().fold(f32::MIN, f32::max);
        let span = bottom - top;
        assert!(
            span >= view.h * 0.8,
            "the focused view used {span:.0}px of {:.0}px: the content is cornered",
            view.h
        );

        // Both halves are framed: the path above and the subtree itself.
        assert!(
            !g.faded.is_empty(),
            "the path above the focus was not framed"
        );
        assert!(g.stats.rows_drawn >= 1, "the subtree was not framed");
    }

    #[test]
    fn fit_places_the_graph_inside_an_offset_viewport() {
        // Wide enough that fitting by width would leave every row sub-pixel, so
        // this covers the legibility clamp as well as the plain fit.
        let scene = synthetic_wide(60_000, 2_000, 100_000.0);
        let view = ScreenRect::new(17.0, 63.0, 1200.0, 700.0);
        let bounds = Bounds::scene(&scene);
        for flip in [false, true] {
            let vp = fit(&scene, view, 14.0, bounds, flip);
            let left = vp.sx(scene.min_x);
            // The root row is the one that has to stay on screen, whichever way
            // the rows run.
            let root_row = vp.sy(scene.min_y);
            assert!(
                left >= view.x && left <= view.x + view.w,
                "left {left} outside view"
            );
            assert!(
                root_row >= view.y && root_row <= view.y + view.h,
                "root row {root_row} outside view (flip {flip})"
            );
            // The graph as a whole has to intersect the view, otherwise the
            // user is staring at an empty canvas.
            let a = vp.sy(scene.min_y);
            let b = vp.sy(scene.max_y);
            let (top, bottom) = if a <= b { (a, b) } else { (b, a) };
            assert!(
                bottom >= view.y && top <= view.y + view.h,
                "graph [{top}, {bottom}] does not intersect the view (flip {flip})"
            );

            let g = geom(&scene, &vp, view, None);
            assert!(
                g.stats.quads > 0,
                "fitted view produced no geometry (flip {flip})"
            );
            assert!(
                g.stats.frames_considered > 0,
                "fitted view considered no frames (flip {flip})"
            );
        }
    }

    #[test]
    fn focus_keeps_the_calls_below() {
        // 12 rows of 20 frames: a grid, so every frame has calls below it.
        let scene = synthetic(240, 20);
        let view = ScreenRect::new(0.0, 0.0, 1200.0, 800.0);

        let target = scene
            .frames
            .iter()
            .position(|f| f.y == 30.0)
            .expect("row 2 exists") as u32;
        let focus = Focus::of(&scene, target);
        assert_eq!(focus.level, 2);

        let vp = fit(&scene, view, 14.0, focus.bounds(&scene), false);
        let g = geom(&scene, &vp, view, Some(focus));

        // The calls below the focused frame are expanded, not hidden. Labels
        // carry frame indices, so they prove what actually got drawn.
        let deepest = g
            .labels
            .iter()
            .filter(|l| !l.faded)
            .map(|l| row_of(&scene, l.frame))
            .fold(f32::MIN, f32::max);
        assert!(
            deepest > focus.y0 + 15.0,
            "nothing below the focused row was drawn: deepest labelled row {deepest}, focus row {}",
            focus.y0
        );
        assert!(
            g.stats.rows_drawn >= 8,
            "only {} rows drawn, the subtree was not expanded",
            g.stats.rows_drawn
        );
    }

    #[test]
    fn focus_fades_only_the_path_to_the_root() {
        // Shallow and wide, so fitting the focused frame by width leaves vertical
        // room for the path above it to stay on screen.
        let scene = synthetic_wide(60, 20, 4000.0);
        let view = ScreenRect::new(0.0, 0.0, 1200.0, 800.0);

        let target = scene
            .frames
            .iter()
            .position(|f| f.y == 15.0)
            .expect("row 1 exists") as u32;
        let focus = Focus::of(&scene, target);
        assert_eq!(focus.level, 1);

        let vp = fit(&scene, view, 14.0, focus.bounds(&scene), false);
        let g = geom(&scene, &vp, view, Some(focus));

        let expected: Vec<u32> = scene
            .ancestry(target)
            .into_iter()
            .filter(|a| *a != target)
            .collect();
        assert!(!expected.is_empty(), "the fixture needs an ancestor");

        // Only the dependency path is faded - not the whole row it sits on.
        let faded_frames: Vec<u32> = g
            .labels
            .iter()
            .filter(|l| l.faded)
            .map(|l| l.frame)
            .collect();
        assert_eq!(faded_frames, expected, "the faded set is not the call path");
        assert_eq!(
            g.faded.len(),
            expected.len(),
            "faded quads are not the call path"
        );

        // ... and every faded frame keeps its name.
        for l in g.labels.iter().filter(|l| l.faded) {
            assert!(
                !scene.name_of(l.frame).is_empty(),
                "a faded frame lost its label"
            );
        }
        // Nothing on the path is drawn at full strength.
        for l in g.labels.iter().filter(|l| !l.faded) {
            assert!(
                row_of(&scene, l.frame) >= focus.y0,
                "a row above the focus was drawn at full strength"
            );
        }
    }

    #[test]
    fn focus_never_draws_outside_the_subtree() {
        let scene = synthetic(240, 20);
        let view = ScreenRect::new(0.0, 0.0, 1200.0, 800.0);
        let target = scene
            .frames
            .iter()
            .position(|f| f.y == 30.0)
            .expect("row 2 exists") as u32;
        let focus = Focus::of(&scene, target);
        let vp = fit(&scene, view, 14.0, focus.bounds(&scene), false);

        let g = geom(&scene, &vp, view, Some(focus));
        // Every bright label must sit on or below the focus row; the faded ones
        // must sit above it.
        for l in &g.labels {
            if l.faded {
                assert!(
                    row_of(&scene, l.frame) < focus.y0,
                    "a faded label sits below the focus row"
                );
            } else {
                assert!(
                    row_of(&scene, l.frame) >= focus.y0,
                    "a row above the focus was drawn at full strength"
                );
            }
        }
        // And framing the same subtree must never cost more than the whole graph.
        let all = geom(&scene, &vp, view, None);
        assert!(
            all.stats.frames_considered >= g.stats.frames_considered,
            "focusing increased the work"
        );
    }

    #[test]
    fn focus_keeps_the_quad_budget() {
        let scene = synthetic(200_000, 400);
        let view = ScreenRect::new(1.0, 64.0, 1599.0, 836.0);
        let target = scene
            .frames
            .iter()
            .position(|f| f.y == 15.0)
            .expect("row 1 exists") as u32;
        let focus = Focus::of(&scene, target);
        for bounds in [Bounds::scene(&scene), focus.bounds(&scene)] {
            let vp = fit(&scene, view, 14.0, bounds, false);
            let g = geom(&scene, &vp, view, Some(focus));
            assert!(
                g.len() <= DEFAULT_BUDGET,
                "emitted {} quads with a budget of {DEFAULT_BUDGET}",
                g.len()
            );
            assert_eq!(g.stats.dropped, 0, "the geometry budget was exceeded");
        }
    }

    #[test]
    fn flip_y_mirrors_the_rows() {
        let scene = synthetic(1_000, 50);
        let view = ScreenRect::new(0.0, 0.0, 1000.0, 700.0);
        let bounds = Bounds::scene(&scene);
        let a = fit(&scene, view, 10.0, bounds, false);
        let b = fit(&scene, view, 10.0, bounds, true);
        assert!(a.sy(scene.min_y) < a.sy(scene.max_y));
        assert!(b.sy(scene.min_y) > b.sy(scene.max_y));

        let mut vp = b;
        let (anchor_x, anchor_y) = (321.0f32, 222.0f32);
        let (gx, gy) = (vp.gx(anchor_x), vp.gy(anchor_y) as f32);
        vp.zoom_at(anchor_x, anchor_y, 3.7);
        assert!((vp.sx(gx) - anchor_x).abs() < 0.01);
        assert!((vp.sy(gy) - anchor_y).abs() < 0.01);
    }

    #[test]
    fn culling_never_exceeds_budget() {
        let scene = synthetic(200_000, 400);
        let view = ScreenRect::new(0.0, 0.0, 1600.0, 900.0);
        for zoom in [0.001f64, 0.01, 0.1, 0.5, 1.0, 4.0, 40.0] {
            for flip in [false, true] {
                let vp = Viewport {
                    pan_x: 0.0,
                    pan_y: 0.0,
                    scale: zoom,
                    flip_y: flip,
                };
                let g = geom(&scene, &vp, view, None);
                assert!(
                    g.len() <= DEFAULT_BUDGET,
                    "zoom {zoom} emitted {} quads",
                    g.len()
                );
                assert_eq!(g.stats.dropped, 0, "zoom {zoom} dropped quads");
            }
        }
    }

    #[test]
    fn fully_zoomed_out_still_draws_something() {
        let scene = synthetic(50_000, 500);
        let view = ScreenRect::new(0.0, 0.0, 1200.0, 800.0);
        let vp = fit(&scene, view, 10.0, Bounds::scene(&scene), false);
        let g = geom(&scene, &vp, view, None);
        assert!(g.len() > 0);
        assert!(g.stats.frames_considered > 0);
    }

    #[test]
    fn focus_frame_keeps_the_context_below() {
        let scene = synthetic(2_000, 40);
        let view = ScreenRect::new(0.0, 0.0, 1200.0, 800.0);
        // A narrow frame deep in the graph: zooming it to the full width would
        // hide everything under it.
        let target = scene
            .frames
            .iter()
            .position(|f| f.y == 300.0 && f.w < 30.0)
            .expect("a narrow frame deep down") as u32;
        let vp = focus_frame(&scene, target, view, false);
        let g = geom(&scene, &vp, view, None);
        let drawn_rows = g
            .quads
            .iter()
            .map(|q| q.y0)
            .fold((f32::MAX, f32::MIN), |(lo, hi), y| (lo.min(y), hi.max(y)));
        let rows = ((drawn_rows.1 - drawn_rows.0) / 15.0).ceil();
        assert!(
            rows >= MIN_VISIBLE_ROWS as f32 - 1.0,
            "zooming to a frame left only {rows} rows on screen"
        );
    }

    #[test]
    fn zoom_at_keeps_anchor_fixed() {
        let mut vp = Viewport {
            pan_x: 12.0,
            pan_y: -5.0,
            scale: 0.5,
            flip_y: false,
        };
        let (ax, ay) = (321.0f32, 222.0f32);
        let (gx, gy) = (vp.gx(ax), vp.gy(ay) as f32);
        vp.zoom_at(ax, ay, 3.7);
        assert!((vp.sx(gx) - ax).abs() < 0.01);
        assert!((vp.sy(gy) - ay).abs() < 0.01);
    }
}
