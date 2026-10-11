//! Application shell: tabs, toolbar, details panel, keyboard handling.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Color32, RichText};

use crate::loader::{LoadMsg, Loader};
use crate::palette::Palette;
use crate::progress::Progress;
use crate::scene::Scene;
use crate::search;
use crate::view::{Action, FlameView};

pub const MIN_BUDGET: usize = 10_000;
pub const MAX_BUDGET: usize = 400_000;

enum DocState {
    Loading,
    Ready(Arc<Scene>),
    Failed(String),
}

struct Doc {
    path: PathBuf,
    title: String,
    state: DocState,
    view: FlameView,
    cancel: Arc<AtomicBool>,
    progress: Arc<Progress>,
}

impl Doc {
    fn scene(&self) -> Option<Arc<Scene>> {
        match &self.state {
            DocState::Ready(s) => Some(s.clone()),
            _ => None,
        }
    }
}

struct Smoke {
    remaining: u32,
    warmup: u32,
    times: Vec<f32>,
    tick: u32,
}

pub struct FlameApp {
    loader: Loader,
    docs: Vec<Doc>,
    active: usize,

    query: String,
    case_sensitive: bool,
    hits: Vec<u32>,
    hit_total: usize,
    hit_cursor: usize,
    search_ms: f64,
    last_query: String,
    query_since: Option<Instant>,

    show_details: bool,
    show_help: bool,
    status: String,
    pick_rx: Option<Receiver<Option<Vec<PathBuf>>>>,
    smoke: Option<Smoke>,
    last_frame_at: Option<Instant>,
    frame_ms: f32,
}

impl FlameApp {
    pub fn new(_cc: &eframe::CreationContext<'_>, files: Vec<PathBuf>) -> FlameApp {
        let mut app = FlameApp {
            loader: Loader::new(),
            docs: Vec::new(),
            active: 0,
            query: String::new(),
            case_sensitive: false,
            hits: Vec::new(),
            hit_total: 0,
            hit_cursor: 0,
            search_ms: 0.0,
            last_query: String::new(),
            query_since: None,
            show_details: true,
            show_help: false,
            status: "Ready. Drop .svg files here, or press Ctrl+O.".into(),
            pick_rx: None,
            smoke: None,
            last_frame_at: None,
            frame_ms: 0.0,
        };
        for f in files {
            app.open(f, None);
        }
        app
    }

    pub fn with_smoke(mut self, frames: u32) -> Self {
        self.show_details = false;
        self.smoke = Some(Smoke {
            remaining: frames,
            warmup: 45,
            times: Vec::with_capacity(frames as usize),
            tick: 0,
        });
        self
    }

    // -- document management ------------------------------------------------

    fn open(&mut self, path: PathBuf, ctx: Option<&egui::Context>) {
        if let Some(i) = self.docs.iter().position(|d| d.path == path) {
            self.active = i;
            return;
        }
        let title = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let progress = Arc::new(Progress::new());
        let cancel = Arc::new(AtomicBool::new(false));
        self.loader
            .spawn(path.clone(), progress.clone(), cancel.clone());
        self.docs.push(Doc {
            path,
            title,
            state: DocState::Loading,
            view: FlameView::default(),
            cancel,
            progress,
        });
        self.active = self.docs.len() - 1;
        self.hits.clear();
        self.hit_total = 0;
        if let Some(c) = ctx {
            c.request_repaint();
        }
    }

    fn close(&mut self, idx: usize) {
        if idx >= self.docs.len() {
            return;
        }
        let doc = self.docs.remove(idx);
        doc.cancel.store(true, Ordering::Relaxed);
        if self.active >= self.docs.len() {
            self.active = self.docs.len().saturating_sub(1);
        }
        self.resync_search();
    }

    fn active_doc(&self) -> Option<&Doc> {
        self.docs.get(self.active)
    }

    fn active_scene(&self) -> Option<Arc<Scene>> {
        self.active_doc().and_then(|d| d.scene())
    }

    // -- background work ----------------------------------------------------

    fn poll_loads(&mut self, ctx: &egui::Context) {
        for msg in self.loader.poll() {
            match msg {
                LoadMsg::Finished { path, scene } => {
                    if let Some(doc) = self.docs.iter_mut().find(|d| d.path == path) {
                        doc.state = DocState::Ready(Arc::from(scene));
                        doc.view = FlameView::default();
                        self.status = format!("Loaded {} ({} frames)", path.display(), 0);
                    }
                }
                LoadMsg::Failed { path, error } => {
                    if let Some(doc) = self.docs.iter_mut().find(|d| d.path == path) {
                        doc.state = DocState::Failed(error.clone());
                    }
                    self.status = format!("Failed to load {}: {}", path.display(), error);
                }
            }
            self.resync_search();
            ctx.request_repaint();
        }

        if let Some(rx) = &self.pick_rx {
            match rx.try_recv() {
                Ok(Some(paths)) => {
                    self.pick_rx = None;
                    for p in paths {
                        self.open(p, Some(ctx));
                    }
                }
                Ok(None) => self.pick_rx = None,
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.pick_rx = None,
            }
        }

        let any_loading = self
            .docs
            .iter()
            .any(|d| matches!(d.state, DocState::Loading));
        if any_loading || self.pick_rx.is_some() {
            ctx.request_repaint_after(Duration::from_millis(70));
        }
    }

    fn pick_files(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel();
        self.pick_rx = Some(rx);
        thread::spawn(move || {
            let files = rfd::FileDialog::new()
                .set_title("Open flame graph SVG")
                .add_filter("Flame graph SVG", &["svg", "svgz", "gz"])
                .add_filter("All files", &["*"])
                .pick_files();
            let _ = tx.send(files);
        });
        ctx.request_repaint_after(Duration::from_millis(120));
    }

    // -- search -------------------------------------------------------------

    fn resync_search(&mut self) {
        self.last_query.clear();
        self.hits.clear();
        self.hit_total = 0;
        self.hit_cursor = 0;
        if let Some(doc) = self.docs.get_mut(self.active) {
            doc.view.highlight.clear();
            doc.view.current_hit = 0;
        }
    }

    fn run_search(&mut self) {
        self.last_query = self.query.clone();
        self.query_since = None;
        let Some(scene) = self.active_scene() else {
            self.hits.clear();
            self.hit_total = 0;
            return;
        };
        if self.query.trim().is_empty() {
            self.hits.clear();
            self.hit_total = 0;
            self.search_ms = 0.0;
        } else {
            let out = search::run(
                &scene,
                &self.query,
                self.case_sensitive,
                search::DEFAULT_LIMIT,
            );
            self.hits = out.hits;
            self.hit_total = out.total;
            self.search_ms = out.elapsed_ms;
        }
        self.hit_cursor = 0;
        if let Some(doc) = self.docs.get_mut(self.active) {
            doc.view.highlight = self.hits.clone();
            doc.view.current_hit = 0;
        }
    }

    fn step_hit(&mut self, delta: i64) {
        if self.hits.is_empty() {
            return;
        }
        let n = self.hits.len() as i64;
        self.hit_cursor = (((self.hit_cursor as i64 + delta) % n + n) % n) as usize;
        let frame = self.hits[self.hit_cursor];
        self.reveal(frame);
    }

    fn reveal(&mut self, frame: u32) {
        let Some(scene) = self.active_scene() else {
            return;
        };
        let f = scene.frames[frame as usize];
        let idx = self.active;
        if let Some(doc) = self.docs.get_mut(idx) {
            doc.view.current_hit = self.hit_cursor;
            doc.view.selected = Some(frame);
            let (x0, x1) = (f.x, f.x + f.w);
            let (y, h) = (f.y, f.h);
            doc.view.focus_rect_visible(x0, x1, y, h);
        }
    }

    // -- frame hooks --------------------------------------------------------

    fn tick(&mut self, ctx: &egui::Context) {
        self.poll_loads(ctx);

        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        for p in dropped {
            self.open(p, Some(ctx));
        }

        // Debounced search while typing.
        if self.query != self.last_query {
            if self.query_since.is_none() {
                self.query_since = Some(Instant::now());
            }
            ctx.request_repaint_after(Duration::from_millis(80));
        }
        if let Some(t0) = self.query_since {
            if t0.elapsed() >= Duration::from_millis(200) {
                self.run_search();
            }
        }

        self.handle_shortcuts(ctx);

        let now = Instant::now();
        if let Some(prev) = self.last_frame_at {
            let ms = (now - prev).as_secs_f32() * 1000.0;
            self.frame_ms = if self.frame_ms == 0.0 {
                ms
            } else {
                self.frame_ms * 0.9 + ms * 0.1
            };
        }
        self.last_frame_at = Some(now);

        self.tick_smoke(ctx);
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        use egui::Key;
        let typing = ctx.egui_wants_keyboard_input();

        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::O)) {
            self.pick_files(ctx);
            return;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::W)) {
            let a = self.active;
            self.close(a);
            return;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::F)) {
            self.show_details = true;
            return;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::F1)) {
            self.show_help = !self.show_help;
            return;
        }
        // Escape (or Backspace) unwinds one step at a time: first out of the
        // focused subtree, then off the selection. Clearing everything at once
        // made it impossible to back out of a deep drill-down gradually.
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape))
            || ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Backspace))
        {
            if let Some(doc) = self.docs.get_mut(self.active) {
                if doc.view.focus.is_some() {
                    doc.view.clear_focus();
                } else {
                    doc.view.selected = None;
                    doc.view.highlight.clear();
                    self.hits.clear();
                    self.hit_total = 0;
                }
            }
            return;
        }
        if typing {
            return;
        }

        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::N)) {
            self.step_hit(1);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::P)) {
            self.step_hit(-1);
        }
        let idx = self.active;
        let Some(doc) = self.docs.get_mut(idx) else {
            return;
        };
        let v = &mut doc.view;
        let center = (
            v.last_view.x + v.last_view.w * 0.5,
            v.last_view.y + v.last_view.h * 0.5,
        );
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Equals))
            || ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Plus))
        {
            v.vp.zoom_at(center.0, center.1, 1.25);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Minus)) {
            v.vp.zoom_at(center.0, center.1, 0.8);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Num0))
            || ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::F))
        {
            v.reset_fit();
        }
        let pan = 60.0;
        if ctx.input(|i| i.key_down(Key::ArrowLeft)) {
            v.vp.pan_x += pan / v.vp.scale;
        }
        if ctx.input(|i| i.key_down(Key::ArrowRight)) {
            v.vp.pan_x -= pan / v.vp.scale;
        }
        if ctx.input(|i| i.key_down(Key::ArrowUp)) {
            v.vp.pan_y += pan / v.vp.scale;
        }
        if ctx.input(|i| i.key_down(Key::ArrowDown)) {
            v.vp.pan_y -= pan / v.vp.scale;
        }
    }

    fn tick_smoke(&mut self, ctx: &egui::Context) {
        let Some(smoke) = &mut self.smoke else { return };
        smoke.tick += 1;
        if smoke.remaining == 0 {
            return;
        }
        // Drive continuous pan/zoom so the measurement covers culling + mesh
        // rebuild, not just a static frame.
        let idx = self.active;
        if let Some(doc) = self.docs.get_mut(idx) {
            let v = &mut doc.view;
            let cx = v.last_view.x + v.last_view.w * 0.5;
            let cy = v.last_view.y + v.last_view.h * 0.5;
            if smoke.tick % 90 == 0 {
                v.reset_fit();
            } else if smoke.tick % 90 < 45 {
                v.vp.zoom_at(cx, cy, 1.045);
            } else {
                v.vp.zoom_at(cx, cy, 0.965);
            }
        }
        if smoke.warmup > 0 {
            smoke.warmup -= 1;
        }
        smoke.remaining -= 1;
        ctx.request_repaint();
    }

    pub fn smoke_report(&self) -> Option<String> {
        let smoke = self.smoke.as_ref()?;
        if smoke.remaining > 0 || smoke.times.is_empty() {
            return None;
        }
        report_frame_times(
            &smoke.times,
            self.active_scene().as_deref(),
            self.docs.get(self.active),
        )
    }

    // -- UI -----------------------------------------------------------------

    fn toolbar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            if ui.button("Open SVG").clicked() {
                self.pick_files(ctx);
            }
            let has_doc = !self.docs.is_empty();
            ui.add_enabled_ui(has_doc, |ui| {
                if ui.button("Fit").clicked() {
                    if let Some(d) = self.docs.get_mut(self.active) {
                        d.view.reset_fit();
                    }
                }
                if ui.button("+").clicked() {
                    if let Some(d) = self.docs.get_mut(self.active) {
                        let c = (
                            d.view.last_view.x + d.view.last_view.w * 0.5,
                            d.view.last_view.y + d.view.last_view.h * 0.5,
                        );
                        d.view.vp.zoom_at(c.0, c.1, 1.3);
                    }
                }
                if ui.button("-").clicked() {
                    if let Some(d) = self.docs.get_mut(self.active) {
                        let c = (
                            d.view.last_view.x + d.view.last_view.w * 0.5,
                            d.view.last_view.y + d.view.last_view.h * 0.5,
                        );
                        d.view.vp.zoom_at(c.0, c.1, 0.77);
                    }
                }
            });
            ui.separator();

            ui.label("Search:");
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .hint_text("substring of a frame name")
                    .desired_width(220.0),
            );
            if resp.changed() {
                self.query_since.get_or_insert_with(Instant::now);
            }
            if ui.checkbox(&mut self.case_sensitive, "Aa").changed() {
                self.query_since = Some(Instant::now() - Duration::from_secs(1));
            }
            if !self.hits.is_empty() {
                ui.label(format!("{} / {} hits", self.hit_cursor + 1, self.hit_total));
                if ui.small_button("<").clicked() {
                    self.step_hit(-1);
                }
                if ui.small_button(">").clicked() {
                    self.step_hit(1);
                }
            } else if !self.query.trim().is_empty() && self.last_query == self.query {
                ui.label(RichText::new("no matches").color(Color32::from_gray(140)));
            }

            ui.separator();
            let mut palette = self
                .docs
                .get(self.active)
                .map(|d| d.view.palette)
                .unwrap_or(Palette::Native);
            egui::ComboBox::from_id_salt("palette")
                .selected_text(palette.label())
                .width(120.0)
                .show_ui(ui, |ui| {
                    for p in Palette::ALL {
                        ui.selectable_value(&mut palette, p, p.label());
                    }
                });
            if let Some(d) = self.docs.get_mut(self.active) {
                d.view.set_palette(palette);
            }

            if let Some(d) = self.docs.get_mut(self.active) {
                ui.add(
                    egui::Slider::new(&mut d.view.budget, MIN_BUDGET..=MAX_BUDGET)
                        .logarithmic(true)
                        .text("quads")
                        .max_decimals(0),
                );
                ui.checkbox(&mut d.view.show_labels, "labels");
                if ui
                    .checkbox(&mut d.view.root_at_bottom, "root at bottom")
                    .on_hover_text(
                        "Mirror the rows so the widest row (the profile root) is at the \n                         bottom, the way browsers draw a flame graph. Documents that are \n                         already rooted at the bottom are left alone.",
                    )
                    .changed()
                {
                    // Mirrored rows need a fresh fit.
                    d.view.initialized = false;
                }
                ui.checkbox(&mut d.view.focus_on_click, "click drills in")
                    .on_hover_text(
                        "Clicking a frame shows only that frame and the calls beneath it. \n                         Use the breadcrumb bar or Esc / right click to go back.",
                    );
            }
            ui.checkbox(&mut self.show_details, "details");
            if ui
                .button("?")
                .on_hover_text("Keyboard shortcuts (F1)")
                .clicked()
            {
                self.show_help = !self.show_help;
            }
        });
    }

    fn tabs(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut activate: Option<usize> = None;
        let mut close: Option<usize> = None;
        let active = self.active;
        let docs = &self.docs;
        egui::ScrollArea::horizontal()
            .id_salt("fr_tabs")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (i, doc) in docs.iter().enumerate() {
                        let marker = match doc.state {
                            DocState::Loading => "..",
                            DocState::Failed(_) => "!!",
                            DocState::Ready(_) => "  ",
                        };
                        let label = format!("{marker} {}", doc.title);
                        if ui.selectable_label(i == active, label).clicked() {
                            activate = Some(i);
                        }
                        if ui.small_button("x").clicked() {
                            close = Some(i);
                        }
                    }
                });
            });
        if let Some(i) = activate {
            self.active = i;
            self.query_since = Some(Instant::now() - Duration::from_secs(1));
        }
        if let Some(i) = close {
            self.close(i);
        }
        let _ = ctx;
    }

    fn status_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            if let Some(doc) = self.docs.get(self.active) {
                match &doc.state {
                    DocState::Loading => {
                        let p = &doc.progress;
                        let frac = p.fraction();
                        ui.add(
                            egui::ProgressBar::new(frac)
                                .desired_width(160.0)
                                .text(format!("{} {:.0}%", p.stage().label(), frac * 100.0)),
                        );
                        ui.label(format!("{} frames so far", p.frames()));
                    }
                    DocState::Failed(e) => {
                        ui.label(
                            RichText::new(format!("error: {e}"))
                                .color(Color32::from_rgb(240, 110, 110)),
                        );
                    }
                    DocState::Ready(scene) => {
                        let mbs = if scene.parse_ms > 0.0 {
                            scene.bytes_parsed as f64 / 1e6 / (scene.parse_ms / 1000.0)
                        } else {
                            0.0
                        };
                        ui.label(format!(
                            "{} frames | {} rows | {:.1} MB | parse {:.0} ms ({:.0} MB/s)",
                            fmt_int(scene.len()),
                            scene.levels.len(),
                            scene.bytes_parsed as f64 / 1e6,
                            scene.parse_ms,
                            mbs
                        ));
                        ui.separator();
                        ui.label(format!(
                            "visible {} | quads {} | cull {:.2} ms",
                            fmt_int(doc.view.geometry.stats.frames_considered as usize),
                            fmt_int(doc.view.geometry.stats.quads as usize),
                            doc.view.build_us / 1000.0
                        ));
                    }
                }
            } else {
                ui.label(&self.status);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.frame_ms > 0.0 {
                    ui.label(format!("{:.0} fps", 1000.0 / self.frame_ms.max(0.001)));
                }
                if self.search_ms > 0.0 {
                    ui.label(format!("search {:.0} ms |", self.search_ms));
                }
                let _ = ctx;
            });
        });
    }

    fn details(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(scene) = self.active_scene() else {
            ui.label("no document");
            return;
        };

        let span = (scene.max_x - scene.min_x).max(1e-6);

        ui.heading("Details");
        ui.label(RichText::new(&scene.title).strong());
        ui.label(
            RichText::new(&scene.source)
                .small()
                .color(Color32::from_gray(130)),
        );
        if scene.declared[0] > 0.0 {
            ui.label(
                RichText::new(format!(
                    "svg canvas {:.0} x {:.0}",
                    scene.declared[0], scene.declared[1]
                ))
                .small()
                .color(Color32::from_gray(130)),
            );
        }
        ui.separator();
        let selected = self.docs.get(self.active).and_then(|d| d.view.selected);
        match selected {
            Some(frame) => {
                let name = scene.name_of(frame).to_string();
                ui.label(RichText::new(&name).strong());
                ui.label(format!(
                    "total {:.2}%",
                    scene.total_width(frame) / span * 100.0
                ));
                ui.label(format!(
                    "self  {:.2}%",
                    scene.self_width(frame) / span * 100.0
                ));
                ui.separator();

                let ancestry = scene.ancestry(frame);
                ui.label(RichText::new("Call path").strong());
                egui::ScrollArea::vertical()
                    .id_salt("fr_path")
                    .max_height(150.0)
                    .show(ui, |ui| {
                        for (i, a) in ancestry.iter().enumerate() {
                            let label = format!("{} {}", "  ".repeat(i.min(12)), scene.name_of(*a));
                            if ui.selectable_label(*a == frame, label).clicked() {
                                let f = scene.frames[*a as usize];
                                if let Some(d) = self.docs.get_mut(self.active) {
                                    d.view.focus_rect_visible(f.x, f.x + f.w, f.y, f.h);
                                    d.view.selected = Some(*a);
                                }
                            }
                        }
                    });

                ui.separator();
                let children = scene.children(frame);
                ui.label(RichText::new(format!("Children ({})", children.len())).strong());
                let mut kids: Vec<(u32, f64)> =
                    children.map(|c| (c as u32, scene.frames[c].w)).collect();
                kids.sort_by(|a, b| b.1.total_cmp(&a.1));
                egui::ScrollArea::vertical()
                    .id_salt("fr_kids")
                    .max_height(220.0)
                    .show(ui, |ui| {
                        for (c, w) in kids.iter().take(300) {
                            let label = format!("{:.2}%  {}", w / span * 100.0, scene.name_of(*c));
                            if ui.button(label).clicked() {
                                let f = scene.frames[*c as usize];
                                if let Some(d) = self.docs.get_mut(self.active) {
                                    d.view.focus_rect_visible(f.x, f.x + f.w, f.y, f.h);
                                    d.view.selected = Some(*c);
                                }
                            }
                        }
                    });
            }
            None => {
                ui.label(
                    RichText::new("Click a frame to inspect it.").color(Color32::from_gray(150)),
                );
            }
        }

        ui.separator();
        ui.label(RichText::new(format!("Search hits ({})", self.hit_total)).strong());
        if self.hits.is_empty() {
            ui.label(RichText::new("Type in the search box above.").color(Color32::from_gray(150)));
        } else {
            let total = self.hits.len();
            let cursor = self.hit_cursor;
            let mut go: Option<usize> = None;
            egui::ScrollArea::vertical()
                .id_salt("fr_hits")
                .max_height(320.0)
                .show_rows(ui, 18.0, total, |ui, range| {
                    for i in range {
                        let fr = self.hits[i];
                        let text = format!(
                            "{:.2}%  {}",
                            scene.frames[fr as usize].w / span * 100.0,
                            scene.name_of(fr)
                        );
                        if ui.selectable_label(i == cursor, text).clicked() {
                            go = Some(i);
                        }
                    }
                });
            if let Some(i) = go {
                self.hit_cursor = i;
                let fr = self.hits[i];
                self.reveal(fr);
            }
        }
        let _ = ctx;
    }

    /// Breadcrumb trail for the focused subtree, plus a way back to the root.
    fn breadcrumbs(&mut self, ui: &mut egui::Ui) {
        let Some(scene) = self.active_scene() else {
            return;
        };
        let idx = self.active;
        let Some(doc) = self.docs.get(idx) else {
            return;
        };
        let focus = doc.view.focus;
        let path = doc.view.focus_path(&scene);

        ui.horizontal(|ui| {
            ui.label(RichText::new("Path:").color(Color32::from_gray(140)));
            let all = ui.selectable_label(focus.is_none(), "all");
            if all.clicked() {
                if let Some(d) = self.docs.get_mut(idx) {
                    d.view.clear_focus();
                    d.view.reset_fit();
                }
                self.status = "showing the whole graph".into();
                return;
            }
            for (i, frame) in path.iter().enumerate() {
                ui.label(RichText::new(">").color(Color32::from_gray(110)));
                let last = i + 1 == path.len();
                let label = scene.name_of(*frame);
                let short = if label.chars().count() > 42 {
                    let s: String = label.chars().take(39).collect();
                    format!("{s}...")
                } else {
                    label.to_string()
                };
                if ui.selectable_label(last, short).clicked() && !last {
                    if let Some(d) = self.docs.get_mut(idx) {
                        d.view.set_focus(&scene, *frame);
                    }
                    self.status =
                        format!("showing only the subtree below {}", scene.name_of(*frame));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if focus.is_some() {
                    if ui
                        .button("Up one level")
                        .on_hover_text("Focus the parent frame")
                        .clicked()
                    {
                        let parent = if path.len() >= 2 {
                            Some(path[path.len() - 2])
                        } else {
                            None
                        };
                        if let Some(d) = self.docs.get_mut(idx) {
                            match parent {
                                Some(p) => d.view.set_focus(&scene, p),
                                None => {
                                    d.view.clear_focus();
                                    d.view.reset_fit();
                                }
                            }
                        }
                    }
                    if ui.button("Whole graph").clicked() {
                        if let Some(d) = self.docs.get_mut(idx) {
                            d.view.clear_focus();
                            d.view.reset_fit();
                        }
                        self.status = "showing the whole graph".into();
                    }
                }
            });
        });
    }

    fn graph(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if self.docs.is_empty() {
            self.welcome(ui, ctx);
            return;
        }
        let idx = self.active.min(self.docs.len() - 1);
        let scene = self.docs[idx].scene();
        match scene {
            Some(scene) => {
                let action = self.docs[idx].view.show(ui, &scene, "");
                match action {
                    Action::FocusChanged => {
                        if let Some(f) = self.docs[idx].view.focus {
                            self.status = format!(
                                "subtree below {} - rows above it stay faded, click one to go back up",
                                scene.name_of(f.frame)
                            );
                        }
                    }
                    Action::Focused(f) => {
                        self.docs[idx].view.focus(&scene, f);
                        self.status = format!("focused {}", scene.name_of(f));
                    }
                    Action::Selected(f) => {
                        self.status = format!(
                            "{}  total {:.2}%  self {:.2}%",
                            scene.name_of(f),
                            scene.total_width(f) / (scene.max_x - scene.min_x).max(1e-6) * 100.0,
                            scene.self_width(f) / (scene.max_x - scene.min_x).max(1e-6) * 100.0
                        );
                    }
                    Action::FitRequested => {
                        self.docs[idx].view.clear_focus();
                        self.docs[idx].view.reset_fit();
                        self.status = "showing the whole graph".into();
                    }
                    Action::None => {}
                }
            }
            None => {
                let loading = matches!(self.docs[idx].state, DocState::Loading);
                let msg = if loading {
                    "parsing..."
                } else {
                    "failed to load"
                };
                self.docs[idx].view.show(ui, &Scene::empty(), msg);
                if loading {
                    ctx.request_repaint_after(Duration::from_millis(70));
                }
            }
        }
    }

    fn welcome(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.vertical_centered(|ui| {
            ui.add_space(60.0);
            ui.heading("FlameReader");
            ui.label("A GPU accelerated SVG flame graph viewer for very large profiles.");
            ui.add_space(12.0);
            if ui.button("Open an SVG flame graph").clicked() {
                self.pick_files(ctx);
            }
            ui.add_space(20.0);
            ui.label(RichText::new("Drag and drop .svg / .svg.gz files anywhere in this window.").color(Color32::from_gray(150)));
            ui.add_space(6.0);
            ui.label(RichText::new("Wheel = zoom at cursor, drag = pan, double click = focus, right click = fit, F1 = help.").color(Color32::from_gray(150)));
        });
    }

    fn overlays(&mut self, ctx: &egui::Context) {
        if !self.show_help {
            return;
        }
        let mut open = self.show_help;
        egui::Window::new("Keyboard and mouse")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                for (k, v) in [
                    ("Wheel", "zoom at the cursor"),
                    ("Drag", "pan"),
                    (
                        "Click",
                        "drill into that frame: its subtree is drawn at full strength and the rows above it stay faded",
                    ),
                    ("Double click", "zoom the frame under the cursor to the viewport"),
                    ("Right click", "fit the whole graph"),
                    ("Ctrl+O", "open SVG files"),
                    ("Ctrl+W", "close the current tab"),
                    ("Ctrl+F", "show the details and search panel"),
                    ("F1", "toggle this window"),
                    ("/ n / p", "next / previous search hit"),
                    ("+ / -", "zoom in / out"),
                    ("0 or F", "fit"),
                    ("Arrows", "pan"),
                    ("Backspace / Esc", "step out of the focused subtree, then clear the selection"),
                ] {
                    ui.horizontal(|ui| {
                        ui.add_sized([110.0, 18.0], egui::Label::new(RichText::new(k).strong()));
                        ui.label(v);
                    });
                }
            });
        self.show_help = open;
    }
}

impl eframe::App for FlameApp {
    // eframe 0.36 hands the app a Ui rather than a Context.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.tick(&ctx);

        egui::Panel::top("fr_toolbar").show(ui, |ui| self.toolbar(ui, &ctx));
        egui::Panel::top("fr_tabs").show(ui, |ui| self.tabs(ui, &ctx));
        if self
            .active_doc()
            .map(|d| d.view.focus.is_some())
            .unwrap_or(false)
        {
            egui::Panel::top("fr_crumbs").show(ui, |ui| self.breadcrumbs(ui));
        }
        egui::Panel::bottom("fr_status").show(ui, |ui| self.status_bar(ui, &ctx));
        if self.show_details {
            egui::Panel::right("fr_details")
                .resizable(true)
                .default_size(330.0)
                .show(ui, |ui| self.details(ui, &ctx));
        }
        egui::CentralPanel::default().show(ui, |ui| self.graph(ui, &ctx));
        self.overlays(&ctx);

        if let Some(report) = self.smoke_report() {
            println!("{report}");
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

pub fn fmt_int(n: usize) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

fn report_frame_times(times: &[f32], scene: Option<&Scene>, doc: Option<&Doc>) -> Option<String> {
    if times.is_empty() {
        return None;
    }
    let mut sorted: Vec<f32> = times.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let mean = sorted.iter().sum::<f32>() / sorted.len() as f32;
    let p = |q: f32| sorted[((sorted.len() as f32 - 1.0) * q) as usize];
    let mut s = String::new();
    s.push_str("\n--- FlameReader render smoke test ---\n");
    if let Some(sc) = scene {
        s.push_str(&format!("frames           : {}\n", fmt_int(sc.len())));
        s.push_str(&format!("rows             : {}\n", sc.levels.len()));
        s.push_str(&format!("parse            : {:.0} ms\n", sc.parse_ms));
    }
    if let Some(d) = doc {
        s.push_str(&format!(
            "visible frames   : {}\n",
            fmt_int(d.view.geometry.stats.frames_considered as usize)
        ));
        s.push_str(&format!(
            "quads emitted    : {}\n",
            fmt_int(d.view.geometry.stats.quads as usize)
        ));
        s.push_str(&format!(
            "mesh vertices    : {}\n",
            fmt_int(d.view.mesh_vertices)
        ));
        s.push_str(&format!(
            "cull+mesh build  : {:.2} ms\n",
            d.view.build_us / 1000.0
        ));
        s.push_str(&format!(
            "label layout     : {:.2} ms\n",
            d.view.label_us / 1000.0
        ));
    }
    s.push_str(&format!("frames measured  : {}\n", times.len()));
    s.push_str(&format!(
        "frame time mean  : {:.2} ms ({:.0} fps)\n",
        mean,
        1000.0 / mean.max(0.001)
    ));
    s.push_str(&format!("frame time p50   : {:.2} ms\n", p(0.50)));
    s.push_str(&format!("frame time p95   : {:.2} ms\n", p(0.95)));
    s.push_str(&format!(
        "frame time max   : {:.2} ms\n",
        sorted[sorted.len() - 1]
    ));
    Some(s)
}
