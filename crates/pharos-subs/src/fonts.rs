//! Caller-supplied fonts. The crate bundles none; the first font added is
//! the default fallback.

use skrifa::font::Font;
use skrifa::raw::types::NameId;

/// Handle to a font added with `Renderer::add_font`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FontId(pub(crate) u32);

/// The bytes were not a usable font.
#[derive(Debug, PartialEq, Eq)]
pub struct FontError;

impl std::fmt::Display for FontError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not a parseable font")
    }
}

impl std::error::Error for FontError {}

pub(crate) struct FontEntry {
    pub font: Font,
    /// Lowercased family and full names, as libass matches them.
    names: Vec<String>,
    pub bold: bool,
    pub italic: bool,
    pub upem: f32,
    /// Line metrics in font units (descent is negative).
    pub ascent: f32,
    pub descent: f32,
}

fn f48(v: impl Into<f64>) -> f32 {
    v.into() as f32
}

impl FontEntry {
    fn new(data: Vec<u8>) -> Option<Self> {
        let font = Font::new(skrifa::font::Blob::from(data), 0)?;
        let mut names = Vec::new();
        let table = font.names();
        for id in [
            NameId::FAMILY_NAME,
            NameId::TYPOGRAPHIC_FAMILY_NAME,
            NameId::FULL_NAME,
        ] {
            if let Some(n) = table.get(id) {
                for loc in n.iter() {
                    let s: String = loc.name().chars().collect::<String>().to_lowercase();
                    if !s.is_empty() && !names.contains(&s) {
                        names.push(s);
                    }
                }
            }
        }
        let attrs = font.default_attributes();
        let bold = attrs.weight.value() >= 600.0;
        let italic = !matches!(attrs.style, skrifa::font::attribute::Style::Normal);
        let m = font.metrics();
        let upem = f32::from(m.units_per_em.max(1));
        let line = if m.use_typo_metrics {
            m.typo_line.or(m.hhea_line)
        } else {
            m.hhea_line.or(m.typo_line)
        };
        let (a, d) = line.map_or((0.8 * upem, -0.2 * upem), |l| {
            (f48(l.ascender.to_f64()), f48(l.descender.to_f64()))
        });
        Some(Self {
            font,
            names,
            bold,
            italic,
            upem,
            ascent: a,
            descent: -d.abs(),
        })
    }
}

#[derive(Default)]
pub(crate) struct FontDb {
    pub fonts: Vec<FontEntry>,
}

impl FontDb {
    pub fn add(&mut self, data: Vec<u8>) -> Result<FontId, FontError> {
        let e = FontEntry::new(data).ok_or(FontError)?;
        self.fonts.push(e);
        Ok(FontId((self.fonts.len() - 1) as u32))
    }

    pub fn get(&self, id: usize) -> Option<&FontEntry> {
        self.fonts.get(id)
    }

    /// Best font for a family request: exact name match preferring the
    /// requested weight/slant, else the default (first added) font.
    pub fn find(&self, family: &str, bold: bool, italic: bool) -> Option<usize> {
        let want = family.trim().trim_start_matches('@').to_lowercase();
        let mut best: Option<(usize, u8)> = None;
        for (i, f) in self.fonts.iter().enumerate() {
            if f.names.contains(&want) {
                let score = u8::from(f.bold == bold) * 2 + u8::from(f.italic == italic);
                if best.map_or(true, |(_, s)| score > s) {
                    best = Some((i, score));
                }
            }
        }
        match best {
            Some((i, _)) => Some(i),
            None if self.fonts.is_empty() => None,
            None => Some(0),
        }
    }
}
