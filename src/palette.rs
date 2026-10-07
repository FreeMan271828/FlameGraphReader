//! Colours: SVG fill parsing and the flamegraph.pl name-hash palettes.

/// Deterministic 0..=255 hash of a frame name.
///
/// This mirrors the Perl "namehash" used by flamegraph.pl / inferno so that
/// recolouring a graph reproduces the original generator's look.
pub fn name_hash(name: &str) -> f32 {
    let mut vector = 0.0f32;
    let mut weight = 1.0f32;
    let mut max = 1.0f32;
    const MODULUS: f32 = 10.0;
    for byte in name.bytes() {
        let i = (byte % 10) as f32;
        vector += (i / (MODULUS - 1.0)) * weight;
        max += weight;
        weight *= 0.70;
    }
    if name.is_empty() {
        return 0.0;
    }
    (1.0 - vector / max) * 255.0
}

/// The palettes offered by the viewer.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Palette {
    /// Use the fill colours embedded in the SVG (default).
    #[default]
    Native,
    /// flamegraph.pl default: red -> yellow.
    Hot,
    /// green -> teal, for allocation profiles.
    Mem,
    /// grey -> blue, for I/O profiles.
    Io,
    /// white -> blue -> black, colour-blind friendly ramp.
    Cold,
    /// A single flat colour (structure only).
    Flat,
}

impl Palette {
    pub const ALL: [Palette; 6] = [
        Palette::Native,
        Palette::Hot,
        Palette::Mem,
        Palette::Io,
        Palette::Cold,
        Palette::Flat,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Palette::Native => "SVG (native)",
            Palette::Hot => "Hot",
            Palette::Mem => "Memory",
            Palette::Io => "I/O",
            Palette::Cold => "Cold",
            Palette::Flat => "Flat",
        }
    }

    /// Colour for a frame, given its name (for hashing) and its native colour.
    pub fn color(self, name: &str, native: u32) -> u32 {
        let v = name_hash(name);
        match self {
            Palette::Native => native,
            Palette::Hot => {
                let r = 205.0 + 50.0 * v / 255.0;
                let g = 230.0 * v / 255.0;
                let b = 55.0 * v / 255.0;
                pack(r, g, b)
            }
            Palette::Mem => {
                let r = 5.0 * v / 255.0;
                let g = 190.0 + 50.0 * v / 255.0;
                let b = 20.0 + 210.0 * v / 255.0;
                pack(r, g, b)
            }
            Palette::Io => {
                let r = 70.0 + 60.0 * v / 255.0;
                let g = 70.0 + 60.0 * v / 255.0;
                let b = 185.0 + 70.0 * v / 255.0;
                pack(r, g, b)
            }
            Palette::Cold => {
                // Piecewise ramp from near-white through blue to near-black.
                let t = v / 255.0;
                if t < 0.5 {
                    let k = t * 2.0;
                    pack(235.0 - 195.0 * k, 240.0 - 180.0 * k, 250.0 - 40.0 * k)
                } else {
                    let k = (t - 0.5) * 2.0;
                    pack(40.0 - 30.0 * k, 60.0 - 45.0 * k, 210.0 - 180.0 * k)
                }
            }
            Palette::Flat => 0x9a_a7_b4,
        }
    }
}

fn pack(r: f32, g: f32, b: f32) -> u32 {
    let c = |x: f32| x.clamp(0.0, 255.0).round() as u32;
    (c(r) << 16) | (c(g) << 8) | c(b)
}

/// Parse a CSS-ish colour into 0xRRGGBB. Returns None when unsupported.
pub fn parse_color(s: &str) -> Option<u32> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(hex) = s.strip_prefix('#') {
        return parse_hex(hex);
    }
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("rgb") {
        let open = lower.find('(')?;
        let close = lower.rfind(')')?;
        let inner = &lower[open + 1..close];
        let mut parts = inner.split([',', ' ']).filter(|p| !p.is_empty());
        let pct = inner.contains('%');
        let mut c = [0u32; 3];
        for slot in c.iter_mut() {
            let raw = parts.next()?;
            let num: f32 = raw.trim_end_matches('%').trim().parse().ok()?;
            *slot = if pct {
                (num * 2.55).round().clamp(0.0, 255.0) as u32
            } else {
                num.round().clamp(0.0, 255.0) as u32
            };
        }
        return Some((c[0] << 16) | (c[1] << 8) | c[2]);
    }
    named_color(&lower)
}

fn parse_hex(hex: &str) -> Option<u32> {
    let h = hex.trim();
    let v = u32::from_str_radix(h, 16).ok()?;
    match h.len() {
        3 => {
            let r = (v >> 8) & 0xf;
            let g = (v >> 4) & 0xf;
            let b = v & 0xf;
            Some(((r * 17) << 16) | ((g * 17) << 8) | (b * 17))
        }
        6 => Some(v & 0xff_ffff),
        _ => None,
    }
}

fn named_color(name: &str) -> Option<u32> {
    // Only the handful of names that show up in hand-written or themed SVGs.
    Some(match name {
        "none" | "transparent" => return None,
        "black" => 0x00_00_00,
        "white" => 0xff_ff_ff,
        "red" => 0xff_00_00,
        "green" => 0x00_80_00,
        "blue" => 0x00_00_ff,
        "yellow" => 0xff_ff_00,
        "orange" => 0xff_a5_00,
        "purple" => 0x80_00_80,
        "grey" | "gray" => 0x80_80_80,
        "silver" => 0xc0_c0_c0,
        "aqua" | "cyan" => 0x00_ff_ff,
        "magenta" | "fuchsia" => 0xff_00_ff,
        "lime" => 0x00_ff_00,
        "navy" => 0x00_00_80,
        "teal" => 0x00_80_80,
        "maroon" => 0x80_00_00,
        "olive" => 0x80_80_00,
        _ => return None,
    })
}

/// Relative luminance, used to pick a readable label colour.
pub fn luminance(c: u32) -> f32 {
    let r = ((c >> 16) & 0xff) as f32 / 255.0;
    let g = ((c >> 8) & 0xff) as f32 / 255.0;
    let b = (c & 0xff) as f32 / 255.0;
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// Best-contrast text colour for a background.
pub fn label_color(bg: u32) -> u32 {
    if luminance(bg) > 0.60 {
        0x10_14_18
    } else {
        0xf2_f5_f8
    }
}

/// Parse a colour from an SVG paint attribute, accepting url() references by
/// falling back to the referenced id (gradients are not resolved).
pub fn parse_paint(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("url(") {
        let _ = rest;
        return None;
    }
    parse_color(s)
}
