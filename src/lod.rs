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

    pub fn bounds(&self, scene: &Scene) -> Bounds {
        Bounds {
            x0: self.x0,
            x1: self.x1,
            y0: self.y0,
            y1: scene.max_y,
        }
    }
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
    let scale = (((view.w - 40.0).max(16.0) as f64) / f.w.max(1e-9)).clamp(1e-12, 1e12);
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

struct RowBudget {
    min_px: f32,
    label_cap: usize,
}

/// Build the quads (and label requests) for one frame of interaction.
#[allow(clippy::too_many_arguments)]
pub fn build(
    scene: &Scene,
    vp: &Viewport,
    view: ScreenRect,
    budget: usize,
    name_colors: Option<&[u32]>,
    focus: Option<Focus>,
    quads: &mut Vec<Quad>,
    labels: &mut Vec<LabelReq>,
) -> Stats {
    quads.clear();
    labels.clear();
    let mut stats = Stats::default();

    if scene.is_empty() || vp.scale <= 0.0 || !vp.scale.is_finite() {
        return stats;
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

    // Focusing a subtree is just a tighter cull window plus a floor on the row
    // index: a subtree occupies one contiguous x-interval per row, so the two
    // constraints are exact and cost nothing.
    if let Some(f) = focus {
        gx0 = gx0.max(f.x0);
        gx1 = gx1.min(f.x1);
        if gx1 <= gx0 {
            return stats;
        }
    }

    let all_levels = scene.levels_in_range(gy_lo, gy_hi);
    let levels = match focus {
        Some(f) => all_levels.start.max(f.level)..all_levels.end,
        None => all_levels,
    };
    let n_levels = levels.len() as u32;
    stats.rows_visited = n_levels;
    if n_levels == 0 {
        return stats;
    }

    // Vertical LOD: when rows are thinner than a pixel, step over them.
    let row_h = scene.levels[levels.start].h.max(1e-6);
    let row_px = row_h as f64 * vp.scale;
    let step = if row_px >= MIN_ROW_PX as f64 {
        1usize
    } else {
        ((MIN_ROW_PX as f64) / row_px).ceil().max(1.0) as usize
    };
    stats.row_step = step as u32;

    let drawn_rows = levels.len().div_ceil(step).max(1);
    // Guard against a pathological zoom-out where every row is sub-pixel.
    let per_row = (budget / drawn_rows).max(MIN_PER_ROW);
    let label_cap = (LABEL_BUDGET / drawn_rows).max(4);

    let budget_u = budget as u32;

    let mut li = levels.start;
    while li < levels.end {
        let level = &scene.levels[li];
        let sy0 = vp.sy(level.y);
        let sy1 = vp.sy(level.y + level.h);
        let (top, bottom) = if sy0 <= sy1 { (sy0, sy1) } else { (sy1, sy0) };
        if bottom < view.y - 1.0 || top > view.y + view.h + 1.0 {
            li += step;
            continue;
        }
        stats.rows_drawn += 1;

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
            budget_u,
            quads,
            labels,
            &mut stats,
        );
        li += step;
    }

    stats.quads = quads.len() as u32;
    stats.labels = labels.len() as u32;
    stats
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
    budget: u32,
    quads: &mut Vec<Quad>,
    labels: &mut Vec<LabelReq>,
    stats: &mut Stats,
) {
    let frames = &scene.frames;
    let mut run: Option<(f32, f32, u32, f32)> = None;
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
            match &mut run {
                Some(r) => {
                    r.1 = sx1;
                    if wpx > r.3 {
                        r.2 = color;
                        r.3 = wpx;
                    }
                    stats.frames_merged += 1;
                }
                None => run = Some((sx0, sx1, color, wpx)),
            }
            continue;
        }

        if let Some(r) = run.take() {
            if quads.len() < budget as usize {
                quads.push(Quad {
                    x0: r.0,
                    y0: sy0,
                    x1: r.1,
                    y1: sy1,
                    color: r.2,
                });
            } else {
                stats.dropped += 1;
            }
        }
        if quads.len() < budget as usize {
            quads.push(Quad {
                x0: sx0,
                y0: sy0,
                x1: sx1,
                y1: sy1,
                color,
            });
            stats.frames_drawn += 1;
        } else {
            stats.dropped += 1;
            continue;
        }

        if wpx >= LABEL_MIN_W && (sy1 - sy0) >= LABEL_MIN_H && row_labels < rb.label_cap {
            row_labels += 1;
            labels.push(LabelReq {
                frame: idx as u32,
                x: sx0 + 2.0,
                center_y: (sy0 + sy1) * 0.5,
            });
        }
    }

    if let Some(r) = run.take() {
        if quads.len() < budget as usize {
            quads.push(Quad {
                x0: r.0,
                y0: sy0,
                x1: r.1,
                y1: sy1,
                color: r.2,
            });
        } else {
            stats.dropped += 1;
        }
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

            let mut quads = Vec::new();
            let mut labels = Vec::new();
            let st = build(
                &scene,
                &vp,
                view,
                DEFAULT_BUDGET,
                None,
                None,
                &mut quads,
                &mut labels,
            );
            assert!(
                st.quads > 0,
                "fitted view produced no geometry (flip {flip})"
            );
            assert!(
                st.frames_considered > 0,
                "fitted view considered no frames (flip {flip})"
            );
        }
    }

    #[test]
    fn focus_draws_only_the_subtree() {
        let scene = synthetic(4_000, 100);
        let view = ScreenRect::new(0.0, 0.0, 1200.0, 800.0);
        let target = scene
            .frames
            .iter()
            .position(|f| f.y > 0.0)
            .expect("non empty") as u32;
        let focus = Focus::of(&scene, target);
        let vp = fit(&scene, view, 14.0, focus.bounds(&scene), false);
        assert_eq!(focus.level, 1);

        let mut quads = Vec::new();
        let mut labels = Vec::new();
        let st = build(
            &scene,
            &vp,
            view,
            DEFAULT_BUDGET,
            None,
            Some(focus),
            &mut quads,
            &mut labels,
        );
        assert!(st.quads > 0, "focused view produced no geometry");

        let st_all = build(
            &scene,
            &vp,
            view,
            DEFAULT_BUDGET,
            None,
            None,
            &mut quads,
            &mut labels,
        );
        assert!(
            st_all.frames_considered >= st.frames_considered,
            "focus must never increase the work"
        );
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
        let mut quads = Vec::new();
        let mut labels = Vec::new();
        for zoom in [0.001f64, 0.01, 0.1, 0.5, 1.0, 4.0, 40.0] {
            for flip in [false, true] {
                let vp = Viewport {
                    pan_x: 0.0,
                    pan_y: 0.0,
                    scale: zoom,
                    flip_y: flip,
                };
                let st = build(
                    &scene,
                    &vp,
                    view,
                    DEFAULT_BUDGET,
                    None,
                    None,
                    &mut quads,
                    &mut labels,
                );
                assert!(
                    quads.len() <= DEFAULT_BUDGET,
                    "zoom {zoom} emitted {}",
                    quads.len()
                );
                assert_eq!(st.dropped, 0, "zoom {zoom} dropped quads");
            }
        }
    }

    #[test]
    fn fully_zoomed_out_still_draws_something() {
        let scene = synthetic(50_000, 500);
        let view = ScreenRect::new(0.0, 0.0, 1200.0, 800.0);
        let vp = fit(&scene, view, 10.0, Bounds::scene(&scene), false);
        let mut quads = Vec::new();
        let mut labels = Vec::new();
        let st = build(
            &scene,
            &vp,
            view,
            DEFAULT_BUDGET,
            None,
            None,
            &mut quads,
            &mut labels,
        );
        assert!(!quads.is_empty());
        assert!(st.frames_considered > 0);
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
