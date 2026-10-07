//! Lock-free progress reporting shared between the loader thread and the UI.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Stage {
    Idle = 0,
    Reading = 1,
    Parsing = 2,
    Indexing = 3,
    Done = 4,
    Failed = 5,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Idle => "idle",
            Stage::Reading => "reading",
            Stage::Parsing => "parsing",
            Stage::Indexing => "indexing",
            Stage::Done => "done",
            Stage::Failed => "failed",
        }
    }

    fn from_u8(v: u8) -> Stage {
        match v {
            1 => Stage::Reading,
            2 => Stage::Parsing,
            3 => Stage::Indexing,
            4 => Stage::Done,
            5 => Stage::Failed,
            _ => Stage::Idle,
        }
    }
}

/// A cheap, lock-free progress channel.
#[derive(Debug, Default)]
pub struct Progress {
    stage: AtomicU8,
    done: AtomicU64,
    total: AtomicU64,
    frames: AtomicU64,
}

impl Progress {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_stage(&self, stage: Stage) {
        self.stage.store(stage as u8, Ordering::Relaxed);
    }

    pub fn stage(&self) -> Stage {
        Stage::from_u8(self.stage.load(Ordering::Relaxed))
    }

    pub fn set_total(&self, total: u64) {
        self.total.store(total, Ordering::Relaxed);
    }

    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// Advance the byte counter. Safe to call from many threads at once.
    pub fn advance(&self, n: u64) {
        self.done.fetch_add(n, Ordering::Relaxed);
    }

    pub fn set_done(&self, n: u64) {
        self.done.store(n, Ordering::Relaxed);
    }

    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }

    pub fn set_frames(&self, n: u64) {
        self.frames.store(n, Ordering::Relaxed);
    }

    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    pub fn fraction(&self) -> f32 {
        let total = self.total();
        if total == 0 {
            return 0.0;
        }
        (self.done() as f64 / total as f64).clamp(0.0, 1.0) as f32
    }
}
