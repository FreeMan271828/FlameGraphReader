//! FlameReader - a GPU accelerated SVG flame graph viewer for very large
//! profiles.
//!
//! Run with no arguments for the GUI, or use one of the CLI modes:
//!
//!     flamereader big.svg            open a file in the GUI (no console window)
//!     flamereader --gen out.svg N    write a synthetic N frame flame graph
//!     flamereader --selftest f.svg   parse and verify, print statistics
//!     flamereader --bench f.svg      measure culling and mesh build cost
//!     flamereader --smoke f.svg N    render N frames headfully and report fps
//!
//! The binary is linked as a console application so the CLI modes can print. A
//! GUI launch detaches that console again (see detach_console_for_gui), unless it
//! was inherited from a terminal.

#![cfg_attr(
    all(windows, not(debug_assertions), feature = "gui"),
    windows_subsystem = "windows"
)]

mod app;
mod gen;
mod loader;
mod lod;
mod palette;
mod progress;
mod scene;
mod search;
mod svg;
mod view;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use eframe::egui;

#[allow(unused_imports)]
use crate::lod as _lod_reexport;
use crate::lod::{ScreenRect, DEFAULT_BUDGET};
use crate::progress::Progress;
use crate::scene::Scene;
use crate::view::FlameView;

enum Mode {
    Gui(Vec<PathBuf>),
    Gen {
        out: PathBuf,
        frames: usize,
    },
    SelfTest {
        file: PathBuf,
        bench: bool,
        iters: u32,
        chunks: bool,
    },
    Smoke {
        file: PathBuf,
        frames: u32,
    },
}

/// Detach the console Windows allocates for a console subsystem binary.
///
/// The binary stays a console application so that the CLI modes can print, but
/// launching it as a GUI would otherwise leave a black console window sitting
/// behind the whole session, which is not what double-clicking a viewer should
/// look like.
///
/// Two conditions keep the console:
/// * any CLI option is present, because the user asked for console output;
/// * more than one process is attached, which means the console belongs to a
///   terminal we were started from and taking it away would close their window.
#[cfg(windows)]
fn detach_console_for_gui(args: &[String]) {
    if args.iter().any(|a| a.starts_with('-')) {
        return;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetConsoleWindow() -> *mut core::ffi::c_void;
        fn GetConsoleProcessList(process_list: *mut u32, count: u32) -> u32;
        fn FreeConsole() -> i32;
    }

    // SAFETY: the three calls only touch this process's console attachment.
    unsafe {
        if GetConsoleWindow().is_null() {
            return;
        }
        let mut attached = [0u32; 2];
        if GetConsoleProcessList(attached.as_mut_ptr(), attached.len() as u32) > 1 {
            return;
        }
        FreeConsole();
    }
}

#[cfg(not(windows))]
fn detach_console_for_gui(_args: &[String]) {}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    detach_console_for_gui(&args);
    let mode = match parse_args(&args) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: {e}\n");
            print_help();
            return ExitCode::from(2);
        }
    };

    match mode {
        Mode::Gen { out, frames } => {
            println!(
                "Generating {} frames into {} ...",
                app::fmt_int(frames),
                out.display()
            );
            match gen::generate(&out, frames) {
                Ok(r) => {
                    println!(
                        "  {} frames, {} rows, {:.2} MB written in {:.0} ms",
                        app::fmt_int(r.frames),
                        r.rows,
                        r.bytes as f64 / 1e6,
                        r.millis
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Mode::SelfTest {
            file,
            bench,
            iters,
            chunks,
        } => run_selftest(&file, bench, iters, chunks),
        Mode::Smoke { file, frames } => run_gui(vec![file], Some(frames)),
        Mode::Gui(files) => run_gui(files, None),
    }
}

fn parse_args(args: &[String]) -> Result<Mode, String> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut i = 0usize;
    let mut selftest: Option<PathBuf> = None;
    let mut bench: Option<(PathBuf, u32)> = None;
    let mut gen_out: Option<(PathBuf, usize)> = None;
    let mut smoke: Option<(PathBuf, u32)> = None;
    let mut chunks = false;

    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("flamereader {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--gen" => {
                let out = args.get(i + 1).ok_or("--gen needs an output path")?;
                let n = args.get(i + 2).ok_or("--gen needs a frame count")?;
                let n: usize = n.parse().map_err(|_| format!("invalid frame count: {n}"))?;
                gen_out = Some((PathBuf::from(out), n));
                i += 3;
            }
            "--selftest" => {
                let f = args.get(i + 1).ok_or("--selftest needs an SVG path")?;
                selftest = Some(PathBuf::from(f));
                i += 2;
            }
            "--bench" => {
                let f = args.get(i + 1).ok_or("--bench needs an SVG path")?;
                let iters = match args.get(i + 2).and_then(|s| s.parse::<u32>().ok()) {
                    Some(n) => {
                        i += 3;
                        n
                    }
                    None => {
                        i += 2;
                        400
                    }
                };
                bench = Some((PathBuf::from(f), iters));
            }
            "--serial" => {
                svg::force_serial(true);
                i += 1;
            }
            "--chunks" => {
                chunks = true;
                i += 1;
            }
            "--smoke" => {
                let f = args.get(i + 1).ok_or("--smoke needs an SVG path")?;
                let frames = args.get(i + 2).and_then(|s| s.parse::<u32>().ok());
                match frames {
                    Some(_) => i += 3,
                    None => i += 2,
                }
                smoke = Some((PathBuf::from(f), frames.unwrap_or(240)));
            }
            other if other.starts_with("--") => return Err(format!("unknown option {other}")),
            other => {
                files.push(PathBuf::from(other));
                i += 1;
            }
        }
    }

    if let Some((out, frames)) = gen_out {
        return Ok(Mode::Gen { out, frames });
    }
    if let Some((file, iters)) = bench {
        return Ok(Mode::SelfTest {
            file,
            bench: true,
            iters,
            chunks,
        });
    }
    if let Some(file) = selftest {
        return Ok(Mode::SelfTest {
            file,
            bench: false,
            iters: 400,
            chunks,
        });
    }
    if let Some((file, frames)) = smoke {
        return Ok(Mode::Smoke { file, frames });
    }
    Ok(Mode::Gui(files))
}

fn print_help() {
    println!(
        "FlameReader {} - fast SVG flame graph viewer

USAGE:
    flamereader [OPTIONS] [FILE.svg ...]

OPTIONS:
    --gen <out.svg> <frames>   write a synthetic flame graph and exit
    --selftest <file.svg>      parse, verify and print statistics, then exit
    --bench <file.svg> [n]     measure culling and mesh build cost, then exit
    --smoke <file.svg> [n]     render n frames headfully, print fps, then exit
    -h, --help                 show this help
    -V, --version              show the version

INTERACTION:
    wheel zoom at cursor | drag pan | double click focus
    right click fit | Ctrl+O open | Ctrl+W close tab
    Ctrl+F details | n/p next/previous search hit | F1 shortcuts",
        env!("CARGO_PKG_VERSION")
    );
}

fn run_gui(files: Vec<PathBuf>, smoke: Option<u32>) -> ExitCode {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FlameReader")
            .with_inner_size([1500.0, 900.0])
            .with_min_inner_size([820.0, 520.0]),
        ..Default::default()
    };

    let result = eframe::run_native(
        "FlameReader",
        options,
        Box::new(move |cc| {
            let mut a = app::FlameApp::new(cc, files);
            if let Some(n) = smoke {
                a = a.with_smoke(n);
            }
            Ok(Box::new(a))
        }),
    );

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_selftest(file: &Path, bench: bool, iters: u32, verbose_chunks: bool) -> ExitCode {
    let meta = match std::fs::metadata(file) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: cannot stat {}: {e}", file.display());
            return ExitCode::FAILURE;
        }
    };

    println!("FlameReader self test");
    println!("  file        : {}", file.display());
    println!("  size        : {:.2} MB", meta.len() as f64 / 1e6);

    let progress = Progress::new();
    let cancel = AtomicBool::new(false);
    let t0 = Instant::now();
    let scene = match loader::load(file, &progress, &cancel) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("  FAILED: {e}");
            return ExitCode::FAILURE;
        }
    };
    let wall = t0.elapsed().as_secs_f64() * 1000.0;

    let mbs = meta.len() as f64 / 1e6 / (wall / 1000.0).max(1e-9);
    println!("  frames      : {}", app::fmt_int(scene.len()));
    println!("  unique names: {}", app::fmt_int(scene.names.len()));
    println!("  rows        : {}", scene.levels.len());
    println!("  graph width : {:.1}", scene.width());
    if scene.declared[0] > 0.0 {
        println!(
            "  svg canvas  : {:.0} x {:.0}",
            scene.declared[0], scene.declared[1]
        );
    }
    println!("  parse       : {:.0} ms total ({:.0} MB/s)", wall, mbs);
    println!(
        "  scene memory: {:.1} MB",
        scene.approx_bytes() as f64 / 1e6
    );
    let report = svg::chunk_report();
    let empty: usize = report.iter().map(|c| c.empty).sum();
    if empty > 0 {
        println!(
            "  zero size   : {} rects skipped (they have no pixels in the source)",
            app::fmt_int(empty)
        );
    }
    if verbose_chunks {
        println!("  chunks      : {}", report.len());
        for (i, c) in report.iter().enumerate() {
            println!(
                "    #{i:<3} off {:>12}  {:>9.1} MB -> {} frames ({} zero size)",
                c.start,
                c.bytes as f64 / 1e6,
                app::fmt_int(c.frames),
                app::fmt_int(c.empty)
            );
        }
    }

    let problems = sanity_checks(&scene);
    if problems.is_empty() {
        println!("  checks      : PASS");
    } else {
        println!("  checks      : FAIL");
        for p in &problems {
            println!("    - {p}");
        }
    }

    let outcome = if bench {
        bench_geometry(&scene, iters)
    } else {
        quick_geometry(&scene)
    };
    println!("{outcome}");

    if problems.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Verify the invariants the renderer relies on.
fn sanity_checks(scene: &Scene) -> Vec<String> {
    let mut problems = Vec::new();
    if scene.frames.is_empty() {
        problems.push("no frames were parsed".to_string());
        return problems;
    }
    if scene.levels.is_empty() {
        problems.push("no rows were built".to_string());
    }

    let mut unsorted = 0u64;
    let mut overlap = 0u64;
    let mut rounded = 0u64;
    let mut worst_overlap = 0.0f64;
    let mut bad_names = 0u64;
    let mut nonfinite = 0u64;
    // A visually meaningful overlap, as opposed to a stored-coordinate rounding
    // step on a hair-thin frame.
    let significant = ((scene.max_x - scene.min_x) * 0.002).max(0.01);
    #[allow(clippy::type_complexity)]
    let mut worst_pair: (
        f64,
        (f64, f64, f64),
        (f64, f64, f64),
        (usize, usize),
        Vec<(f64, f64)>,
    ) = (0.0, (0.0, 0.0, 0.0), (0.0, 0.0, 0.0), (0, 0), Vec::new());

    for (li, l) in scene.levels.iter().enumerate() {
        let s = &scene.frames[l.frames.start as usize..l.frames.end as usize];
        if s.is_empty() {
            problems.push(format!("row {li} is empty"));
        }
        for k in 0..s.len().saturating_sub(1) {
            let (a, b) = (&s[k], &s[k + 1]);
            if b.x < a.x {
                unsorted += 1;
                continue;
            }
            // SVG stores coordinates with a few decimals, so neighbours in a
            // dense row can overlap by a rounding step. Only a real overlap
            // (a meaningful fraction of the narrower frame) is a problem.
            let over = a.x + a.w - b.x;
            if over <= 0.0 {
                continue;
            }
            worst_overlap = worst_overlap.max(over);
            if over > significant && over > a.w.min(b.w) * 0.25 {
                overlap += 1;
            } else {
                rounded += 1;
            }
            if over > worst_pair.0 {
                let lo = k.saturating_sub(3);
                let hi = (k + 5).min(s.len());
                let around: Vec<(f64, f64)> = s[lo..hi].iter().map(|f| (f.x, f.w)).collect();
                worst_pair = (
                    over,
                    (a.x, a.w, a.y as f64),
                    (b.x, b.w, b.y as f64),
                    (li, k),
                    around,
                );
            }
        }
    }
    for f in &scene.frames {
        if f.name as usize >= scene.names.len() {
            bad_names += 1;
        }
        if !(f.x.is_finite() && f.y.is_finite() && f.w.is_finite() && f.h.is_finite()) {
            nonfinite += 1;
        }
    }
    if unsorted > 0 {
        problems.push(format!("{unsorted} frames are out of order inside a row"));
    }
    if overlap > 0 {
        problems.push(format!(
            "{overlap} frames overlap significantly inside a row"
        ));
    }
    if rounded > 0 {
        println!("  note        : {rounded} pairs overlap slightly (max {worst_overlap:.4} units)");
    }
    if worst_pair.0 > 0.0 && std::env::var("FLAMEREADER_DIAG").is_ok() {
        println!("  worst pair  : over {:.4}", worst_pair.0);
        println!("    a (x,w,y) : {:?}", worst_pair.1);
        println!("    b (x,w,y) : {:?}", worst_pair.2);
        println!(
            "    level {:<3} index {:<6}",
            worst_pair.3 .0, worst_pair.3 .1
        );
        println!("    neighbourhood:");
        for (i, (x, w)) in worst_pair.4.iter().enumerate() {
            println!(
                "      {} x={x:<14.4} w={w:<12.4} end={:.4}",
                i as i32 - 3,
                x + w
            );
        }
    }
    if bad_names > 0 {
        problems.push(format!("{bad_names} frames reference an unknown name"));
    }
    if nonfinite > 0 {
        problems.push(format!("{nonfinite} frames have non finite geometry"));
    }

    // Hit testing must land on the frame we aimed at.
    if let Some((_, widest)) = scene
        .frames
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.w.total_cmp(&b.1.w))
    {
        let probe = scene.frame_at(widest.x + widest.w * 0.5, widest.y + widest.h * 0.5);
        match probe {
            Some(h) if scene.frames[h as usize].name == widest.name => {}
            other => problems.push(format!(
                "hit test mismatch: expected a frame, got {other:?}"
            )),
        }
    }

    let rows = scene.levels.len() as u64;
    if rows > 0 {
        println!(
            "  density     : {:.1} frames per row on average",
            scene.frames.len() as f64 / rows as f64
        );
    }
    problems
}

fn make_view(scene: &Scene) -> (FlameView, ScreenRect) {
    // Deliberately offset: a real viewport sits below a toolbar, and code that
    // forgets that draws the whole graph off screen.
    let view = ScreenRect::new(1.0, 64.0, 1599.0, 836.0);
    let mut fv = FlameView::default();
    fv.budget = DEFAULT_BUDGET;
    // The readable fit, not the exact one: the report is about what the viewer
    // actually shows when a document is opened.
    fv.vp = lod::fit(scene, view, 14.0, lod::Bounds::scene(scene), false, false);
    // Warm up so palette colour tables are not charged to the measurement.
    fv.build_geometry(scene, view);
    (fv, view)
}

fn quick_geometry(scene: &Scene) -> String {
    let (mut fv, view) = make_view(scene);
    let stats = fv.build_geometry(scene, view);
    format!(
        "  fit view    : {} visible frames -> {} quads in {:.2} ms",
        app::fmt_int(stats.frames_considered as usize),
        app::fmt_int(stats.quads as usize),
        fv.build_us / 1000.0
    )
}

/// Sweep the zoom range and report culling + mesh build cost per frame.
fn bench_geometry(scene: &Scene, iters: u32) -> String {
    let (mut fv, view) = make_view(scene);
    let iters = iters.max(1);
    let mut samples: Vec<f64> = Vec::with_capacity(iters as usize);
    let mut quad_counts: Vec<u32> = Vec::with_capacity(iters as usize);
    let mut worst_visible = 0u64;

    // Sweep from "the whole graph fits" to 200x that, always keeping the graph
    // centre in the middle of the viewport so the measurement covers real
    // content at every step.
    let base = ((view.w as f64) / scene.width().max(1e-9))
        .min((view.h as f64) / (scene.height() as f64).max(1e-9))
        .max(1e-12);
    let cx = (scene.min_x + scene.max_x) * 0.5;
    let cy = ((scene.min_y + scene.max_y) * 0.5) as f64;

    for i in 0..iters {
        let t = i as f32 / iters as f32;
        let factor = 1.0 + (t * std::f32::consts::TAU).sin().abs() * 200.0;
        let scale = base * factor as f64;
        fv.vp = lod::Viewport {
            scale,
            pan_x: (view.w as f64) * 0.5 / scale - cx,
            pan_y: (view.h as f64) * 0.5 / scale - cy,
            flip_y: i % 2 == 1,
        };
        let stats = fv.build_geometry(scene, view);
        samples.push(fv.build_us / 1000.0);
        quad_counts.push(stats.quads);
        worst_visible = worst_visible.max(stats.frames_considered);
        assert_eq!(
            stats.dropped, 0,
            "geometry budget was exceeded at iteration {i}"
        );
    }

    let mut sorted = samples.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let mean = sorted.iter().sum::<f64>() / sorted.len() as f64;
    let pick = |q: f64| sorted[((sorted.len() as f64 - 1.0) * q) as usize];
    let max_quads = quad_counts.iter().copied().max().unwrap_or(0);

    format!(
        "  geometry    : {} iterations across the zoom range
                    cull + mesh build  mean {:.2} ms | p50 {:.2} ms | p95 {:.2} ms | max {:.2} ms
                    peak visible       {} frames
                    peak quads         {} (budget {})",
        app::fmt_int(iters as usize),
        mean,
        pick(0.50),
        pick(0.95),
        sorted[sorted.len() - 1],
        app::fmt_int(worst_visible as usize),
        app::fmt_int(max_quads as usize),
        app::fmt_int(DEFAULT_BUDGET)
    )
}
