//! Synthetic flame graph generator.
//!
//! Emits an SVG shaped exactly like flamegraph.pl / inferno output, which makes
//! it easy to build realistically sized inputs for benchmarking without any
//! external tooling:
//!
//!     flamereader --gen big.svg 1000000

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use crate::palette::Palette;

/// Deterministic xorshift so repeated runs produce byte-identical files.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }

    fn unit(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64) / ((1u64 << 53) as f64)
    }
}

const POOL: usize = 4096;

fn name_pool() -> Vec<String> {
    const VERBS: [&str; 10] = [
        "alloc", "parse", "render", "io", "gc", "hash", "sort", "copy", "lock", "walk",
    ];
    const NOUNS: [&str; 10] = [
        "buffer", "node", "entry", "frame", "shard", "task", "index", "batch", "chunk", "ctx",
    ];
    (0..POOL)
        .map(|i| format!("{}::{}_{}", VERBS[i % 10], NOUNS[(i / 10) % 10], i))
        .collect()
}

pub struct GenReport {
    pub frames: usize,
    pub rows: u32,
    pub bytes: u64,
    pub millis: f64,
}

/// Write a synthetic flame graph with approximately target frames.
pub fn generate(path: &Path, target: usize) -> std::io::Result<GenReport> {
    let t0 = std::time::Instant::now();
    let target = target.max(1);
    let pool = name_pool();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

    let mut parent: Vec<u32> = vec![u32::MAX];
    let mut depth: Vec<u32> = vec![0];
    let mut first_child: Vec<u32> = vec![u32::MAX];
    let mut next_sib: Vec<u32> = vec![u32::MAX];
    let mut name: Vec<u32> = vec![0];
    let mut own: Vec<f64> = vec![1.0];

    // Random recursive tree: a pool of nodes that can still take children,
    // picked mostly uniformly (which yields the depth growth real profiles
    // show) and partly from the most recent nodes (which keeps local stacks
    // deep instead of bushy).
    let mut open: Vec<u32> = vec![0];
    let mut recent: VecDeque<u32> = VecDeque::new();
    recent.push_back(0);
    let mut count: usize = 1;
    const MAX_DEPTH: u32 = 64;
    const BRANCH: u64 = 4;

    while count < target {
        if open.is_empty() {
            break;
        }
        let p = if !recent.is_empty() && rng.below(3) == 0 {
            recent[rng.below(recent.len() as u64) as usize]
        } else {
            open[rng.below(open.len() as u64) as usize]
        };
        let k = 1 + rng.below(BRANCH);
        for _ in 0..k {
            if count >= target {
                break;
            }
            let idx = count as u32;
            parent.push(p);
            let d = depth[p as usize] + 1;
            depth.push(d);
            first_child.push(u32::MAX);
            next_sib.push(u32::MAX);
            name.push(rng.below(POOL as u64) as u32);
            own.push(0.2 + rng.unit() * 2.0);
            next_sib[idx as usize] = first_child[p as usize];
            first_child[p as usize] = idx;
            if d < MAX_DEPTH {
                open.push(idx);
                recent.push_back(idx);
                if recent.len() > 64 {
                    recent.pop_front();
                }
            }
            count += 1;
        }
        if count.is_multiple_of(500_000) && count > 0 {
            eprint!("\r  generated {count} nodes...");
        }
    }
    let n = count;
    let max_depth = depth.iter().copied().max().unwrap_or(0);
    eprint!("\r  generated {n} nodes, laying out...    \n");

    // Inclusive sample counts, bottom up.
    let mut incl = own.clone();
    for i in (1..n).rev() {
        let p = parent[i] as usize;
        incl[p] += incl[i];
    }

    // Top down x assignment: children share their parent's span in proportion
    // to their inclusive counts, leaving the parent's self time on the right.
    let mut x = vec![0.0f64; n];
    let mut w = vec![0.0f64; n];
    w[0] = incl[0];
    for i in 0..n {
        let mut cursor = x[i];
        let span = w[i];
        let total = incl[i].max(1e-6);
        let mut c = first_child[i];
        while c != u32::MAX {
            let cu = c as usize;
            let cw = span * incl[cu] / total;
            x[cu] = cursor;
            w[cu] = cw;
            cursor += cw;
            c = next_sib[cu];
        }
    }

    // Real generators widen the canvas for large profiles. Without that, five
    // million frames compressed into 1200 units would round to zero-width rects
    // and the data itself would be lossy.
    let width: f64 = 1200.0f64.max(n as f64 / 4.0).ceil();
    const ROW: f64 = 16.0;
    let height = (max_depth as f64 + 1.0) * ROW;
    let scale = width / incl[0].max(1e-6);

    let file = File::create(path)?;
    let mut out = BufWriter::with_capacity(1 << 22, file);
    writeln!(out, "<?xml version=\"1.0\" standalone=\"no\"?>")?;
    writeln!(
        out,
        "<!DOCTYPE svg PUBLIC \"-//W3C//DTD SVG 1.1//EN\" \"http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd\">"
    )?;
    writeln!(
        out,
        "<svg version=\"1.1\" width=\"{width:.0}\" height=\"{height:.0}\" viewBox=\"0 0 {width:.0} {height:.0}\" xmlns=\"http://www.w3.org/2000/svg\">"
    )?;
    // A plain background rect with no title, exactly like inferno emits.
    writeln!(
        out,
        "<rect x=\"0\" y=\"0\" width=\"{width:.0}\" height=\"{height:.0}\" fill=\"#eeeeee\"/>"
    )?;

    let total_incl = incl[0].max(1e-6);
    for i in 0..n {
        let nm = &pool[name[i] as usize];
        let y = depth[i] as f64 * ROW;
        let gx = x[i] * scale;
        let gw = w[i] * scale;
        let pct = incl[i] / total_incl * 100.0;
        let c = Palette::Hot.color(nm, 0);
        let (r, g, b) = ((c >> 16) & 0xff, (c >> 8) & 0xff, c & 0xff);
        writeln!(
            out,
            "<g><title>{nm} ({:.0} samples, {pct:.2}%)</title><rect x=\"{gx:.4}\" y=\"{y:.1}\" width=\"{gw:.4}\" height=\"15\" fill=\"rgb({r},{g},{b})\" rx=\"2\" ry=\"2\"/><text x=\"{:.4}\" y=\"{:.1}\">{nm}</text></g>",
            incl[i],
            gx + (gw * 0.05).min(4.0),
            y + 12.0
        )?;
    }
    writeln!(out, "</svg>")?;
    out.flush()?;
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    Ok(GenReport {
        frames: n,
        rows: max_depth + 1,
        bytes,
        millis: t0.elapsed().as_secs_f64() * 1000.0,
    })
}
