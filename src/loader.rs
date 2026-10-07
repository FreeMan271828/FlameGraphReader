//! Background document loading.
//!
//! Files are mmapped (never copied) and parsed on a worker thread while the UI
//! stays responsive. Progress is published through a lock-free Progress cell and
//! work can be cancelled at any point.

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;

use memmap2::Mmap;

use crate::progress::{Progress, Stage};
use crate::scene::Scene;
use crate::svg;

pub enum LoadMsg {
    Finished { path: PathBuf, scene: Box<Scene> },
    Failed { path: PathBuf, error: String },
}

/// Owns the channel that worker threads report on.
pub struct Loader {
    tx: Sender<LoadMsg>,
    rx: Receiver<LoadMsg>,
}

impl Default for Loader {
    fn default() -> Self {
        Self::new()
    }
}

impl Loader {
    pub fn new() -> Loader {
        let (tx, rx) = channel();
        Loader { tx, rx }
    }

    /// Kick off a load. Returns immediately; poll() delivers the result.
    pub fn spawn(&self, path: PathBuf, progress: Arc<Progress>, cancel: Arc<AtomicBool>) {
        let tx = self.tx.clone();
        let name = path.clone();
        thread::Builder::new()
            .name("flamereader-load".into())
            .spawn(move || {
                let result = load(&path, &progress, &cancel);
                let msg = match result {
                    Ok(scene) => LoadMsg::Finished {
                        path,
                        scene: Box::new(scene),
                    },
                    Err(e) => {
                        progress.set_stage(Stage::Failed);
                        LoadMsg::Failed {
                            path: name,
                            error: e,
                        }
                    }
                };
                let _ = tx.send(msg);
            })
            .expect("spawn loader thread");
    }

    /// Drain finished loads without blocking.
    pub fn poll(&self) -> Vec<LoadMsg> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(m) => out.push(m),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        out
    }
}

/// Load and parse a document synchronously. Also used by the CLI modes.
pub fn load(path: &PathBuf, progress: &Progress, cancel: &AtomicBool) -> Result<Scene, String> {
    let t0 = std::time::Instant::now();
    progress.set_stage(Stage::Reading);

    let file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("stat {}: {e}", path.display()))?;
    let file_len = meta.len();
    progress.set_total(file_len);

    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }

    // Peak at the first two bytes to decide whether this is gzip.
    let mut magic = [0u8; 2];
    let is_gzip = {
        let mut head = &file;
        use std::io::Read as _;
        let n = head.read(&mut magic).unwrap_or(0);
        n == 2 && magic == [0x1f, 0x8b]
    };

    // The bytes must outlive parsing; each branch keeps its own owner alive.
    let parse_result = if is_gzip {
        progress.set_stage(Stage::Reading);
        let mut decoder =
            flate2::read::MultiGzDecoder::new(File::open(path).map_err(|e| e.to_string())?);
        let mut buf: Vec<u8> = Vec::with_capacity((file_len * 4).min(1 << 31) as usize);
        let mut chunk = vec![0u8; 1 << 20];
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err("cancelled".into());
            }
            let n = decoder
                .read(&mut chunk)
                .map_err(|e| format!("gunzip: {e}"))?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        progress.set_total(buf.len() as u64);
        progress.set_done(0);
        let parsed = svg::parse(&buf, progress);
        let elapsed = t0.elapsed().as_secs_f64() * 1000.0;
        finish(path, parsed, buf.len() as u64, elapsed)
    } else {
        // SAFETY: the mapping is read-only and stays alive for the whole parse.
        let mmap =
            unsafe { Mmap::map(&file).map_err(|e| format!("mmap {}: {e}", path.display()))? };
        progress.set_total(mmap.len() as u64);
        progress.set_done(0);
        let parsed = svg::parse(&mmap, progress);
        let elapsed = t0.elapsed().as_secs_f64() * 1000.0;
        finish(path, parsed, mmap.len() as u64, elapsed)
    };

    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    progress.set_stage(Stage::Done);
    Ok(parse_result)
}

fn finish(path: &PathBuf, parsed: svg::ParsedSvg, bytes: u64, elapsed_ms: f64) -> Scene {
    let title = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "flame graph".into());
    Scene::build(
        parsed.frames,
        parsed.names,
        title,
        path.display().to_string(),
        bytes,
        elapsed_ms,
        [parsed.width, parsed.height],
    )
}
