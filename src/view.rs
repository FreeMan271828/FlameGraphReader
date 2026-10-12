//! The interactive flame graph widget.
//!
//! All geometry comes from lod::build; this file only turns quads into an egui
//! mesh, lays out the handful of visible labels and handles input.

use std::time::Instant;

use eframe::egui;
use egui::{Align2, Color32, FontId, Mesh, Pos2, Rect, Sense, Shape, Stroke};

use crate::lod::{self, Bounds, Focus, Geometry, ScreenRect, Stats, Viewport};
use crate::palette::{self, Palette};
use crate::scene::Scene;

#[inline]
fn c32(c: u32) -> Color32 {
    Color32::from_rgb(
        ((c >> 16) & 0xff) as u8,
        ((c >> 8) & 0xff) as u8,
        (c & 0xff) as u8,
    )
}

#[inline]
fn pos(x: f32, y: f32) -> Pos2 {
    Pos2::new(x, y)
}

/// Dark rim drawn under every search marker, so the marker reads even when the
/// frame it sits on is a similar colour.
#[inline]
fn hit_halo() -> Color32 {
    Color32::from_rgba_unmultiplied(8, 10, 14, 235)
}

#[inline]
fn hit_wash(core: Color32) -> Color32 {
    Color32::from_rgba_unmultiplied(core.r(), core.g(), core.b(), 64)
}

/// Alpha applied to the path above a focused subtree.
const FADED_ALPHA: u8 = 52;

#[inline]
fn faded(c: Color32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), FADED_ALPHA)
}

/// Label colour for the faded path. Always light, because those rectangles are
/// blended into the dark panel background.
#[inline]
fn faded_label() -> Color32 {
    Color32::from_rgba_unmultiplied(228, 233, 240, 180)
}

/// Screen rect of a frame. Normalised, because a flipped viewport maps the
/// frame's top edge below its bottom edge.
fn frame_rect(vp: &Viewport, scene: &Scene, frame: u32) -> Rect {
    let f = scene.frames[frame as usize];
    let x0 = vp.sx(f.x);
    let x1 = vp.sx(f.x + f.w);
    let (mut y0, mut y1) = (vp.sy(f.y), vp.sy(f.y + f.h));
    if y1 < y0 {
        std::mem::swap(&mut y0, &mut y1);
    }
    // Match the inset the geometry applies, so outlines and label clips line up
    // with the row that is actually drawn.
    let inset = vp.band_inset(f.h);
    if inset > 0.0 && y1 - y0 > 2.0 * inset + 1.0 {
        y0 += inset;
        y1 -= inset;
    }
    let (mut xa, mut xb) = (x0.min(x1), x0.max(x1));
    let cin = lod::column_inset(xb - xa);
    if cin > 0.0 && xb - xa > 2.0 * cin + 0.5 {
        xa += cin;
        xb -= cin;
    }
    Rect::from_min_max(pos(xa, y0), pos(xb, y1))
}

/// What the user did inside the graph this frame.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Action {
    None,
    Selected(u32),
    Focused(u32),
    /// The focused subtree changed, so the surrounding UI has to refresh.
    FocusChanged,
    FitRequested,
}

pub struct FlameView {
    pub vp: Viewport,
    pub initialized: bool,
    pub selected: Option<u32>,
    pub hovered: Option<u32>,
    pub palette: Palette,
    /// Per-name colour table, rebuilt when the palette changes or a scene loads.
    name_colors: Vec<u32>,
    palette_dirty: bool,
    pub budget: usize,
    pub show_labels: bool,
    pub highlight: Vec<u32>,
    pub current_hit: usize,
    pub last_view: ScreenRect,

    /// When set, only this frame and its descendants are drawn.
    pub focus: Option<Focus>,
    /// Cached path from the root down to the focused frame. Kept here so hit
    /// testing and the breadcrumb bar do not recompute it every frame.
    focus_path: Vec<u32>,
    /// Clicking a frame drills into its subtree. On by default: it is the
    /// fastest way to answer "what is under this?".
    pub focus_on_click: bool,
    /// Present the graph with the widest row at the bottom (browser convention),
    /// mirroring whenever that is not how the document is written.
    pub root_at_bottom: bool,
    /// The next refit ignores the legibility clamp and shows the whole graph.
    exact_fit: bool,

    /// Frames to draw at full strength, plus the faded rows above a focused
    /// subtree and the labels to lay out over them.
    pub geometry: Geometry,
    pub build_us: f64,
    pub label_us: f64,
    pub mesh_vertices: usize,
}

impl Default for FlameView {
    fn default() -> Self {
        FlameView {
            vp: Viewport::default(),
            initialized: false,
            selected: None,
            hovered: None,
            palette: Palette::Native,
            name_colors: Vec::new(),
            palette_dirty: true,
            budget: lod::DEFAULT_BUDGET,
            show_labels: true,
            highlight: Vec::new(),
            current_hit: 0,
            last_view: ScreenRect::new(0.0, 0.0, 1.0, 1.0),
            focus: None,
            focus_path: Vec::new(),
            exact_fit: false,
            focus_on_click: true,
            root_at_bottom: true,
            geometry: Geometry::default(),
            build_us: 0.0,
            label_us: 0.0,
            mesh_vertices: 0,
        }
    }
}

impl FlameView {
    /// The search hits that get a marker this frame, as (index, frame).
    ///
    /// Two filters, both of which were bugs once: only a window around the
    /// current hit is walked, so a hit far down the list still gets its marker;
    /// and only frames the focused view actually draws qualify, because
    /// highlighting a culled frame left markers floating in the empty half of a
    /// focused canvas.
    fn markers<'a>(&'a self, scene: &'a Scene) -> impl Iterator<Item = (usize, u32)> + 'a {
        const WINDOW: usize = 400;
        let lo = self.current_hit.min(self.highlight.len().saturating_sub(1));
        let lo = lo.saturating_sub(WINDOW / 2);
        let hi = (lo + WINDOW).min(self.highlight.len());
        let focus = self.focus;
        (lo..hi).filter_map(move |i| {
            let fr = self.highlight[i];
            lod::is_focused_visible(scene, focus, &self.focus_path, fr).then_some((i, fr))
        })
    }

    /// Resolve a pending refit.
    ///
    /// Split out of show() so the framing rules can be tested without a Ui: this
    /// is where "drilling in keeps your zoom" actually lives.
    pub fn apply_pending_fit(&mut self, scene: &Scene, view: ScreenRect) {
        if self.initialized {
            return;
        }
        let exact = std::mem::take(&mut self.exact_fit);
        let flip = self.flip_y(scene);
        self.vp = match self.focus {
            // Recentre at the zoom the user is already working at. Changing the
            // scale on every click threw away the framing they had just chosen.
            Some(f) if !exact => lod::focus_centre(scene, f.frame, view, flip, self.vp.scale),
            _ => lod::fit(scene, view, 14.0, self.bounds(scene), flip, exact),
        };
        self.initialized = true;
    }

    /// Show the entire graph, even if that makes the rows sub-pixel. Explicit
    /// "fit"/"whole graph" actions mean exactly that, unlike the automatic fit
    /// on load which keeps the rows readable.
    pub fn reset_fit(&mut self) {
        self.initialized = false;
        self.exact_fit = true;
    }

    /// Whether the rows have to be mirrored to satisfy the orientation
    /// preference.
    ///
    /// This is an absolute comparison, not a one-way "flip it if it is upside
    /// down": the toggle has to change the picture for every document, including
    /// ones that already happen to be rooted at the bottom.
    #[inline]
    pub fn flip_y(&self, scene: &Scene) -> bool {
        lod::flip_for(self.root_at_bottom, scene)
    }

    /// The region currently being explored.
    #[inline]
    pub fn bounds(&self, scene: &Scene) -> Bounds {
        match self.focus {
            Some(f) => f.bounds(scene),
            None => Bounds::scene(scene),
        }
    }

    /// Drill into a frame: draw only it and its descendants.
    pub fn set_focus(&mut self, scene: &Scene, frame: u32) {
        self.focus = Some(Focus::of(scene, frame));
        self.focus_path = scene.ancestry(frame);
        self.selected = Some(frame);
        self.initialized = false;
        self.exact_fit = false;
    }

    /// Return to the whole graph.
    pub fn clear_focus(&mut self) {
        if self.focus.take().is_some() {
            self.focus_path.clear();
            self.initialized = false;
        }
    }

    /// Path from the root down to the focused frame, for breadcrumbs.
    pub fn focus_path(&self) -> &[u32] {
        &self.focus_path
    }

    pub fn set_palette(&mut self, p: Palette) {
        if self.palette != p {
            self.palette = p;
            self.palette_dirty = true;
        }
    }

    /// Rebuild the per-name colour table. One linear pass over the frames, so
    /// this stays cheap even for multi-million frame graphs.
    fn ensure_colors(&mut self, scene: &Scene) {
        if !self.palette_dirty && self.name_colors.len() == scene.names.len() {
            return;
        }
        let mut native = vec![0u32; scene.names.len()];
        for f in &scene.frames {
            native[f.name as usize] = f.color;
        }
        self.name_colors = match self.palette {
            Palette::Native => native,
            p => scene
                .names
                .iter()
                .enumerate()
                .map(|(i, n)| p.color(n, native[i]))
                .collect(),
        };
        self.palette_dirty = false;
    }

    #[inline]
    fn frame_color(&self, scene: &Scene, frame: u32) -> u32 {
        let f = scene.frames[frame as usize];
        if self.palette == Palette::Native {
            f.color
        } else {
            self.name_colors
                .get(f.name as usize)
                .copied()
                .unwrap_or(f.color)
        }
    }

    /// Build only the geometry. Split out so --bench can drive it headlessly.
    pub fn build_geometry(&mut self, scene: &Scene, view: ScreenRect) -> Stats {
        self.ensure_colors(scene);
        let table = if self.palette == Palette::Native {
            None
        } else {
            Some(&self.name_colors[..])
        };
        let t0 = Instant::now();
        lod::build(
            scene,
            &self.vp,
            view,
            self.budget,
            table,
            self.focus,
            &mut self.geometry,
        );
        self.build_us = t0.elapsed().as_secs_f64() * 1e6;
        self.geometry.stats
    }

    pub fn show(&mut self, ui: &mut egui::Ui, scene: &Scene, placeholder: &str) -> Action {
        let avail = ui.available_size();
        let (resp, painter) = ui.allocate_painter(avail, Sense::click_and_drag());
        let rect = resp.rect;
        let view = ScreenRect::new(rect.min.x, rect.min.y, rect.width(), rect.height());
        self.last_view = view;
        let mut action = Action::None;

        if scene.is_empty() {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                placeholder,
                FontId::proportional(15.0),
                Color32::from_gray(140),
            );
            return action;
        }

        // ---- input -------------------------------------------------------
        if resp.dragged() {
            let d = resp.drag_delta();
            if d != egui::Vec2::ZERO {
                self.vp.pan_x += d.x as f64 / self.vp.scale;
                self.vp.pan_y += d.y as f64 / self.vp.scale;
            }
        }
        let hover_pos = resp.hover_pos();
        if let Some(p) = hover_pos {
            let scroll = ui.input(|i| i.smooth_scroll_delta);
            let zoom_delta = ui.input(|i| i.zoom_delta());
            let mut factor = 1.0f32;
            if scroll.y.abs() > 0.01 {
                factor *= (scroll.y * 0.0026).exp();
            }
            if (zoom_delta - 1.0).abs() > 1e-4 {
                factor *= zoom_delta;
            }
            if (factor - 1.0).abs() > 1e-6 {
                self.vp.zoom_at(p.x, p.y, factor.clamp(0.2, 5.0) as f64);
            }
        }

        let hovered = hover_pos.and_then(|p| {
            let hit = scene.frame_at(self.vp.gx(p.x), self.vp.gy(p.y) as f32)?;
            // The empty space around a focused subtree must be genuinely empty:
            // only the faded path and the subtree itself are on screen, so only
            // those may be hovered or clicked.
            if !lod::is_focused_visible(scene, self.focus, &self.focus_path, hit) {
                return None;
            }
            Some(hit)
        });
        self.hovered = hovered;

        if resp.clicked() {
            if let Some(h) = hovered {
                self.selected = Some(h);
                if self.focus_on_click && self.focus.map(|f| f.frame) != Some(h) {
                    // Drill in: only this frame and its descendants stay.
                    self.set_focus(scene, h);
                    action = Action::FocusChanged;
                } else {
                    action = Action::Selected(h);
                }
            } else if self.focus.is_none() {
                // Clicking empty space clears the selection, but not while a
                // subtree is focused: the space around it is mostly the culled
                // away part of the graph, and a stray click should not throw
                // away what the user is inspecting.
                self.selected = None;
            }
        }
        if resp.double_clicked() {
            if let Some(h) = hovered {
                self.selected = Some(h);
                action = Action::Focused(h);
            }
        }
        if resp.secondary_clicked() {
            self.reset_fit();
            action = Action::FitRequested;
        }

        // A click that changed the focus (or a search jump) invalidates the
        // viewport. Refit here, after input and before geometry, so the frame
        // the user is about to see is already correct: egui only repaints on
        // input, so deferring the refit to the next frame would leave a stale
        // view on screen until the mouse moved.
        self.apply_pending_fit(scene, view);

        // ---- geometry ----------------------------------------------------
        self.build_geometry(scene, view);

        let mut mesh = Mesh::default();
        let total = self.geometry.len();
        mesh.vertices.reserve(total * 4);
        mesh.indices.reserve(total * 6);
        // The rows above a focused subtree go in first and faded, so the call
        // path stays visible without competing with the subtree. One mesh means
        // one draw call for both passes.
        for q in &self.geometry.faded {
            if q.x1 <= q.x0 || q.y1 <= q.y0 {
                continue;
            }
            mesh.add_colored_rect(
                Rect::from_min_max(pos(q.x0, q.y0), pos(q.x1, q.y1)),
                faded(c32(q.color)),
            );
        }
        for q in &self.geometry.quads {
            if q.x1 <= q.x0 || q.y1 <= q.y0 {
                continue;
            }
            mesh.add_colored_rect(
                Rect::from_min_max(pos(q.x0, q.y0), pos(q.x1, q.y1)),
                c32(q.color),
            );
        }
        self.mesh_vertices = mesh.vertices.len();
        painter.add(Shape::mesh(mesh));

        // Search highlights. A thin translucent outline was nearly invisible at
        // the zoom levels people actually search at, so hits get a solid outline
        // and the current one is washed and outlined heavily. Only a window
        // around the current hit is walked, so a hit far down the list still
        // gets its marker.
        if !self.highlight.is_empty() {
            let core = c32(self.palette.hit_rgb());
            for (i, fr) in self.markers(scene) {
                let r = frame_rect(&self.vp, scene, fr);
                if r.max.x < rect.min.x
                    || r.min.x > rect.max.x
                    || r.max.y < rect.min.y
                    || r.min.y > rect.max.y
                {
                    continue;
                }
                // Halo first, core on top: the dark rim is what makes the marker
                // readable even when the frame underneath happens to be a
                // similar hue.
                if i == self.current_hit {
                    painter.rect_filled(r, egui::CornerRadius::ZERO, hit_wash(core));
                    outline(&painter, r, Stroke::new(6.0, hit_halo()));
                    outline(&painter, r, Stroke::new(2.5, core));
                } else {
                    outline(&painter, r, Stroke::new(3.5, hit_halo()));
                    outline(&painter, r, Stroke::new(1.5, core));
                }
            }
        }

        // ---- labels ------------------------------------------------------
        let t_lab = Instant::now();
        if self.show_labels {
            for l in &self.geometry.labels {
                let clip = frame_rect(&self.vp, scene, l.frame);
                let h = clip.height();
                if h < 9.0 {
                    continue;
                }
                let size = (h * 0.68).clamp(7.0, 15.0);
                let col = if l.faded {
                    // Names on the faded path must stay readable, so they use a
                    // light colour rather than the contrast colour of a
                    // rectangle that has been blended into the background.
                    faded_label()
                } else {
                    c32(palette::label_color(self.frame_color(scene, l.frame)))
                };
                painter.with_clip_rect(clip).text(
                    pos(l.x, l.center_y),
                    Align2::LEFT_CENTER,
                    scene.name_of(l.frame),
                    FontId::proportional(size),
                    col,
                );
            }
        }
        self.label_us = t_lab.elapsed().as_secs_f64() * 1e6;

        // ---- hover outline + tooltip -------------------------------------
        if let Some(h) = self.hovered {
            let r = frame_rect(&self.vp, scene, h);
            outline(
                &painter,
                r,
                Stroke::new(1.5, Color32::from_rgb(255, 255, 255)),
            );
            if let Some(p) = hover_pos {
                draw_tooltip(&painter, rect, p, scene, h);
            }
        }

        action
    }

    /// Zoom so a frame fills the view.
    pub fn focus(&mut self, scene: &Scene, frame: u32) {
        self.vp = lod::focus_frame(scene, frame, self.last_view, self.flip_y(scene));
        self.initialized = true;
        self.selected = Some(frame);
    }

    /// Bring a frame into view without touching the zoom.
    ///
    /// Search navigation in "keep the view" mode has to respect the resolution
    /// the user chose: it pans just far enough to show the hit and leaves the
    /// scale alone.
    pub fn reveal_frame(&mut self, scene: &Scene, frame: u32) {
        let f = scene.frames[frame as usize];
        let v = self.last_view;
        if v.w <= 0.0 || v.h <= 0.0 {
            return;
        }
        let (sx0, sx1) = (self.vp.sx(f.x), self.vp.sx(f.x + f.w));
        let (sy0, sy1) = (self.vp.sy(f.y), self.vp.sy(f.y + f.h));
        let (lo_x, hi_x) = (sx0.min(sx1), sx0.max(sx1));
        let (lo_y, hi_y) = (sy0.min(sy1), sy0.max(sy1));
        let m = 28.0f32;
        let scale = self.vp.scale;
        let (left, right) = (v.x + m, v.x + v.w - m);
        let (top, bottom) = (v.y + m, v.y + v.h - m);

        if hi_x > right {
            self.vp.pan_x -= (hi_x - right) as f64 / scale;
        } else if lo_x < left {
            self.vp.pan_x += (left - lo_x) as f64 / scale;
        }
        if hi_y > bottom {
            self.vp.pan_y -= (hi_y - bottom) as f64 / scale;
        } else if lo_y < top {
            self.vp.pan_y += (top - lo_y) as f64 / scale;
        }

        self.initialized = true;
        self.exact_fit = false;
    }

    /// Zoom to a horizontal range at a given row (used by search navigation).
    ///
    /// Sized so the target occupies most of the viewport without zooming in so
    /// far that the surrounding context disappears.
    pub fn focus_rect_visible(&mut self, x0: f64, x1: f64, y: f32, h: f32) {
        let v = self.last_view;
        let w = (x1 - x0).max(1e-9);
        let scale = (((v.w - 60.0).max(32.0) as f64) / w).clamp(1e-12, 1e12);
        let dir = if self.vp.flip_y { -1.0 } else { 1.0 };
        let y_center = y as f64 + h as f64 * 0.5;
        self.vp = Viewport {
            pan_x: (v.x as f64 + 30.0) / scale - x0,
            pan_y: (v.y as f64 + v.h as f64 * 0.4) / scale - dir * y_center,
            scale,
            flip_y: self.vp.flip_y,
        };
        self.initialized = true;
        self.exact_fit = false;
    }
}

fn outline(painter: &egui::Painter, r: Rect, stroke: Stroke) {
    // Four segments rather than rect_stroke, so the outline never depends on
    // corner-radius defaults.
    painter.line_segment([r.left_top(), r.right_top()], stroke);
    painter.line_segment([r.right_top(), r.right_bottom()], stroke);
    painter.line_segment([r.right_bottom(), r.left_bottom()], stroke);
    painter.line_segment([r.left_bottom(), r.left_top()], stroke);
}

fn draw_tooltip(painter: &egui::Painter, bounds: Rect, at: Pos2, scene: &Scene, frame: u32) {
    let name = scene.name_of(frame);
    let span = if scene.max_x > scene.min_x {
        scene.max_x - scene.min_x
    } else {
        1.0
    };
    let total = scene.total_width(frame) / span * 100.0;
    let own = scene.self_width(frame) / span * 100.0;
    let text = format!("{name}\ntotal {total:.1}%    self {own:.1}%");

    let galley = painter.layout_no_wrap(
        text,
        FontId::proportional(12.0),
        Color32::from_rgb(235, 238, 242),
    );
    let size = galley.size();
    let pad = egui::vec2(7.0, 5.0);
    let mut origin = at + egui::vec2(16.0, 18.0);
    if origin.x + size.x + pad.x * 2.0 > bounds.max.x {
        origin.x = bounds.max.x - size.x - pad.x * 2.0;
    }
    if origin.y + size.y + pad.y * 2.0 > bounds.max.y {
        origin.y = at.y - size.y - pad.y * 2.0 - 6.0;
    }
    origin.x = origin.x.max(bounds.min.x);
    origin.y = origin.y.max(bounds.min.y);
    let bg = Rect::from_min_size(origin, size + pad * 2.0);
    painter.rect_filled(
        bg,
        egui::CornerRadius::ZERO,
        Color32::from_rgba_unmultiplied(24, 28, 34, 242),
    );
    outline(
        &painter.clone(),
        bg,
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(120, 130, 145, 200)),
    );
    painter.galley(origin + pad, galley, Color32::WHITE);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lod::ScreenRect;
    use crate::scene::{Frame, Scene};

    fn scene() -> Scene {
        let f = |x: f64, w: f64, y: f32| Frame {
            x,
            w,
            y,
            h: 15.0,
            color: 0x80_40_20,
            name: 0,
        };
        let frames = vec![
            f(0.0, 1000.0, 0.0),
            f(0.0, 500.0, 15.0),
            f(500.0, 500.0, 15.0),
            f(500.0, 200.0, 30.0),
        ];
        Scene::build(
            frames,
            vec!["fn".into()],
            "t".into(),
            "mem".into(),
            0,
            0.0,
            [0.0, 0.0],
        )
    }

    #[test]
    fn markers_are_only_painted_on_frames_that_are_drawn() {
        // Frames: 0 = root [0,1000], 1 = [0,500], 2 = [500,500] sibling,
        // 3 = [500,200] under the sibling.
        let scene = scene();
        let mut fv = FlameView {
            highlight: vec![0, 1, 2, 3],
            ..Default::default()
        };
        fv.set_focus(&scene, 1);

        let marked: Vec<u32> = fv.markers(&scene).map(|(_, f)| f).collect();
        assert!(marked.contains(&1), "the focused frame lost its marker");
        assert!(
            marked.contains(&0),
            "the faded path is on screen, it keeps its marker"
        );
        assert!(
            !marked.contains(&2) && !marked.contains(&3),
            "a culled frame got a marker, it would float in empty space: {marked:?}"
        );
    }

    #[test]
    fn drilling_in_keeps_the_zoom() {
        let scene = scene();
        let view = ScreenRect::new(0.0, 0.0, 400.0, 200.0);
        let mut fv = FlameView {
            last_view: view,
            ..Default::default()
        };
        fv.vp = crate::lod::fit(
            &scene,
            view,
            14.0,
            crate::lod::Bounds::scene(&scene),
            false,
            false,
        );
        fv.vp.scale *= 3.0; // as if the user had zoomed in by hand
        let scale = fv.vp.scale;

        fv.set_focus(&scene, 2);
        fv.apply_pending_fit(&scene, view);

        assert_eq!(fv.vp.scale, scale, "clicking a frame changed the zoom");
        let f = scene.frames[2];
        let cy = fv.vp.sy(f.y + f.h * 0.5);
        let expected = view.y + view.h * 0.5;
        assert!(
            (cy - expected).abs() < 1.0,
            "the clicked frame is not on the centre line: {cy} vs {expected}"
        );
    }

    #[test]
    fn reveal_frame_keeps_the_zoom_and_pans_into_view() {
        let scene = scene();
        let view = ScreenRect::new(0.0, 0.0, 400.0, 200.0);
        // Zoomed in on the left of the graph, so frame 2 hangs off the right.
        let mut fv = FlameView {
            last_view: view,
            vp: crate::lod::focus_frame(&scene, 1, view, false),
            ..Default::default()
        };
        let scale = fv.vp.scale;

        fv.reveal_frame(&scene, 2);

        // "Keep the view" means exactly that: the resolution must not change.
        assert_eq!(fv.vp.scale, scale, "reveal changed the zoom");
        let f = scene.frames[2];
        let (x0, x1) = (fv.vp.sx(f.x), fv.vp.sx(f.x + f.w));
        assert!(
            x0 >= view.x - 0.5 && x1 <= view.x + view.w + 0.5,
            "frame 2 was not brought into view: [{x0}, {x1}] in a {}px wide view",
            view.w
        );
    }
}
