//! Substring search across every frame name in a scene.

use rayon::prelude::*;

use crate::scene::Scene;

/// Maximum number of hits kept (ranked by inclusive width).
pub const DEFAULT_LIMIT: usize = 20_000;

pub struct Outcome {
    /// Frame indices, widest first.
    pub hits: Vec<u32>,
    /// Total number of matching frames before truncation.
    pub total: usize,
    pub elapsed_ms: f64,
}

impl Outcome {
    pub fn empty() -> Outcome {
        Outcome {
            hits: Vec::new(),
            total: 0,
            elapsed_ms: 0.0,
        }
    }
}

/// Find every frame whose name contains the query.
///
/// Names are de-duplicated in the scene, so the substring test runs once per
/// unique name and the frame walk is a single cache-friendly pass over bools.
pub fn run(scene: &Scene, query: &str, case_sensitive: bool, limit: usize) -> Outcome {
    let t0 = std::time::Instant::now();
    if query.is_empty() {
        return Outcome::empty();
    }

    let matched: Vec<bool> = scene
        .names
        .iter()
        .map(|n| {
            if case_sensitive {
                n.contains(query)
            } else {
                n.to_lowercase().contains(&query.to_lowercase())
            }
        })
        .collect();

    let mut hits: Vec<u32> = scene
        .frames
        .par_iter()
        .enumerate()
        .filter_map(|(i, f)| {
            if matched[f.name as usize] {
                Some(i as u32)
            } else {
                None
            }
        })
        .collect();

    let total = hits.len();
    if total > limit {
        // Keep the widest frames: those are the ones a user is looking for.
        let frames = &scene.frames;
        hits.select_nth_unstable_by(limit, |&a, &b| {
            frames[b as usize].w.total_cmp(&frames[a as usize].w)
        });
        hits.truncate(limit);
    }
    let frames = &scene.frames;
    hits.sort_unstable_by(|&a, &b| frames[b as usize].w.total_cmp(&frames[a as usize].w));

    Outcome {
        hits,
        total,
        elapsed_ms: t0.elapsed().as_secs_f64() * 1000.0,
    }
}
