//! A fast, zero-copy SVG flame graph reader.
//!
//! The parser is deliberately specialised: flame graph SVGs are machine
//! generated and extremely regular (a title, a rect and a text per frame), so a
//! hand written byte scanner beats a general XML DOM by a wide margin.
//!
//! Two execution strategies are used:
//!
//! * When the document contains no transform attributes (true for flamegraph.pl
//!   and inferno output) the buffer is split at element boundaries and the
//!   pieces are parsed in parallel with rayon.
//! * Otherwise a single sequential pass with incremental progress is used.
//!
//! Either way the input is never copied: callers hand over an mmap.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};

use memchr::{memchr, memmem};
use rayon::prelude::*;

use crate::palette;
use crate::progress::{Progress, Stage};
use crate::scene::Frame;

/// Colour used when a rect has no usable fill.
const DEFAULT_FILL: u32 = 0xd6_da_e0;

/// Below this size the parallel split is not worth its overhead.
const PARALLEL_THRESHOLD: usize = 4 << 20;

/// Set to true to disable the parallel split (useful for diagnosing files that
/// rely on inherited state, and as a fallback).
static FORCE_SERIAL: AtomicBool = AtomicBool::new(false);

pub fn force_serial(on: bool) {
    FORCE_SERIAL.store(on, Ordering::Relaxed);
}

/// One parsed work unit: byte range and how many frames came out of it.
#[derive(Clone, Copy, Debug)]
pub struct ChunkStat {
    pub start: usize,
    pub bytes: usize,
    pub frames: usize,
    /// Rects skipped because the document gave them zero width or height.
    pub empty: usize,
}

/// Diagnostics for the most recent parse, used by the CLI.
static LAST_CHUNKS: std::sync::Mutex<Vec<ChunkStat>> = std::sync::Mutex::new(Vec::new());

pub fn chunk_report() -> Vec<ChunkStat> {
    LAST_CHUNKS.lock().map(|g| g.clone()).unwrap_or_default()
}

/// The result of parsing one SVG document.
pub struct ParsedSvg {
    pub frames: Vec<Frame>,
    pub names: Vec<String>,
    pub width: f32,
    pub height: f32,
}

// ---------------------------------------------------------------------------
// 2x3 affine transform
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Affine {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl Affine {
    const IDENTITY: Affine = Affine {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    #[inline]
    fn is_identity(&self) -> bool {
        self.a == 1.0
            && self.b == 0.0
            && self.c == 0.0
            && self.d == 1.0
            && self.e == 0.0
            && self.f == 0.0
    }

    /// self * other: other is applied first.
    fn concat(&self, o: &Affine) -> Affine {
        Affine {
            a: self.a * o.a + self.c * o.b,
            b: self.b * o.a + self.d * o.b,
            c: self.a * o.c + self.c * o.d,
            d: self.b * o.c + self.d * o.d,
            e: self.a * o.e + self.c * o.f + self.e,
            f: self.b * o.e + self.d * o.f + self.f,
        }
    }

    #[inline]
    fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }
}

// ---------------------------------------------------------------------------
// Byte level helpers
// ---------------------------------------------------------------------------

/// Parse a decimal float from a byte slice, ignoring trailing junk such as "px".
///
/// Returns f64: graph coordinates of a multi-million frame diagram need more
/// than f32's relative precision.
#[inline]
fn parse_f64(s: &[u8]) -> Option<f64> {
    if s.is_empty() {
        return None;
    }
    let mut i = 0usize;
    let mut neg = false;
    match s[0] {
        b'-' => {
            neg = true;
            i = 1;
        }
        b'+' => i = 1,
        _ => {}
    }
    let mut int_part: u64 = 0;
    let mut any = false;
    while i < s.len() && s[i].is_ascii_digit() {
        int_part = int_part
            .saturating_mul(10)
            .saturating_add((s[i] - b'0') as u64);
        i += 1;
        any = true;
    }
    let mut frac = 0.0f64;
    if i < s.len() && s[i] == b'.' {
        i += 1;
        let mut scale = 0.1f64;
        while i < s.len() && s[i].is_ascii_digit() {
            frac += (s[i] - b'0') as f64 * scale;
            scale *= 0.1;
            i += 1;
            any = true;
        }
    }
    if !any {
        return None;
    }
    let mut val = int_part as f64 + frac;
    if i < s.len() && (s[i] | 0x20) == b'e' {
        i += 1;
        let mut eneg = false;
        if i < s.len() && (s[i] == b'-' || s[i] == b'+') {
            eneg = s[i] == b'-';
            i += 1;
        }
        let mut e: i32 = 0;
        while i < s.len() && s[i].is_ascii_digit() {
            e = (e * 10 + (s[i] - b'0') as i32).min(9999);
            i += 1;
        }
        val *= 10f64.powi(if eneg { -e } else { e });
    }
    if neg {
        val = -val;
    }
    Some(val)
}

/// Call f(name, value) for every attribute in a tag body.
#[inline]
fn for_each_attr<F: FnMut(&[u8], &[u8])>(mut s: &[u8], mut f: F) {
    loop {
        while let Some((c, rest)) = s.split_first() {
            if c.is_ascii_whitespace() {
                s = rest;
            } else {
                break;
            }
        }
        if s.is_empty() || s[0] == b'/' {
            return;
        }
        let name_end = s
            .iter()
            .position(|&c| c == b'=' || c.is_ascii_whitespace() || c == b'/')
            .unwrap_or(s.len());
        let name = &s[..name_end];
        s = &s[name_end..];
        while let Some((c, rest)) = s.split_first() {
            if c.is_ascii_whitespace() {
                s = rest;
            } else {
                break;
            }
        }
        if s.is_empty() || s[0] != b'=' {
            if !name.is_empty() {
                f(name, b"");
            }
            continue;
        }
        s = &s[1..];
        while let Some((c, rest)) = s.split_first() {
            if c.is_ascii_whitespace() {
                s = rest;
            } else {
                break;
            }
        }
        if s.is_empty() {
            return;
        }
        let value: &[u8];
        if s[0] == b'"' || s[0] == b'\'' {
            let quote = s[0];
            let body = &s[1..];
            match memchr(quote, body) {
                Some(p) => {
                    value = &body[..p];
                    s = &body[p + 1..];
                }
                None => {
                    value = body;
                    s = b"";
                }
            }
        } else {
            let p = s
                .iter()
                .position(|c| c.is_ascii_whitespace())
                .unwrap_or(s.len());
            value = &s[..p];
            s = &s[p..];
        }
        if !name.is_empty() {
            f(name, value);
        }
        if s.is_empty() {
            return;
        }
    }
}

/// Find the end of a tag whose body starts at "start".
///
/// Returns (body_end, self_closing, index_after_tag).
#[inline]
fn scan_tag(data: &[u8], start: usize) -> (usize, bool, usize) {
    let mut j = start;
    let mut quote = 0u8;
    while j < data.len() {
        let c = data[j];
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
        } else if c == b'"' || c == b'\'' {
            quote = c;
        } else if c == b'>' {
            if j > start && data[j - 1] == b'/' {
                return (j - 1, true, j + 1);
            }
            return (j, false, j + 1);
        }
        j += 1;
    }
    (data.len(), false, data.len())
}

/// Decode XML entities. The common case (no ampersand) borrows nothing extra.
fn unescape(input: &[u8]) -> String {
    let s = String::from_utf8_lossy(input);
    if !s.contains('&') {
        return s.into_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest: &str = &s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        let tail = &rest[p..];
        let window = &tail[..tail.len().min(16)];
        if let Some(semi) = window.find(';') {
            let ent = &tail[1..semi];
            let simple = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ => None,
            };
            let decoded = simple.or_else(|| {
                let hex = ent
                    .strip_prefix("#x")
                    .or_else(|| ent.strip_prefix("#X"))
                    .and_then(|h| u32::from_str_radix(h, 16).ok());
                let dec = ent.strip_prefix('#').and_then(|d| d.parse::<u32>().ok());
                hex.or(dec).and_then(char::from_u32)
            });
            if let Some(ch) = decoded {
                out.push(ch);
                rest = &tail[semi + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

/// Drop the " (123 samples, 4.56%)" suffix that flame graph titles carry.
fn strip_stats(title: &str) -> &str {
    let t = title.trim();
    if let Some(open) = t.rfind(" (") {
        let tail = &t[open + 2..];
        if tail.ends_with(')') && (tail.contains("sample") || tail.contains('%')) {
            return t[..open].trim_end();
        }
    }
    t
}

fn parse_transform(list: &[u8]) -> Affine {
    let mut acc = Affine::IDENTITY;
    let mut i = 0usize;
    while i < list.len() {
        while i < list.len() && !list[i].is_ascii_alphabetic() {
            i += 1;
        }
        let name_start = i;
        while i < list.len() && list[i].is_ascii_alphabetic() {
            i += 1;
        }
        let name = &list[name_start..i];
        while i < list.len() && list[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= list.len() || list[i] != b'(' {
            continue;
        }
        let open = i + 1;
        let close = match memchr(b')', &list[open..]) {
            Some(p) => open + p,
            None => list.len(),
        };
        let mut nums = [0.0f32; 6];
        let mut count = 0usize;
        let mut k = open;
        while k < close && count < 6 {
            while k < close && (list[k].is_ascii_whitespace() || list[k] == b',') {
                k += 1;
            }
            let s = k;
            while k < close && !(list[k].is_ascii_whitespace() || list[k] == b',') {
                k += 1;
            }
            if s == k {
                break;
            }
            nums[count] = parse_f64(&list[s..k]).unwrap_or(0.0) as f32;
            count += 1;
        }
        let t = match name {
            b"translate" => {
                let tx = if count > 0 { nums[0] } else { 0.0 };
                let ty = if count > 1 { nums[1] } else { 0.0 };
                Affine {
                    a: 1.0,
                    b: 0.0,
                    c: 0.0,
                    d: 1.0,
                    e: tx,
                    f: ty,
                }
            }
            b"scale" => {
                let sx = if count > 0 { nums[0] } else { 1.0 };
                let sy = if count > 1 { nums[1] } else { sx };
                Affine {
                    a: sx,
                    b: 0.0,
                    c: 0.0,
                    d: sy,
                    e: 0.0,
                    f: 0.0,
                }
            }
            b"matrix" if count == 6 => Affine {
                a: nums[0],
                b: nums[1],
                c: nums[2],
                d: nums[3],
                e: nums[4],
                f: nums[5],
            },
            b"rotate" if count >= 1 => {
                let rad = nums[0].to_radians();
                let (s, c) = rad.sin_cos();
                let (cx, cy) = if count >= 3 {
                    (nums[1], nums[2])
                } else {
                    (0.0, 0.0)
                };
                let rot = Affine {
                    a: c,
                    b: s,
                    c: -s,
                    d: c,
                    e: 0.0,
                    f: 0.0,
                };
                let to = Affine {
                    a: 1.0,
                    b: 0.0,
                    c: 0.0,
                    d: 1.0,
                    e: cx,
                    f: cy,
                };
                let back = Affine {
                    a: 1.0,
                    b: 0.0,
                    c: 0.0,
                    d: 1.0,
                    e: -cx,
                    f: -cy,
                };
                to.concat(&rot).concat(&back)
            }
            _ => Affine::IDENTITY,
        };
        acc = acc.concat(&t);
        i = if close < list.len() {
            close + 1
        } else {
            list.len()
        };
    }
    acc
}

// ---------------------------------------------------------------------------
// Sink: name interning + frame accumulation for one chunk
// ---------------------------------------------------------------------------

struct Sink {
    frames: Vec<Frame>,
    names: Vec<String>,
    map: HashMap<String, u32>,
    anon: u32,
    /// Index of a rect that is still waiting for its following text element.
    pending_text_rect: Option<u32>,
    pending_title: Option<String>,
    /// Rects the document declared as zero sized; they can never be drawn.
    empty_rects: usize,
    /// Depth inside g elements. Standalone text elements (ids such as "title"
    /// or "search" that generators emit around the graph) must never be adopted
    /// as a frame label.
    group_depth: u32,
}

impl Sink {
    fn new() -> Sink {
        let mut s = Sink {
            frames: Vec::new(),
            names: Vec::new(),
            map: HashMap::new(),
            anon: 0,
            pending_text_rect: None,
            pending_title: None,
            empty_rects: 0,
            group_depth: 0,
        };
        s.anon = s.intern(b"(anonymous)");
        s
    }

    #[inline]
    fn intern(&mut self, name: &[u8]) -> u32 {
        // Fast path: validate + hash the borrowed slice without allocating.
        let s = std::str::from_utf8(name).unwrap_or("(invalid)");
        if let Some(&id) = self.map.get(s) {
            return id;
        }
        let owned = s.to_string();
        let id = self.names.len() as u32;
        self.names.push(s.to_string());
        self.map.insert(owned, id);
        id
    }

    #[inline]
    fn push_rect(&mut self, x: f64, y: f64, w: f64, h: f64, color: u32) {
        let name = match self.pending_title.take() {
            Some(t) => self.intern(t.as_bytes()),
            None => {
                if self.group_depth > 0 {
                    self.pending_text_rect = Some(self.frames.len() as u32);
                }
                self.anon
            }
        };
        self.frames.push(Frame {
            x,
            y: y as f32,
            w,
            h: h as f32,
            color,
            name,
        });
    }
}

// ---------------------------------------------------------------------------
// The chunk scanner
// ---------------------------------------------------------------------------

fn parse_chunk(data: &[u8], sink: &mut Sink, progress: Option<&Progress>) {
    let mut stack: Vec<Affine> = vec![Affine::IDENTITY];
    let mut i = 0usize;
    let mut since_tick = 0usize;

    while i < data.len() {
        let rel = match memchr(b'<', &data[i..]) {
            Some(p) => p,
            None => break,
        };
        let lt = i + rel;
        i = lt + 1;

        if let Some(p) = progress {
            since_tick += 1;
            if since_tick >= 4096 {
                since_tick = 0;
                p.set_done(i as u64);
            }
        }

        if i >= data.len() {
            break;
        }

        match data[i] {
            b'!' => {
                if data[i..].starts_with(b"!--") {
                    i = find_after(data, i, b"-->");
                } else if data[i..].starts_with(b"![CDATA[") {
                    i = find_after(data, i, b"]]>");
                } else {
                    i = find_after(data, i, b">");
                }
                continue;
            }
            b'?' => {
                i = find_after(data, i, b"?>");
                continue;
            }
            b'/' => {
                let ns = i + 1;
                let ne = tag_name_end(data, ns);
                let name = &data[ns..ne];
                if is_group(name) {
                    if stack.len() > 1 {
                        stack.pop();
                    }
                    sink.group_depth = sink.group_depth.saturating_sub(1);
                }
                i = find_after(data, i, b">");
                continue;
            }
            _ => {}
        }

        let ns = i;
        let ne = tag_name_end(data, ns);
        let name = &data[ns..ne];
        let (body_end, self_closing, after) = scan_tag(data, ne);
        i = after;

        let is_g = name.eq_ignore_ascii_case(b"g");
        if is_g || name.eq_ignore_ascii_case(b"svg") {
            // A new group starts a new frame; the previous rect can no longer
            // pick up a label.
            sink.pending_text_rect = None;
            if !self_closing {
                if is_g {
                    sink.group_depth += 1;
                }
                let mut t = Affine::IDENTITY;
                for_each_attr(&data[ne..body_end], |k, v| {
                    if k.eq_ignore_ascii_case(b"transform") {
                        t = parse_transform(v);
                    }
                });
                let top = *stack.last().unwrap_or(&Affine::IDENTITY);
                stack.push(top.concat(&t));
            }
            continue;
        }

        if name.eq_ignore_ascii_case(b"rect") {
            let mut x = 0.0f64;
            let mut y = 0.0f64;
            let mut w = 0.0f64;
            let mut h = 0.0f64;
            let mut fill: Option<u32> = None;
            let mut has_fill = false;
            let mut local = Affine::IDENTITY;
            for_each_attr(&data[ne..body_end], |k, v| {
                if k.eq_ignore_ascii_case(b"x") {
                    x = parse_f64(v).unwrap_or(0.0);
                } else if k.eq_ignore_ascii_case(b"y") {
                    y = parse_f64(v).unwrap_or(0.0);
                } else if k.eq_ignore_ascii_case(b"width") {
                    w = parse_f64(v).unwrap_or(0.0);
                } else if k.eq_ignore_ascii_case(b"height") {
                    h = parse_f64(v).unwrap_or(0.0);
                } else if k.eq_ignore_ascii_case(b"fill") {
                    has_fill = true;
                    fill = paint(v);
                } else if k.eq_ignore_ascii_case(b"transform") {
                    local = parse_transform(v);
                }
            });
            if w <= 0.0 || h <= 0.0 {
                // Zero sized rects exist in SVG output for very large profiles;
                // they can never be seen or hit, so they are not frames.
                sink.empty_rects += 1;
                sink.pending_text_rect = None;
                sink.pending_title = None;
                continue;
            }
            let top = *stack.last().unwrap_or(&Affine::IDENTITY);
            let m = if local.is_identity() {
                top
            } else {
                top.concat(&local)
            };
            let (x0, y0, x1, y1) = if m.is_identity() {
                (x, y, x + w, y + h)
            } else {
                let (ax, ay) = m.apply(x as f32, y as f32);
                let (bx, by) = m.apply((x + w) as f32, (y + h) as f32);
                let (cx, cy) = m.apply((x + w) as f32, y as f32);
                let (dx, dy) = m.apply(x as f32, (y + h) as f32);
                (
                    ax.min(bx).min(cx).min(dx) as f64,
                    ay.min(by).min(cy).min(dy) as f64,
                    ax.max(bx).max(cx).max(dx) as f64,
                    ay.max(by).max(cy).max(dy) as f64,
                )
            };
            // SVG's initial fill is black, so a rect with no fill attribute is
            // a real frame. An explicit "none" or an unresolvable paint such as
            // fill="url(#background)" is decoration: drawing it as a frame makes
            // the background rect - which spans the whole canvas - swallow every
            // row below it, because row grouping is derived from frame height.
            let color = match fill {
                Some(c) => c,
                None if has_fill => {
                    sink.pending_text_rect = None;
                    sink.pending_title = None;
                    continue;
                }
                None => DEFAULT_FILL,
            };
            sink.push_rect(x0, y0, x1 - x0, y1 - y0, color);
            continue;
        }

        if name.eq_ignore_ascii_case(b"title") {
            // A title names the rect that follows it, never the previous one.
            sink.pending_text_rect = None;
            if !self_closing {
                let start = after;
                let end = match memmem::find(&data[start..], b"</title") {
                    Some(p) => start + p,
                    None => data.len(),
                };
                let raw = unescape(trim(&data[start..end]));
                let clean = strip_stats(&raw).to_string();
                sink.pending_title = Some(clean);
                i = find_after(data, end, b">");
            }
            continue;
        }

        if name.eq_ignore_ascii_case(b"text") || name.eq_ignore_ascii_case(b"tspan") {
            if !self_closing {
                let start = after;
                let close: &[u8] = if name.eq_ignore_ascii_case(b"text") {
                    b"</text"
                } else {
                    b"</tspan"
                };
                let end = match memmem::find(&data[start..], close) {
                    Some(p) => start + p,
                    None => data.len(),
                };
                if name.eq_ignore_ascii_case(b"text") {
                    if sink.group_depth > 0 {
                        if let Some(idx) = sink.pending_text_rect.take() {
                            let raw = unescape(trim(&data[start..end]));
                            if !raw.is_empty() {
                                let id = sink.intern(raw.as_bytes());
                                sink.frames[idx as usize].name = id;
                            }
                        }
                    } else {
                        sink.pending_text_rect = None;
                    }
                }
                i = find_after(data, end, b">");
            }
            continue;
        }

        // Any other element starts a new sibling scope.
        sink.pending_text_rect = None;
        sink.pending_title = None;
    }
}

#[inline]
fn trim(mut s: &[u8]) -> &[u8] {
    while let Some((c, rest)) = s.split_first() {
        if c.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    while let Some((c, rest)) = s.split_last() {
        if c.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    s
}

#[inline]
fn find_after(data: &[u8], from: usize, needle: &[u8]) -> usize {
    match memmem::find(&data[from..], needle) {
        Some(p) => from + p + needle.len(),
        None => data.len(),
    }
}

#[inline]
fn tag_name_end(data: &[u8], start: usize) -> usize {
    let mut j = start;
    while j < data.len() {
        let c = data[j];
        if c.is_ascii_whitespace() || c == b'>' || c == b'/' {
            break;
        }
        j += 1;
    }
    j
}

#[inline]
fn is_group(name: &[u8]) -> bool {
    name.eq_ignore_ascii_case(b"g")
}

/// Resolve an SVG paint value. Returns None for "none" and unresolvable paints.
#[inline]
fn paint(v: &[u8]) -> Option<u32> {
    let s = std::str::from_utf8(v).ok()?.trim();
    if s.is_empty() {
        return None;
    }
    // Returns None for "none" and for paints we cannot resolve (gradients and
    // patterns referenced through url()). Callers treat that as "not a frame".
    palette::parse_paint(s)
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

fn parse_header(bytes: &[u8]) -> (f32, f32) {
    let head = &bytes[..bytes.len().min(1 << 20)];
    let start = match memmem::find(head, b"<svg") {
        Some(p) => p + 4,
        None => return (0.0, 0.0),
    };
    let (body_end, _, _) = scan_tag(head, start);
    let mut w = 0.0f32;
    let mut h = 0.0f32;
    let mut viewbox: Option<[f32; 4]> = None;
    for_each_attr(&head[start..body_end], |k, v| {
        if k.eq_ignore_ascii_case(b"width") {
            w = parse_f64(v).unwrap_or(0.0) as f32;
        } else if k.eq_ignore_ascii_case(b"height") {
            h = parse_f64(v).unwrap_or(0.0) as f32;
        } else if k.eq_ignore_ascii_case(b"viewBox") || k.eq_ignore_ascii_case(b"viewbox") {
            let mut nums = [0.0f32; 4];
            let mut n = 0usize;
            let mut i = 0usize;
            while i < v.len() && n < 4 {
                while i < v.len() && (v[i].is_ascii_whitespace() || v[i] == b',') {
                    i += 1;
                }
                let s = i;
                while i < v.len() && !(v[i].is_ascii_whitespace() || v[i] == b',') {
                    i += 1;
                }
                if s == i {
                    break;
                }
                nums[n] = parse_f64(&v[s..i]).unwrap_or(0.0) as f32;
                n += 1;
            }
            if n == 4 {
                viewbox = Some(nums);
            }
        }
    });
    if let Some(vb) = viewbox {
        if vb[2] > 0.0 && vb[3] > 0.0 {
            return (vb[2], vb[3]);
        }
    }
    (w, h)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

struct ChunkOut {
    frames: Vec<Frame>,
    names: Vec<String>,
    empty: usize,
}

impl ChunkOut {
    fn new(sink: Sink) -> ChunkOut {
        ChunkOut {
            frames: sink.frames,
            names: sink.names,
            empty: sink.empty_rects,
        }
    }
}

pub fn parse(bytes: &[u8], progress: &Progress) -> ParsedSvg {
    progress.set_total(bytes.len() as u64);
    progress.set_done(0);
    progress.set_stage(Stage::Parsing);

    let (width, height) = parse_header(bytes);

    let has_transform = memmem::find(bytes, b"transform").is_some();
    let forced_serial = FORCE_SERIAL.load(Ordering::Relaxed);
    let use_parallel = !forced_serial && !has_transform && bytes.len() >= PARALLEL_THRESHOLD;

    let mut chunks: Vec<ChunkOut> = Vec::new();
    let mut stats: Vec<ChunkStat> = Vec::new();

    if use_parallel {
        let bounds = chunk_bounds(bytes, rayon::current_num_threads());
        if bounds.len() > 1 {
            let ranges = bounds.clone();
            chunks = bounds
                .into_par_iter()
                .map(|r| {
                    let mut sink = Sink::new();
                    parse_chunk(&bytes[r.clone()], &mut sink, None);
                    progress.advance((r.end - r.start) as u64);
                    ChunkOut::new(sink)
                })
                .collect();
            stats = ranges
                .iter()
                .zip(chunks.iter())
                .map(|(r, c)| ChunkStat {
                    start: r.start,
                    bytes: r.end - r.start,
                    frames: c.frames.len(),
                    empty: c.empty,
                })
                .collect();
        }
    }
    if chunks.is_empty() {
        let mut sink = Sink::new();
        parse_chunk(bytes, &mut sink, Some(progress));
        progress.set_done(bytes.len() as u64);
        stats.push(ChunkStat {
            start: 0,
            bytes: bytes.len(),
            frames: sink.frames.len(),
            empty: sink.empty_rects,
        });
        chunks.push(ChunkOut::new(sink));
    }

    let total_frames: usize = chunks.iter().map(|c| c.frames.len()).sum();
    progress.set_frames(total_frames as u64);
    progress.set_stage(Stage::Indexing);

    let mut frames: Vec<Frame> = Vec::with_capacity(total_frames);
    let mut names: Vec<String> = Vec::new();
    let mut map: HashMap<String, u32> = HashMap::new();

    for chunk in chunks {
        let mut remap: Vec<u32> = Vec::with_capacity(chunk.names.len());
        for n in &chunk.names {
            let id = match map.get(n.as_str()) {
                Some(&id) => id,
                None => {
                    let id = names.len() as u32;
                    names.push(n.clone());
                    map.insert(n.clone(), id);
                    id
                }
            };
            remap.push(id);
        }
        let anon_local = chunk
            .names
            .iter()
            .position(|n| n == "(anonymous)")
            .map(|i| remap[i]);
        for mut f in chunk.frames {
            f.name = remap[f.name as usize];
            frames.push(f);
        }
        if let Some(anon) = anon_local {
            drop_background(&mut frames, anon);
        }
    }

    if let Ok(mut g) = LAST_CHUNKS.lock() {
        *g = stats.clone();
    }
    ParsedSvg {
        frames,
        names,
        width,
        height,
    }
}

/// Generators such as inferno paint a full-width background rect. It carries no
/// title and would otherwise show up as a bogus root frame, so remove any
/// unnamed rect that spans essentially the whole graph.
fn drop_background(frames: &mut Vec<Frame>, anon: u32) {
    if frames.len() < 2 {
        return;
    }
    let mut min_x = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    for f in frames.iter() {
        if f.x < min_x {
            min_x = f.x;
        }
        if f.x + f.w > max_x {
            max_x = f.x + f.w;
        }
    }
    let span = max_x - min_x;
    if span <= 0.0 {
        return;
    }
    let cutoff = span * 0.995;
    frames.retain(|f| !(f.name == anon && f.w >= cutoff));
}

/// Pick split points for parallel parsing.
///
/// Boundaries are snapped forward to an element start so that each piece is a
/// self-contained sequence of complete elements.
fn chunk_bounds(bytes: &[u8], threads: usize) -> Vec<Range<usize>> {
    let whole: Vec<Range<usize>> = std::iter::once(0..bytes.len()).collect();
    let threads = threads.max(1);
    if threads == 1 {
        return whole;
    }
    // Anchor on title elements when present, because a title carries the name of
    // the rect that follows it and must stay in the same piece.
    let titles = memmem::find_iter(bytes, b"<title").take(2).count();
    let anchor: &[u8] = if titles >= 2 { b"<title" } else { b"<rect" };

    let step = bytes.len() / threads;
    if step < 1 << 16 {
        return whole;
    }
    let mut bounds: Vec<Range<usize>> = Vec::with_capacity(threads);
    let mut start = 0usize;
    for k in 1..threads {
        let target = k * step;
        if target <= start {
            continue;
        }
        let split = match memmem::find(&bytes[target..], anchor) {
            Some(p) => target + p,
            None => break,
        };
        if split <= start {
            continue;
        }
        bounds.push(start..split);
        start = split;
    }
    if start < bytes.len() {
        bounds.push(start..bytes.len());
    }
    bounds.retain(|r| r.end > r.start);
    if bounds.is_empty() {
        bounds.push(0..bytes.len());
    }
    bounds
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::Scene;

    /// Shaped like real flamegraph.pl output: a gradient-filled background rect
    /// whose paint cannot be resolved, standalone text elements with ids, a
    /// script block wrapped in CDATA that contains "<", and two rows of frames.
    const PL_STYLE: &str = r##"<?xml version="1.0" standalone="no"?>
<svg version="1.1" width="1200" height="148" viewBox="0 0 1200 148" xmlns="http://www.w3.org/2000/svg">
<defs><linearGradient id="background" y1="0" y2="1" x1="0" x2="0"><stop stop-color="#eeeeee" offset="5%"/><stop stop-color="#eeeeb0" offset="95%"/></linearGradient></defs>
<style type="text/css">text { font-family:Verdana; } #frames > *:hover { stroke:black; }</style>
<script type="text/ecmascript"><![CDATA[ for (var i = 0; i < 5; i++) { if (i > 1) {} } ]]></script>
<rect x="0.0" y="0.0" width="1200.0" height="148.0" fill="url(#background)"/>
<text id="title" x="600.00" y="24.00">Flame Graph</text>
<text id="details" x="10.00" y="140.00"> </text>
<svg id="frames" x="10" width="1180">
<g class="func_g" onmouseover="s(this)"><title>all (100 samples, 100.00%)</title><rect x="0.0" y="0.0" width="1200.0" height="15.0" fill="rgb(255,0,0)" rx="2" ry="2"/><text x="3.00" y="12.00">all</text></g>
<g class="func_g" onmouseover="s(this)"><title>a (60 samples, 60.00%)</title><rect x="0.0" y="16.0" width="700.0" height="15.0" fill="rgb(255,60,0)" rx="2" ry="2"/><text x="3.00" y="28.00">a</text></g>
<g class="func_g" onmouseover="s(this)"><title>b (40 samples, 40.00%)</title><rect x="700.0" y="16.0" width="500.0" height="15.0" fill="rgb(255,120,0)" rx="2" ry="2"/><text x="703.00" y="28.00">b</text></g>
</svg></svg>"##;

    fn build(src: &str) -> Scene {
        let progress = Progress::new();
        let parsed = parse(src.as_bytes(), &progress);
        Scene::build(
            parsed.frames,
            parsed.names,
            "t".into(),
            "mem".into(),
            src.len() as u64,
            0.0,
            [parsed.width, parsed.height],
        )
    }

    #[test]
    fn gradient_background_is_not_a_frame() {
        let scene = build(PL_STYLE);
        assert_eq!(
            scene.frames.len(),
            3,
            "the url(#background) rect leaked into the frames"
        );
        // Row grouping derives its tolerance from frame height, so a background
        // rect spanning the whole canvas would collapse every row into one.
        assert_eq!(scene.levels.len(), 2, "rows were merged into one slab");
        assert!(
            scene.frames.iter().all(|f| f.h <= 15.0),
            "a frame spans the whole canvas"
        );
    }

    #[test]
    fn standalone_text_does_not_name_a_frame() {
        let scene = build(PL_STYLE);
        assert_ne!(scene.name_of(0), "Flame Graph");
        assert_eq!(scene.name_of(0), "all");
        // "(anonymous)" is always interned first and stays in the table even
        // when nothing references it.
        assert_eq!(scene.names, vec!["(anonymous)", "all", "a", "b"]);
    }

    #[test]
    fn fill_none_is_skipped_but_a_missing_fill_is_drawn() {
        let src = r#"<svg width="100" height="40"><rect x="0" y="0" width="50" height="10" fill="none"/><rect x="0" y="20" width="50" height="10"/></svg>"#;
        let scene = build(src);
        assert_eq!(scene.frames.len(), 1, "fill=none must not become a frame");
        assert_eq!(scene.frames[0].h, 10.0);
    }
}
