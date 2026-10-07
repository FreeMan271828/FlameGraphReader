//! Flat, cache-friendly scene representation of a flame graph.
//!
//! Everything is stored in one contiguous Vec of 24-byte frames sorted by
//! (y, x), plus a small level index. That lets the renderer cull to the
//! viewport with two binary searches per visible row instead of touching
//! millions of nodes, which is what keeps pan/zoom interactive.

use std::ops::Range;

/// One flame graph node as an axis-aligned rectangle in graph coordinates.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Frame {
    /// Graph space x. f64 because a multi-million frame graph has frames whose
    /// widths are below f32's relative resolution.
    pub x: f64,
    pub w: f64,
    pub y: f32,
    pub h: f32,
    /// 0xRRGGBB, straight from the SVG fill (or a palette).
    pub color: u32,
    /// Index into Scene::names.
    pub name: u32,
}

impl Default for Frame {
    fn default() -> Self {
        Frame {
            x: 0.0,
            w: 0.0,
            y: 0.0,
            h: 0.0,
            color: 0xdd_dd_dd,
            name: 0,
        }
    }
}

/// One horizontal band of the graph (all frames sharing a y coordinate).
#[derive(Clone, Debug)]
pub struct Level {
    pub y: f32,
    pub h: f32,
    /// Range into Scene::frames, sorted by x and non-overlapping.
    pub frames: Range<u32>,
}

/// An immutable, fully indexed flame graph.
pub struct Scene {
    pub frames: Vec<Frame>,
    pub levels: Vec<Level>,
    pub names: Vec<String>,
    pub title: String,
    pub source: String,
    pub min_x: f64,
    pub max_x: f64,
    pub min_y: f32,
    pub max_y: f32,
    pub bytes_parsed: u64,
    pub parse_ms: f64,
    /// Canvas size declared by the svg element, if it had one.
    pub declared: [f32; 2],
    /// True when the widest row (the profile root) sits at the top of the
    /// document. The viewer uses this to offer the browser convention, where
    /// the longest row is at the bottom, without inverting already flipped
    /// input.
    pub root_at_top: bool,
}

impl Scene {
    /// Sort frames, build the level index and compute the bounding box.
    pub fn build(
        mut frames: Vec<Frame>,
        names: Vec<String>,
        title: String,
        source: String,
        bytes_parsed: u64,
        parse_ms: f64,
        declared: [f32; 2],
    ) -> Scene {
        // Sorting by (y, x) is what makes the per-row binary searches valid.
        frames.sort_unstable_by(|a, b| a.y.total_cmp(&b.y).then_with(|| a.x.total_cmp(&b.x)));

        let mut levels: Vec<Level> = Vec::with_capacity(64);
        let mut i = 0usize;
        while i < frames.len() {
            let y = frames[i].y;
            let h = frames[i].h;
            let eps = if h > 0.0 { h * 0.5 } else { 0.0 };
            let start = i;
            i += 1;
            while i < frames.len() && (frames[i].y - y).abs() <= eps {
                i += 1;
            }
            levels.push(Level {
                y,
                h,
                frames: start as u32..i as u32,
            });
        }

        let (mut min_x, mut max_x) = (f64::INFINITY, f64::NEG_INFINITY);
        let (mut min_y, mut max_y) = (f32::INFINITY, f32::NEG_INFINITY);
        for f in &frames {
            if f.x < min_x {
                min_x = f.x;
            }
            if f.x + f.w > max_x {
                max_x = f.x + f.w;
            }
            if f.y < min_y {
                min_y = f.y;
            }
            if f.y + f.h > max_y {
                max_y = f.y + f.h;
            }
        }
        if frames.is_empty() {
            min_x = 0.0;
            max_x = 1.0;
            min_y = 0.0;
            max_y = 1.0;
        }

        // The widest frame is the root of the call tree. Which edge it sits on
        // decides whether the view has to be mirrored to match the browser
        // convention of "longest row at the bottom".
        let mut widest = 0usize;
        for (i, f) in frames.iter().enumerate() {
            if f.w > frames[widest].w {
                widest = i;
            }
        }
        let root_at_top = frames.is_empty() || frames[widest].y <= (min_y + max_y) * 0.5;

        Scene {
            frames,
            levels,
            names,
            title,
            source,
            min_x,
            max_x,
            min_y,
            max_y,
            bytes_parsed,
            parse_ms,
            declared,
            root_at_top,
        }
    }

    /// An empty scene, used as a placeholder while a document loads.
    pub fn empty() -> Scene {
        Scene {
            frames: Vec::new(),
            levels: Vec::new(),
            names: Vec::new(),
            title: String::new(),
            source: String::new(),
            min_x: 0.0,
            max_x: 1.0,
            min_y: 0.0,
            max_y: 1.0,
            bytes_parsed: 0,
            parse_ms: 0.0,
            declared: [0.0, 0.0],
            root_at_top: true,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    #[inline]
    pub fn name_of(&self, frame: u32) -> &str {
        match self.names.get(self.frames[frame as usize].name as usize) {
            Some(s) => s.as_str(),
            None => "?",
        }
    }

    #[inline]
    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }

    #[inline]
    pub fn height(&self) -> f32 {
        self.max_y - self.min_y
    }

    /// Levels whose vertical extent intersects [y0, y1].
    pub fn levels_in_range(&self, y0: f32, y1: f32) -> Range<usize> {
        let lo = self.levels.partition_point(|l| l.y + l.h < y0);
        let hi = self.levels.partition_point(|l| l.y <= y1);
        lo..hi.max(lo)
    }

    /// Indices of frames in one level whose horizontal extent intersects [x0, x1].
    ///
    /// Two binary searches, so this is O(log n) regardless of graph size.
    pub fn frames_in_level(&self, level: usize, x0: f64, x1: f64) -> Range<usize> {
        let l = &self.levels[level];
        let s = &self.frames[l.frames.start as usize..l.frames.end as usize];
        let lo = s.partition_point(|f| f.x + f.w < x0);
        let hi = s.partition_point(|f| f.x <= x1);
        let base = l.frames.start as usize;
        base + lo..base + hi.max(lo)
    }

    /// Hit test in graph coordinates.
    pub fn frame_at(&self, x: f64, y: f32) -> Option<u32> {
        let li = self.levels.partition_point(|l| l.y <= y);
        if li == 0 {
            return None;
        }
        let level = li - 1;
        let l = &self.levels[level];
        if y > l.y + l.h {
            return None;
        }
        let r = self.frames_in_level(level, x, x);
        if r.is_empty() {
            return None;
        }
        // A hit exactly on a boundary can match two frames; the deeper/first wins.
        let idx = self.frames[r.start..r.end]
            .iter()
            .position(|f| x >= f.x && x <= f.x + f.w)
            .map(|p| (r.start + p) as u32)?;
        Some(idx)
    }

    /// The level index that contains a frame.
    pub fn level_of(&self, frame: u32) -> usize {
        // levels are sorted and frames are grouped, so a binary search works.
        let f = self.frames[frame as usize];
        let li = self.levels.partition_point(|l| l.y <= f.y);
        li.saturating_sub(1)
    }

    /// Direct children of a frame (frames in the next level fully inside it).
    pub fn children(&self, frame: u32) -> Range<usize> {
        let level = self.level_of(frame);
        if level + 1 >= self.levels.len() {
            return 0..0;
        }
        let f = self.frames[frame as usize];
        // Children are the frames of the next level within [x, x + w].
        self.frames_in_level(level + 1, f.x, f.x + f.w)
    }

    /// Inclusive (self) width: the frame's width minus its children's widths.
    pub fn self_width(&self, frame: u32) -> f64 {
        let f = self.frames[frame as usize];
        let mut acc = 0.0f64;
        for c in self.children(frame) {
            acc += self.frames[c].w;
        }
        (f.w - acc).max(0.0)
    }

    /// Total (inclusive) width, i.e. the frame width itself.
    pub fn total_width(&self, frame: u32) -> f64 {
        self.frames[frame as usize].w
    }

    /// Ancestor chain from the root down to (and including) the frame.
    pub fn ancestry(&self, frame: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut level = self.level_of(frame);
        let mut cur = frame;
        out.push(cur);
        while level > 0 {
            let f = self.frames[cur as usize];
            let mid_x = f.x + f.w * 0.5;
            let parent = {
                let r = self.frames_in_level(level - 1, mid_x, mid_x);
                r.clone().find(|&i| self.frames[i].w > 0.0)
            };
            match parent {
                Some(p) => {
                    cur = p as u32;
                    out.push(cur);
                    level -= 1;
                }
                None => break,
            }
        }
        out.reverse();
        out
    }

    /// Approximate resident size of the scene in bytes.
    pub fn approx_bytes(&self) -> u64 {
        let frames = self.frames.len() as u64 * std::mem::size_of::<Frame>() as u64;
        let levels = self.levels.len() as u64 * std::mem::size_of::<Level>() as u64;
        let names: u64 = self.names.iter().map(|n| n.len() as u64 + 24 + 8).sum();
        frames + levels + names
    }
}
