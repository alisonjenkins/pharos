//! The fonts a media file embeds for its ASS subtitles, cut down to the
//! glyphs one subtitle track draws (026 FR-010), so a player fetches a few
//! kilobytes for a CJK font instead of the whole attachment.
//!
//! `skera` (fontations' general-purpose subsetter) does the cutting. The
//! `subsetter` crate the spec first named writes PDF-only fonts without a
//! `cmap`, which libass cannot use.

use std::collections::BTreeSet;

use pharos_core::MediaAttachment;
use read_fonts::{
    collections::IntSet,
    types::{GlyphId, NameId, Tag},
    FileRef,
};
use skera::{subset_font, Plan, SubsetFlags, DEFAULT_LAYOUT_FEATURES};

/// Bump when the subsetting settings or [`ass_codepoints`] change what they
/// output: the version is part of the cache directory name, so a build never
/// reads what an older one cut.
pub const FONT_SUBSET_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum FontError {
    #[error("not an OpenType font: {0}")]
    Parse(String),
    #[error("collection has no font at index 0")]
    EmptyCollection,
    #[error("subsetting failed: {0}")]
    Subset(String),
}

/// Whether `a` is a font a subtitle can use: a font mimetype, or a font file
/// extension for the muxers that write `application/octet-stream`.
pub fn is_font_attachment(a: &MediaAttachment) -> bool {
    let by_mime = a
        .mime_type
        .as_deref()
        .is_some_and(crate::api::jellyfin::subtitles::is_font);
    let by_name = a.filename.as_deref().is_some_and(|name| {
        let lower = name.to_ascii_lowercase();
        [".ttf", ".otf", ".ttc"].iter().any(|e| lower.ends_with(e))
    });
    by_mime || by_name
}

/// The `Content-Type` a served font carries: the muxer's font mimetype, else
/// one from the file name.
pub fn font_mime(a: &MediaAttachment) -> &str {
    match a.mime_type.as_deref() {
        Some(m) if crate::api::jellyfin::subtitles::is_font(m) => m,
        _ if a
            .filename
            .as_deref()
            .is_some_and(|n| n.to_ascii_lowercase().ends_with(".otf")) =>
        {
            "font/otf"
        }
        _ => "font/ttf",
    }
}

/// Every code point the script's dialogue can draw, over all its styles and
/// `\fn` overrides at once (a missing glyph makes libass fall back to another
/// font without a word, so too many is harmless and too few is not).
///
/// Override blocks (`{...}`) and vector drawings (`\p1` .. `\p0`) draw no
/// text; `\N` and `\n` are breaks and `\h` is a no-break space. A space and a
/// no-break space are always included.
pub fn ass_codepoints(script: &str) -> BTreeSet<u32> {
    let mut out: BTreeSet<u32> = BTreeSet::from([0x20, 0xA0]);
    let mut in_events = false;
    let mut text_field = 9;
    let mut fields = 10;
    for line in script.lines() {
        let line = line.trim_start_matches('\u{feff}').trim();
        if line.starts_with('[') {
            in_events = line.eq_ignore_ascii_case("[events]");
            continue;
        }
        if !in_events {
            continue;
        }
        if let Some(format) = strip_prefix_ci(line, "format:") {
            let names: Vec<&str> = format.split(',').map(str::trim).collect();
            if let Some(at) = names.iter().position(|n| n.eq_ignore_ascii_case("text")) {
                text_field = at;
                fields = names.len();
            }
            continue;
        }
        let Some(event) = strip_prefix_ci(line, "dialogue:") else {
            continue;
        };
        // Text is the last field and may hold commas itself.
        let mut parts = event.splitn(fields.max(text_field + 1), ',');
        if let Some(text) = parts.nth(text_field) {
            collect_dialogue(text, &mut out);
        }
    }
    out
}

fn strip_prefix_ci<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let head = line.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| line.get(prefix.len()..))
        .flatten()
}

fn collect_dialogue(text: &str, out: &mut BTreeSet<u32>) {
    let mut drawing = false;
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('{') {
            let (block, tail) = after.split_once('}').unwrap_or((after, ""));
            if let Some(on) = drawing_mode(block) {
                drawing = on;
            }
            rest = tail;
            continue;
        }
        let mut chars = rest.chars();
        let Some(c) = chars.next() else { break };
        let after = chars.as_str();
        if c == '\\' {
            match after.chars().next() {
                Some('N' | 'n') => {
                    rest = after.get(1..).unwrap_or_default();
                    continue;
                }
                Some('h') => {
                    rest = after.get(1..).unwrap_or_default();
                    continue;
                }
                _ => {}
            }
        }
        if !drawing {
            out.insert(u32::from(c));
        }
        rest = after;
    }
}

/// The drawing mode the last `\p<n>` tag in an override block sets: `Some(true)`
/// for `\p1` and up, `Some(false)` for `\p0`; `None` when the block has none.
/// `\pos`, `\pbo` and the like are not it.
fn drawing_mode(block: &str) -> Option<bool> {
    let mut mode = None;
    let mut rest = block;
    while let Some(at) = rest.find("\\p") {
        let after = rest.get(at + 2..).unwrap_or_default();
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(n) = digits.parse::<u32>() {
            mode = Some(n > 0);
        }
        rest = after;
    }
    mode
}

/// `font` cut to the glyphs `codepoints` map to, with the layout features the
/// default shaper uses and the name table (every language, since libass
/// matches a style's `Fontname` against any of a font's names). A collection
/// yields its first font.
pub fn subset(font: &[u8], codepoints: &BTreeSet<u32>) -> Result<Vec<u8>, FontError> {
    let font = match FileRef::new(font).map_err(|e| FontError::Parse(e.to_string()))? {
        FileRef::Font(f) => f,
        FileRef::Collection(c) => c.get(0).map_err(|_| FontError::EmptyCollection)?,
    };
    let unicodes: IntSet<u32> = codepoints.iter().copied().collect();
    let gids = IntSet::<GlyphId>::empty();
    // The tables HarfBuzz drops by default: old Apple layout and Graphite.
    let drop_tables: IntSet<Tag> = [
        skera::MORX,
        skera::MORT,
        skera::KERX,
        skera::JSTF,
        skera::DSIG,
        skera::EBSC,
        skera::GLAT,
        skera::GLOC,
        skera::SILF,
        skera::SILL,
        skera::PCLT,
        skera::LTSH,
    ]
    .into_iter()
    .collect();
    let mut scripts = IntSet::<Tag>::empty();
    scripts.invert();
    let features: IntSet<Tag> = DEFAULT_LAYOUT_FEATURES.iter().copied().collect();
    let mut name_ids = IntSet::<NameId>::empty();
    name_ids.insert_range(NameId::from(0)..=NameId::from(6));
    name_ids.insert_range(NameId::from(16)..=NameId::from(17));
    let mut name_languages = IntSet::<u16>::empty();
    name_languages.invert();
    let plan = Plan::new(
        &gids,
        &unicodes,
        &font,
        SubsetFlags::default(),
        &drop_tables,
        &scripts,
        &features,
        &name_ids,
        &name_languages,
    );
    subset_font(&font, &plan).map_err(|e| FontError::Subset(e.to_string()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const SCRIPT: &str = "\u{feff}[Script Info]\nTitle: ZZZ\n\n[V4+ Styles]\nStyle: Default,Arial,20\n\n\
[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,Hi, {\\i1}wor{\\i0}ld\\NA\\hB\n\
Comment: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,Qq\n\
Dialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,{\\p1}m 0 0 l 9 9{\\p0}\u{3042}{\\pos(1,2)}\u{3044}\n";

    /// IBM Plex Mono Regular (SIL OFL 1.1), already cut to printable ASCII.
    const PLEX: &[u8] = include_bytes!("../../tests/fixtures/IBMPlexMono-Ascii.ttf");

    fn cmap_has(font: &[u8], c: char) -> bool {
        use skrifa::MetadataProvider as _;
        let font = read_fonts::FontRef::new(font).unwrap();
        font.charmap().map(c).is_some()
    }

    fn family_names(font: &[u8]) -> Vec<String> {
        use skrifa::MetadataProvider as _;
        let font = read_fonts::FontRef::new(font).unwrap();
        font.localized_strings(skrifa::string::StringId::FAMILY_NAME)
            .map(|s| s.to_string())
            .collect()
    }

    /// V488: the subset still maps what the script draws and nothing else,
    /// keeps the names libass matches a style's Fontname against, and is
    /// smaller. Disarm check: an empty codepoint set maps no 'H'.
    #[test]
    fn a_subset_keeps_the_drawn_glyphs_and_the_names_and_drops_the_rest() {
        let cps = ass_codepoints("[Events]\nFormat: Start, Text\nDialogue: 0:00:01.00,Hi, there\n");
        let cut = subset(PLEX, &cps).unwrap();
        assert!(
            cut.len() < PLEX.len() / 2,
            "{} of {}",
            cut.len(),
            PLEX.len()
        );
        for c in ['H', 'i', 't', 'h', 'e', 'r', ',', ' '] {
            assert!(cmap_has(&cut, c), "{c:?} lost");
        }
        for c in ['Z', 'q', '9', '~'] {
            assert!(!cmap_has(&cut, c), "{c:?} kept");
        }
        assert!(family_names(&cut).iter().any(|n| n.contains("Plex Mono")));
        assert!(!cmap_has(&subset(PLEX, &BTreeSet::new()).unwrap(), 'H'));
    }

    fn chars(set: &BTreeSet<u32>) -> String {
        set.iter().filter_map(|&c| char::from_u32(c)).collect()
    }

    /// V488: the codepoints of a track's fonts are what its dialogue draws.
    #[test]
    fn codepoints_are_the_dialogue_text_without_tags_drawings_or_breaks() {
        let got = chars(&ass_codepoints(SCRIPT));
        for want in [
            'H', 'i', ',', ' ', 'w', 'o', 'r', 'l', 'd', 'A', 'B', '\u{a0}',
        ] {
            assert!(got.contains(want), "{want:?} missing from {got:?}");
        }
        assert!(got.contains('\u{3042}') && got.contains('\u{3044}'));
        // Not dialogue: header, styles, comments, tags, breaks, drawing commands.
        for not in ['Z', 'Q', 'q', 'N', 'h', '\\', '{', 'm', '9', 'p'] {
            assert!(!got.contains(not), "{not:?} leaked into {got:?}");
        }
    }

    #[test]
    fn a_format_line_moves_the_text_field() {
        let script = "[Events]\nFormat: Start, Text\nDialogue: 0:00:01.00,Xy,z\n";
        assert_eq!(chars(&ass_codepoints(script)), " ,Xyz\u{a0}");
    }

    #[test]
    fn a_script_without_events_still_gets_a_space() {
        assert_eq!(ass_codepoints("[Script Info]\nTitle: x\n").len(), 2);
    }

    #[test]
    fn a_non_font_is_refused_not_panicked_on() {
        assert!(matches!(
            subset(b"FONT-ZERO-payload", &BTreeSet::from([0x41])),
            Err(FontError::Parse(_))
        ));
    }
}
