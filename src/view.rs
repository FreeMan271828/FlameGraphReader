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
    let y0 = vp.sy(f.y);
    let y1 = vp.sy(f.y + f.h);
    Rect::from_min_max(pos(x0.min(x1), y0.min(y1)), pos(x0.max(x1), y0.max(y1)))
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
    /// Clicking a frame drills into its subtree. On by default: it is the
    /// fastest way to answer "what is under this?".
    pub focus_on_click: bool,
    /// Present the graph with the widest row at the bottom (browser convention).
    /// Only mirrors documents that are actually rooted at the top.
    pub root_at_bottom: bool,

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
    pub fn reset_fit(&mut self) {
        self.initialized = false;
    }

    /// Whether the rows have to be mirrored for the current document and
    /// orientation preference.
    #[inline]
    pub fn flip_y(&self, scene: &Scene) -> bool {
        self.root_at_bottom && scene.root_at_top
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
        self.selected = Some(frame);
        self.initialized = false;
    }

    /// Return to the whole graph.
    pub fn clear_focus(&mut self) {
        if self.focus.take().is_some() {
            self.initialized = false;
        }
    }

    /// Path from the root down to the focused frame, for breadcrumbs.
    pub fn focus_path(&self, scene: &Scene) -> Vec<u32> {
        match self.focus {
            Some(f) => scene.ancestry(f.frame),
            None => Vec::new(),
        }
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
            // While a subtree is focused, only that subtree and the faded rows
            // above it are on screen. Hitting a culled away frame would select
            // something the user cannot see.
            if let Some(f) = self.focus {
                let hf = scene.frames[hit as usize];
                let above = hf.y < f.y0;
                let inside_x = hf.x < f.x1 && hf.x + hf.w > f.x0;
                if !above && !inside_x {
                    return None;
                }
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
            } else {
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
        if !self.initialized {
            self.vp = lod::fit(scene, view, 14.0, self.bounds(scene), self.flip_y(scene));
            self.initialized = true;
        }

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

        // Search highlights: one thin outline per hit, capped for sanity.
        if !self.highlight.is_empty() {
            let n = self.highlight.len().min(400);
            for (i, &fr) in self.highlight.iter().take(n).enumerate() {
                let r = frame_rect(&self.vp, scene, fr);
                if r.max.x < rect.min.x
                    || r.min.x > rect.max.x
                    || r.max.y < rect.min.y
                    || r.min.y > rect.max.y
                {
                    continue;
                }
                let strong = i == self.current_hit;
                let col = if strong {
                    Color32::from_rgb(255, 214, 64)
                } else {
                    Color32::from_rgba_unmultiplied(255, 214, 64, 96)
                };
                outline(
                    &painter,
                    r,
                    Stroke::new(if strong { 2.0 } else { 1.0 }, col),
                );
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
