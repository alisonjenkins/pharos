//! LIB-D6 — filename/folder parsing as a [`MetadataProvider`].
//!
//! [`FilenameProvider`] derives lightweight, local-first metadata purely
//! from the file/folder *name* carried in the [`MetadataRequest`] — it
//! performs **no IO** (no `stat`, no read), so it is safe to run in the
//! parallel probe phase of `scan_into` and is fully deterministic.
//!
//! What it produces, from a movie stem like
//! `"The Matrix (1999) [1080p] BluRay - Director's Cut"`:
//! - [`production_year`] from a trailing `(YYYY)` or a `.YYYY.` /
//!   ` YYYY ` scene token (reusing the canonical 1800–2999 validation
//!   shared with [`fs`]'s folder-year parser, so a stray `[1080p]` or
//!   `(Uncut)` never masquerades as a year);
//! - the edition/version tag (e.g. `Director's Cut`) via the **shared**
//!   [`fs::split_edition_tag`] vocabulary — D6 does not duplicate the
//!   edition word list nor fight `group_editions` (the grouping driver
//!   still owns demoting editions to alternate sources; this provider only
//!   surfaces the recognised label as a tag and strips it from the title);
//! - quality/source tokens (`1080p`, `2160p`, `BluRay`, `WEB-DL`,
//!   `HDTV`, …) collected into [`MetadataResult::tags`];
//! - a **cleaned title** with the year, quality/source tokens, edition
//!   suffix, and scene separators (`.`/`_`) stripped.
//!
//! ## Priority
//! [`PRIORITY`] is low (below NFO, above an empty resolver): a filename is
//! the weakest local source, so a real `<title>`/`<year>` in an NFO
//! always wins the scalar merge. The cleaned filename title only fills the
//! gap when no higher-priority provider supplied one.
//!
//! ## Episodes
//! C11 already captures the show-folder year on [`SeriesInfo`]; this
//! provider therefore does **not** emit `production_year` for episodes (it
//! would fight the season/series identity) — it still contributes a
//! cleaned title + quality tags from the episode filename.
//!
//! [`production_year`]: pharos_core::MetadataResult::production_year
//! [`fs`]: crate::fs
//! [`fs::split_edition_tag`]: crate::fs::split_edition_tag
//! [`SeriesInfo`]: pharos_core::SeriesInfo

use pharos_core::{
    DomainResult, MediaKind, MetadataProvider, MetadataRequest, MetadataResult, Video3DFormat,
};

use crate::fs;

/// Merge priority for the filename source — deliberately low so an NFO
/// `<title>`/`<year>` (higher priority) wins the scalar merge and the
/// filename only fills gaps. Still positive so it beats a hypothetical
/// negative-priority "last resort" provider.
pub const PRIORITY: i32 = 10;

/// LIB-D6 — pure, IO-free [`MetadataProvider`] that derives a cleaned
/// title, production year, and quality/source/edition tags from the
/// media file (or, for movies, folder) name. Stateless; cheap to clone.
#[derive(Debug, Clone, Copy, Default)]
pub struct FilenameProvider;

impl FilenameProvider {
    /// Construct the provider. Stateless — `FilenameProvider` and
    /// `FilenameProvider::new()` are equivalent.
    pub fn new() -> Self {
        Self
    }

    /// Pure parse of `stem` (a file stem or folder name, already
    /// extension-stripped) for a movie/episode. Exposed for table-driven
    /// unit tests; `fetch` delegates here.
    ///
    /// `with_year`: whether to emit a parsed `production_year`
    /// (suppressed for episodes — C11 owns the show year).
    pub fn parse_stem(stem: &str, with_year: bool) -> ParsedName {
        // 1) Edition: reuse the shared edition vocabulary. `split_edition_tag`
        //    keys on a trailing ` - <edition>` form; the raw stem's ` - ` is
        //    meaningful so check it before separator-normalisation. When it
        //    matches we both record the label AND clean the title from the
        //    truncated left side, so multi-word editions ("Director's Cut")
        //    are handled without per-word matching.
        let (edition, stem_no_edition) = match fs::split_edition_tag(stem) {
            Some((left, ed)) => (Some(ed.to_string()), left),
            None => (None, stem),
        };

        // 2) Pull a parenthesised `(YYYY)` from the (edition-stripped) stem
        //    — a trailing one via the canonical [`fs::parse_folder_year`]
        //    (1800–2999 window so `(Uncut)` / `(1)` never match), else a
        //    bracketed/parenthesised one anywhere in the stem. A
        //    *parenthesised* year is the canonical title delimiter, whereas
        //    a bare 4-digit token that's also a valid year may legitimately
        //    be part of the title ("2001 A Space Odyssey", "Blade Runner
        //    2049").
        let paren_year: Option<u32> =
            fs::parse_folder_year(stem_no_edition).or_else(|| bracketed_year(stem_no_edition));

        // Work on a normalised copy: scene releases use `.` / `_` as word
        // separators. Replace them with spaces so token matching + the
        // cleaned title read naturally. Parentheses/brackets become spaces
        // too so `[1080p]` and `(1999)` split into bare tokens.
        let normalised = normalise_separators(stem_no_edition);
        let words: Vec<&str> = normalised.split_whitespace().collect();

        // 3) Walk tokens. Collect quality/source tags wherever they appear.
        //    The title runs from the start up to the first *metadata
        //    boundary*: a quality/source token, or a bare 4-digit year that
        //    is NOT the leading token (a leading "2001"/"1984" is the
        //    title). A parenthesised year (already captured) is also a
        //    boundary. Everything after the boundary is metadata, not title.
        let mut tags: Vec<String> = Vec::new();
        let mut bare_year_val: Option<u32> = None;
        let mut title_end = words.len();
        let mut boundary_hit = false;
        let mut idx = 0;
        while idx < words.len() {
            let Some(&word) = words.get(idx) else { break };
            if let Some(tag) = quality_source_tag(word) {
                push_unique(&mut tags, tag);
                if !boundary_hit {
                    title_end = idx;
                    boundary_hit = true;
                }
                idx = idx.saturating_add(1);
                continue;
            }
            if let Some(&next_word) = words.get(idx.saturating_add(1)) {
                let compound = format!("{word}-{next_word}");
                if let Some(tag) = quality_source_tag(&compound) {
                    push_unique(&mut tags, tag);
                    if !boundary_hit {
                        title_end = idx;
                        boundary_hit = true;
                    }
                    idx = idx.saturating_add(2);
                    continue;
                }
            }
            if let Some(y) = bare_year(word) {
                // When a parenthesised year was found, ONLY the token equal
                // to it delimits the title — an earlier in-title number that
                // happens to be a valid year ("Blade Runner 2049 (2017)")
                // stays part of the title.
                if let Some(py) = paren_year {
                    if idx > 0 && !boundary_hit && y == py {
                        title_end = idx;
                        boundary_hit = true;
                    }
                    idx = idx.saturating_add(1);
                    continue;
                }
                // No parenthesised year: a leading year is part of the title;
                // the first later year delimits it and becomes the year.
                if idx > 0 && !boundary_hit {
                    bare_year_val = Some(y);
                    title_end = idx;
                    boundary_hit = true;
                }
                idx = idx.saturating_add(1);
                continue;
            }
            // A standalone known-edition word (e.g. "Uncut", "Remastered"
            // not in the trailing ` - ` form) also bounds the title. The
            // edition label itself isn't surfaced here (only the trailing
            // ` - <edition>` form populates `edition`); it's simply trimmed.
            if !boundary_hit && idx > 0 && is_edition_word(word) {
                title_end = idx;
                boundary_hit = true;
                idx = idx.saturating_add(1);
                continue;
            }
            idx = idx.saturating_add(1);
        }

        let title = {
            let joined = words[..title_end].join(" ");
            let trimmed = joined.trim().trim_end_matches('-').trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        };

        // Prefer the parenthesised year (canonical); fall back to a bare
        // year token found past the title.
        let year = paren_year.or(bare_year_val);

        ParsedName {
            title,
            year: if with_year { year } else { None },
            edition,
            tags,
        }
    }
}

/// LIB-D6 — the deterministic parse of a single file/folder name. The
/// table-driven unit tests assert against this directly; `fetch` lifts it
/// into a [`MetadataResult`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedName {
    /// Cleaned title (year / quality / edition / scene separators
    /// stripped), or `None` when nothing readable remained.
    pub title: Option<String>,
    /// Parsed production year (movies only; `None` for episodes).
    pub year: Option<u32>,
    /// Recognised edition label (e.g. `"Director's Cut"`), if any. Carried
    /// as a tag; `group_editions` still owns alternate-source demotion.
    pub edition: Option<String>,
    /// Quality/source tokens (`"1080p"`, `"BluRay"`, …), de-duplicated in
    /// first-seen order.
    pub tags: Vec<String>,
}

/// Convenience alias for [`FilenameProvider::parse_stem`].
pub fn parse_filename(stem: &str, with_year: bool) -> ParsedName {
    FilenameProvider::parse_stem(stem, with_year)
}

/// LIB-D6 / 018-3d-media: Parse 3D video layout and source tag from a filename or path.
///
/// Returns `(format, source)`:
/// - `(Some(Video3DFormat::...), Some("filename"))` for recognized 3D layouts:
///   `fsbs`, `f-sbs`, `full-sbs`, `full.sbs` -> `FullSideBySide`
///   `hsbs`, `h-sbs`, `half-sbs`, `half.sbs`, or bare `sbs` (when not preceded by `full`/`f`) -> `HalfSideBySide`
///   `ftab`, `f-tab`, `full-tab`, `full.tab`, `fou`, `f-ou`, `full-ou`, `full.ou` -> `FullTopAndBottom`
///   `htab`, `h-tab`, `half-tab`, `half.tab`, `hou`, `h-ou`, `half-ou`, `half.ou`, or bare `tab` / `ou` (when not preceded by `full`/`f`) -> `HalfTopAndBottom`
///   `mvc`, `3d.mvc`, `3d-mvc`, `bluray3d`, `bd3d` -> `Mvc`
/// - `(None, Some("anaglyph"))` for anaglyph releases.
/// - `(None, Some("unspecified_3d"))` for bare `3d` markers without a recognized layout.
/// - `(None, None)` when no 3D indicators are present.
pub fn parse_video_3d_format(input: &str) -> (Option<Video3DFormat>, Option<&'static str>) {
    let tokens = tokenize_3d(input);
    let mut detected_format = None;
    let mut has_anaglyph = false;
    let mut has_bare_3d = false;

    for (i, t) in tokens.iter().enumerate() {
        let prev = i
            .checked_sub(1)
            .and_then(|p| tokens.get(p).map(String::as_str));
        let next = i
            .checked_add(1)
            .and_then(|n| tokens.get(n).map(String::as_str));

        // 1. FullSideBySide: fsbs, f-sbs, full-sbs, full.sbs
        if t == "fsbs"
            || ((t == "full" || t == "f") && next == Some("sbs"))
            || (t == "sbs" && (prev == Some("full") || prev == Some("f")))
        {
            detected_format = Some(Video3DFormat::FullSideBySide);
            break;
        }

        // 2. HalfSideBySide: hsbs, h-sbs, half-sbs, half.sbs, or sbs (when not preceded by full/f)
        if t == "hsbs"
            || ((t == "half" || t == "h") && next == Some("sbs"))
            || (t == "sbs" && prev != Some("full") && prev != Some("f"))
        {
            detected_format = Some(Video3DFormat::HalfSideBySide);
            break;
        }

        // 3. FullTopAndBottom: ftab, f-tab, full-tab, full.tab, fou, f-ou, full-ou, full.ou
        if t == "ftab"
            || t == "fou"
            || ((t == "full" || t == "f") && (next == Some("tab") || next == Some("ou")))
            || ((t == "tab" || t == "ou") && (prev == Some("full") || prev == Some("f")))
        {
            detected_format = Some(Video3DFormat::FullTopAndBottom);
            break;
        }

        // 4. HalfTopAndBottom: htab, h-tab, half-tab, half.tab, hou, h-ou, half-ou, half.ou, or tab / ou (when not preceded by full/f)
        if t == "htab"
            || t == "hou"
            || ((t == "half" || t == "h") && (next == Some("tab") || next == Some("ou")))
            || ((t == "tab" || t == "ou") && prev != Some("full") && prev != Some("f"))
        {
            detected_format = Some(Video3DFormat::HalfTopAndBottom);
            break;
        }

        // 5. Mvc: mvc, 3d.mvc, 3d-mvc, bluray3d, bd3d
        if t == "mvc"
            || t == "bd3d"
            || t == "bluray3d"
            || (t == "3d" && next == Some("mvc"))
            || (t == "bd" && next == Some("3d"))
            || (t == "bluray" && next == Some("3d"))
        {
            detected_format = Some(Video3DFormat::Mvc);
            break;
        }

        if t == "anaglyph" {
            has_anaglyph = true;
        }

        if t == "3d" {
            has_bare_3d = true;
        }
    }

    if let Some(format) = detected_format {
        (Some(format), Some("filename"))
    } else if has_anaglyph {
        (None, Some("anaglyph"))
    } else if has_bare_3d {
        (None, Some("unspecified_3d"))
    } else {
        (None, None)
    }
}

/// LIB-D6 / 018-3d-media: Check whether `input` explicitly specifies a half or full
/// 3D layout qualifier (e.g. `FSBS`, `Full-SBS`, `HSBS`, `Half-SBS`, `FTAB`, `HTAB`,
/// `FOU`, `HOU`), as opposed to a generic unadorned `SBS`, `TAB`, or `OU`.
///
/// Used by scanner probe reconciliation to refine generic 1920x1080 SBS/TAB detections.
pub fn has_explicit_full_or_half_3d_token(input: &str) -> Option<Video3DFormat> {
    let tokens = tokenize_3d(input);
    for (i, t) in tokens.iter().enumerate() {
        let prev = i
            .checked_sub(1)
            .and_then(|p| tokens.get(p).map(String::as_str));
        let next = i
            .checked_add(1)
            .and_then(|n| tokens.get(n).map(String::as_str));

        // Explicit full SBS:
        if t == "fsbs"
            || ((t == "full" || t == "f") && next == Some("sbs"))
            || (t == "sbs" && (prev == Some("full") || prev == Some("f")))
        {
            return Some(Video3DFormat::FullSideBySide);
        }

        // Explicit half SBS:
        if t == "hsbs"
            || ((t == "half" || t == "h") && next == Some("sbs"))
            || (t == "sbs" && (prev == Some("half") || prev == Some("h")))
        {
            return Some(Video3DFormat::HalfSideBySide);
        }

        // Explicit full TAB / OU:
        if t == "ftab"
            || t == "fou"
            || ((t == "full" || t == "f") && (next == Some("tab") || next == Some("ou")))
            || ((t == "tab" || t == "ou") && (prev == Some("full") || prev == Some("f")))
        {
            return Some(Video3DFormat::FullTopAndBottom);
        }

        // Explicit half TAB / OU:
        if t == "htab"
            || t == "hou"
            || ((t == "half" || t == "h") && (next == Some("tab") || next == Some("ou")))
            || ((t == "tab" || t == "ou") && (prev == Some("half") || prev == Some("h")))
        {
            return Some(Video3DFormat::HalfTopAndBottom);
        }
    }
    None
}

/// Tokenize a filename or path into lowercase tokens on boundary characters.
fn tokenize_3d(input: &str) -> Vec<String> {
    let is_sep = |c: char| {
        matches!(
            c,
            '.' | '_' | '-' | ' ' | '[' | ']' | '(' | ')' | '{' | '}' | '/' | '\\'
        )
    };
    input
        .split(is_sep)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

impl MetadataProvider for FilenameProvider {
    fn name(&self) -> &'static str {
        "filename"
    }

    fn priority(&self) -> i32 {
        PRIORITY
    }

    fn supports(&self, kind: MediaKind) -> bool {
        // Title/year/quality from a name is meaningful for movies and
        // episodes. Audio identity is artist/album/track-tag driven
        // (a later slice); skip it here so we don't surface a bogus
        // "title" from a track filename.
        // 004-books / FR-007 — books are admitted so a file whose OPF or
        // ComicInfo.xml carries no title still gets one from its filename. Without
        // this a title-less epub lists untitled, and this predicate is a `matches!`
        // that the compiler never flagged when `Book` was added (R10).
        matches!(
            kind,
            MediaKind::Movie | MediaKind::Episode | MediaKind::Book
        )
    }

    async fn fetch(&self, req: &MetadataRequest<'_>) -> DomainResult<MetadataResult> {
        // IO-free: derive everything from the path's own components. No
        // stat/read, so this never errors (V6 spirit: a weird name yields
        // an empty result, never an abort).
        let Some(stem) = req.path.file_stem().and_then(|s| s.to_str()) else {
            return Ok(MetadataResult::default());
        };
        let with_year = req.kind == MediaKind::Movie;
        let parsed = Self::parse_stem(stem, with_year);

        let mut tags = parsed.tags;
        if let Some(ed) = parsed.edition {
            push_unique(&mut tags, ed);
        }

        Ok(MetadataResult {
            title: parsed.title,
            production_year: parsed.year,
            tags,
            ..MetadataResult::default()
        })
    }
}

/// Replace scene-release separators with spaces so token-splitting and the
/// cleaned title read naturally. `.` and `_` between words become spaces;
/// brackets/parens are dropped to bare tokens.
fn normalise_separators(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '.' | '_' | '[' | ']' | '(' | ')' | '{' | '}' => ' ',
            other => other,
        })
        .collect()
}

/// Map a single token to its canonical quality/source tag, or `None` if it
/// isn't one. Matched case-insensitively; the canonical casing is what
/// lands in [`MetadataResult::tags`].
fn quality_source_tag(word: &str) -> Option<String> {
    let lower = word.to_ascii_lowercase();
    let canonical = match lower.as_str() {
        // Resolutions.
        "2160p" | "4k" | "uhd" => "2160p",
        "1080p" => "1080p",
        "1080i" => "1080i",
        "720p" => "720p",
        "576p" => "576p",
        "480p" => "480p",
        // Sources.
        "bluray" | "blu-ray" | "bdrip" | "brrip" | "bdremux" | "remux" => "BluRay",
        "web-dl" | "webdl" | "webrip" | "web" => "WEB-DL",
        "hdtv" => "HDTV",
        "dvdrip" | "dvd" => "DVD",
        "hdrip" => "HDRip",
        "cam" | "camrip" => "CAM",
        // HDR / codec markers commonly present in scene names.
        "hdr" | "hdr10" => "HDR",
        "dovi" | "dv" => "DolbyVision",
        "x264" | "h264" | "avc" => "H264",
        "x265" | "h265" | "hevc" => "HEVC",
        // 3D format markers (018-3d-media / T134).
        "3d" => "3D",
        "sbs" => "SBS",
        "hsbs" | "h-sbs" | "half-sbs" => "HSBS",
        "fsbs" | "f-sbs" | "full-sbs" => "FSBS",
        "tab" => "TAB",
        "htab" | "h-tab" | "half-tab" => "HTAB",
        "ftab" | "f-tab" | "full-tab" => "FTAB",
        "ou" => "OU",
        "hou" | "h-ou" | "half-ou" => "HOU",
        "fou" | "f-ou" | "full-ou" => "FOU",
        "mvc" | "3d-mvc" => "MVC",
        "bd3d" | "bd-3d" => "BD3D",
        "bluray3d" | "bluray-3d" => "BluRay3D",
        "anaglyph" => "Anaglyph",
        _ => return None,
    };
    Some(canonical.to_string())
}

/// Find a year wrapped in `(...)` or `[...]` anywhere in `stem`
/// (`Blade Runner 2049 (2017) [2160p]` → `2017`). Returns the first such
/// match. The plausible 1800–2999 window guards against `(1)` / `[1080p]`.
fn bracketed_year(stem: &str) -> Option<u32> {
    let bytes = stem.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let open = bytes[i];
        if open == b'(' || open == b'[' {
            let close = if open == b'(' { b')' } else { b']' };
            // Inner must be exactly 4 digits immediately followed by close.
            if i + 5 < bytes.len()
                && bytes[i + 5] == close
                && bytes[i + 1..i + 5].iter().all(u8::is_ascii_digit)
            {
                if let Ok(s) = std::str::from_utf8(&bytes[i + 1..i + 5]) {
                    if let Ok(year) = s.parse::<u32>() {
                        if (1800..3000).contains(&year) {
                            return Some(year);
                        }
                    }
                }
            }
        }
        i += 1;
    }
    None
}

/// A bare 4-digit year token within the plausible 1800–2999 window.
/// Mirrors the validation in [`fs::parse_folder_year`] but for a token
/// that isn't parenthesised (`The.Matrix.1999.1080p`).
fn bare_year(word: &str) -> Option<u32> {
    if word.len() != 4 || !word.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year: u32 = word.parse().ok()?;
    (1800..3000).contains(&year).then_some(year)
}

/// Whether a single normalised token is (part of) a known edition word.
/// Used to stop title accumulation at an edition token even when it isn't
/// in the ` - <edition>` trailing form `split_edition_tag` keys on.
fn is_edition_word(word: &str) -> bool {
    fs::is_known_edition(word)
}

/// Append `value` to `tags` only if not already present (stable, first-seen
/// order). Case-sensitive on the canonical form, which is fixed per token.
fn push_unique(tags: &mut Vec<String>, value: String) {
    if !tags.iter().any(|t| t == &value) {
        tags.push(value);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use pharos_core::{MediaProbe, SeriesInfo};
    use std::path::Path;

    /// Table-driven: `input stem` → expected `(title, year, tags)`.
    /// `with_year = true` (movie semantics) for every row here.
    #[test]
    fn parse_stem_table() {
        struct Case {
            input: &'static str,
            title: Option<&'static str>,
            year: Option<u32>,
            tags: &'static [&'static str],
        }
        let cases = [
            Case {
                input: "The Matrix (1999)",
                title: Some("The Matrix"),
                year: Some(1999),
                tags: &[],
            },
            Case {
                input: "The.Matrix.1999.1080p.BluRay.x264",
                title: Some("The Matrix"),
                year: Some(1999),
                tags: &["1080p", "BluRay", "H264"],
            },
            Case {
                input: "Blade Runner 2049 (2017) [2160p] WEB-DL",
                title: Some("Blade Runner 2049"),
                year: Some(2017),
                tags: &["2160p", "WEB-DL"],
            },
            // Edition trailing form: title stops before the edition; the
            // edition is reported separately, not in the title.
            Case {
                input: "Apocalypse Now - Redux",
                title: Some("Apocalypse Now"),
                year: None,
                tags: &[],
            },
            // Title containing digits that are NOT a year, plus an HDTV
            // episode-ish token — bare 4-digit year still wins only when
            // in 1800–2999.
            Case {
                input: "2001 A Space Odyssey (1968)",
                title: Some("2001 A Space Odyssey"),
                year: Some(1968),
                tags: &[],
            },
            // No year, no tags — clean title passthrough.
            Case {
                input: "Amelie",
                title: Some("Amelie"),
                year: None,
                tags: &[],
            },
            // Bracketed quality with no year.
            Case {
                input: "Some_Movie_720p_HDTV",
                title: Some("Some Movie"),
                year: None,
                tags: &["720p", "HDTV"],
            },
            // A parenthesised non-year must not masquerade as a year.
            Case {
                input: "Director Commentary (Uncut)",
                title: Some("Director Commentary"),
                year: None,
                // "uncut" is a known edition → not a title word, not a tag.
                tags: &[],
            },
            // 4K alias → 2160p; dedupe a repeated source token.
            Case {
                input: "Dune.2021.4K.UHD.BluRay.BluRay",
                title: Some("Dune"),
                year: Some(2021),
                tags: &["2160p", "BluRay"],
            },
        ];

        for c in cases {
            let parsed = FilenameProvider::parse_stem(c.input, true);
            assert_eq!(
                parsed.title.as_deref(),
                c.title,
                "title mismatch for {:?}",
                c.input
            );
            assert_eq!(parsed.year, c.year, "year mismatch for {:?}", c.input);
            let got: Vec<&str> = parsed.tags.iter().map(String::as_str).collect();
            assert_eq!(got, c.tags, "tags mismatch for {:?}", c.input);
        }
    }

    /// Episodes: C11 owns the show year, so the filename provider must NOT
    /// emit `production_year` — but still cleans the title + quality tags.
    #[test]
    fn parse_stem_episode_suppresses_year() {
        let parsed = FilenameProvider::parse_stem("The Show 2018 720p", false);
        assert_eq!(parsed.year, None);
        let got: Vec<&str> = parsed.tags.iter().map(String::as_str).collect();
        assert_eq!(got, ["720p"]);
    }

    /// The trailing ` - <edition>` form is recognised and reported on
    /// `ParsedName::edition` (shared with `fs::split_edition_tag`).
    #[test]
    fn parse_stem_reports_edition() {
        let parsed = FilenameProvider::parse_stem("Aliens - Director's Cut", true);
        assert_eq!(parsed.edition.as_deref(), Some("Director's Cut"));
        assert_eq!(parsed.title.as_deref(), Some("Aliens"));
    }

    fn req<'a>(path: &'a Path, kind: MediaKind, probe: &'a MediaProbe) -> MetadataRequest<'a> {
        MetadataRequest {
            path,
            kind,
            probe,
            series: None,
        }
    }

    #[tokio::test]
    async fn fetch_movie_populates_result() {
        let probe = MediaProbe::default();
        let provider = FilenameProvider::new();
        let path = Path::new("/m/The.Matrix.1999.1080p.BluRay.mkv");
        let r = provider
            .fetch(&req(path, MediaKind::Movie, &probe))
            .await
            .unwrap();
        assert_eq!(r.title.as_deref(), Some("The Matrix"));
        assert_eq!(r.production_year, Some(1999));
        assert!(r.tags.iter().any(|t| t == "1080p"));
        assert!(r.tags.iter().any(|t| t == "BluRay"));
    }

    #[tokio::test]
    async fn fetch_episode_no_year() {
        let probe = MediaProbe::default();
        let provider = FilenameProvider::new();
        let path = Path::new("/tv/Show/Season 01/Show.2018.S01E02.720p.mkv");
        let r = provider
            .fetch(&req(path, MediaKind::Episode, &probe))
            .await
            .unwrap();
        // Year suppressed for episodes (C11 owns it).
        assert_eq!(r.production_year, None);
        assert!(r.tags.iter().any(|t| t == "720p"));
    }

    #[tokio::test]
    async fn fetch_audio_unsupported() {
        let provider = FilenameProvider::new();
        assert!(!provider.supports(MediaKind::Audio));
        assert!(provider.supports(MediaKind::Movie));
        assert!(provider.supports(MediaKind::Episode));
    }

    #[tokio::test]
    async fn fetch_edition_becomes_tag() {
        let probe = MediaProbe::default();
        let provider = FilenameProvider::new();
        let path = Path::new("/m/Apocalypse Now - Redux.mkv");
        let r = provider
            .fetch(&req(path, MediaKind::Movie, &probe))
            .await
            .unwrap();
        assert_eq!(r.title.as_deref(), Some("Apocalypse Now"));
        assert!(r.tags.iter().any(|t| t == "Redux"));
    }

    /// Sanity: a stem with no usable parts yields an all-empty result and
    /// never panics (V6 spirit) — series field is accepted but unused.
    #[tokio::test]
    async fn fetch_empty_stem_is_empty() {
        let probe = MediaProbe::default();
        let series = SeriesInfo::default();
        let provider = FilenameProvider::new();
        let path = Path::new("/m/2160p.mkv");
        let r = MetadataProvider::fetch(
            &provider,
            &MetadataRequest {
                path,
                kind: MediaKind::Movie,
                probe: &probe,
                series: Some(&series),
            },
        )
        .await
        .unwrap();
        assert_eq!(r.title, None);
        assert!(r.tags.iter().any(|t| t == "2160p"));
    }

    #[test]
    fn parse_video_3d_format_movies_table() {
        struct Case {
            input: &'static str,
            format: Option<Video3DFormat>,
            source: Option<&'static str>,
        }
        let cases = [
            // HSBS variants
            Case {
                input: "Avatar (2009) [1080p] HSBS",
                format: Some(Video3DFormat::HalfSideBySide),
                source: Some("filename"),
            },
            Case {
                input: "Avatar.2009.H-SBS.1080p",
                format: Some(Video3DFormat::HalfSideBySide),
                source: Some("filename"),
            },
            Case {
                input: "Avatar (2009) Half-SBS",
                format: Some(Video3DFormat::HalfSideBySide),
                source: Some("filename"),
            },
            Case {
                input: "Avatar.2009.Half.SBS.1080p",
                format: Some(Video3DFormat::HalfSideBySide),
                source: Some("filename"),
            },
            Case {
                input: "Avatar (2009) SBS 1080p",
                format: Some(Video3DFormat::HalfSideBySide),
                source: Some("filename"),
            },
            // FSBS variants
            Case {
                input: "Avatar (2009) FSBS 1080p",
                format: Some(Video3DFormat::FullSideBySide),
                source: Some("filename"),
            },
            Case {
                input: "Avatar.2009.F-SBS.1080p",
                format: Some(Video3DFormat::FullSideBySide),
                source: Some("filename"),
            },
            Case {
                input: "Avatar (2009) Full-SBS 1080p",
                format: Some(Video3DFormat::FullSideBySide),
                source: Some("filename"),
            },
            Case {
                input: "Avatar.2009.Full.SBS.1080p",
                format: Some(Video3DFormat::FullSideBySide),
                source: Some("filename"),
            },
            // HTAB / TAB variants
            Case {
                input: "Avatar.2009.HTAB.1080p",
                format: Some(Video3DFormat::HalfTopAndBottom),
                source: Some("filename"),
            },
            Case {
                input: "Avatar.2009.H-TAB.1080p",
                format: Some(Video3DFormat::HalfTopAndBottom),
                source: Some("filename"),
            },
            Case {
                input: "Avatar.2009.Half-TAB.1080p",
                format: Some(Video3DFormat::HalfTopAndBottom),
                source: Some("filename"),
            },
            Case {
                input: "Avatar.2009.Half.TAB.1080p",
                format: Some(Video3DFormat::HalfTopAndBottom),
                source: Some("filename"),
            },
            Case {
                input: "Avatar (2009) TAB 1080p",
                format: Some(Video3DFormat::HalfTopAndBottom),
                source: Some("filename"),
            },
            // HOU / OU variants
            Case {
                input: "Avatar.2009.HOU.1080p",
                format: Some(Video3DFormat::HalfTopAndBottom),
                source: Some("filename"),
            },
            Case {
                input: "Avatar.2009.H-OU.1080p",
                format: Some(Video3DFormat::HalfTopAndBottom),
                source: Some("filename"),
            },
            Case {
                input: "Avatar.2009.Half-OU.1080p",
                format: Some(Video3DFormat::HalfTopAndBottom),
                source: Some("filename"),
            },
            Case {
                input: "Avatar.2009.Half.OU.1080p",
                format: Some(Video3DFormat::HalfTopAndBottom),
                source: Some("filename"),
            },
            Case {
                input: "Avatar (2009) OU 1080p",
                format: Some(Video3DFormat::HalfTopAndBottom),
                source: Some("filename"),
            },
            // FTAB variants
            Case {
                input: "Avatar.2009.FTAB.1080p",
                format: Some(Video3DFormat::FullTopAndBottom),
                source: Some("filename"),
            },
            Case {
                input: "Avatar.2009.F-TAB.1080p",
                format: Some(Video3DFormat::FullTopAndBottom),
                source: Some("filename"),
            },
            Case {
                input: "Avatar (2009) Full-TAB 1080p",
                format: Some(Video3DFormat::FullTopAndBottom),
                source: Some("filename"),
            },
            Case {
                input: "Avatar.2009.Full.TAB.1080p",
                format: Some(Video3DFormat::FullTopAndBottom),
                source: Some("filename"),
            },
            // FOU variants
            Case {
                input: "Avatar.2009.FOU.1080p",
                format: Some(Video3DFormat::FullTopAndBottom),
                source: Some("filename"),
            },
            Case {
                input: "Avatar.2009.F-OU.1080p",
                format: Some(Video3DFormat::FullTopAndBottom),
                source: Some("filename"),
            },
            Case {
                input: "Avatar.2009.Full-OU.1080p",
                format: Some(Video3DFormat::FullTopAndBottom),
                source: Some("filename"),
            },
            Case {
                input: "Avatar.2009.Full.OU.1080p",
                format: Some(Video3DFormat::FullTopAndBottom),
                source: Some("filename"),
            },
            // MVC / BD3D / BluRay3D variants
            Case {
                input: "Avatar (2009) MVC 1080p",
                format: Some(Video3DFormat::Mvc),
                source: Some("filename"),
            },
            Case {
                input: "Avatar.2009.3D.MVC.1080p",
                format: Some(Video3DFormat::Mvc),
                source: Some("filename"),
            },
            Case {
                input: "Avatar.2009.3D-MVC.1080p",
                format: Some(Video3DFormat::Mvc),
                source: Some("filename"),
            },
            Case {
                input: "Avatar.2009.BD3D.1080p",
                format: Some(Video3DFormat::Mvc),
                source: Some("filename"),
            },
            Case {
                input: "Avatar (2009) BluRay3D 1080p",
                format: Some(Video3DFormat::Mvc),
                source: Some("filename"),
            },
            // Anaglyph
            Case {
                input: "Creature from the Black Lagoon (1954) [Anaglyph] 1080p",
                format: None,
                source: Some("anaglyph"),
            },
            // Bare 3D
            Case {
                input: "Avatar (2009) [3D] 1080p",
                format: None,
                source: Some("unspecified_3d"),
            },
            // 2D Stems
            Case {
                input: "The Matrix (1999) 1080p",
                format: None,
                source: None,
            },
            Case {
                input: "Inception.2010.1080p.BluRay.x264",
                format: None,
                source: None,
            },
        ];

        for c in cases {
            let got = parse_video_3d_format(c.input);
            assert_eq!(
                got,
                (c.format, c.source),
                "parse_video_3d_format failed for {:?}",
                c.input
            );
        }
    }

    #[test]
    fn parse_filename_strips_3d_tags_movies_table() {
        struct Case {
            input: &'static str,
            title: Option<&'static str>,
            year: Option<u32>,
            tag: &'static str,
        }
        let cases = [
            Case {
                input: "Avatar (2009) HSBS",
                title: Some("Avatar"),
                year: Some(2009),
                tag: "HSBS",
            },
            Case {
                input: "Avatar.2009.H-SBS.1080p",
                title: Some("Avatar"),
                year: Some(2009),
                tag: "HSBS",
            },
            Case {
                input: "Avatar (2009) Half-SBS",
                title: Some("Avatar"),
                year: Some(2009),
                tag: "HSBS",
            },
            Case {
                input: "Avatar.2009.Half.SBS.1080p",
                title: Some("Avatar"),
                year: Some(2009),
                tag: "HSBS",
            },
            Case {
                input: "Avatar (2009) SBS",
                title: Some("Avatar"),
                year: Some(2009),
                tag: "SBS",
            },
            Case {
                input: "Avatar.2009.FSBS.1080p",
                title: Some("Avatar"),
                year: Some(2009),
                tag: "FSBS",
            },
            Case {
                input: "Avatar (2009) Full-SBS",
                title: Some("Avatar"),
                year: Some(2009),
                tag: "FSBS",
            },
            Case {
                input: "Avatar.2009.Full.SBS.1080p",
                title: Some("Avatar"),
                year: Some(2009),
                tag: "FSBS",
            },
            Case {
                input: "Avatar.2009.HTAB.1080p",
                title: Some("Avatar"),
                year: Some(2009),
                tag: "HTAB",
            },
            Case {
                input: "Avatar (2009) TAB",
                title: Some("Avatar"),
                year: Some(2009),
                tag: "TAB",
            },
            Case {
                input: "Avatar.2009.HOU.1080p",
                title: Some("Avatar"),
                year: Some(2009),
                tag: "HOU",
            },
            Case {
                input: "Avatar (2009) OU",
                title: Some("Avatar"),
                year: Some(2009),
                tag: "OU",
            },
            Case {
                input: "Avatar.2009.FTAB.1080p",
                title: Some("Avatar"),
                year: Some(2009),
                tag: "FTAB",
            },
            Case {
                input: "Avatar (2009) Full-TAB",
                title: Some("Avatar"),
                year: Some(2009),
                tag: "FTAB",
            },
            Case {
                input: "Avatar.2009.FOU.1080p",
                title: Some("Avatar"),
                year: Some(2009),
                tag: "FOU",
            },
            Case {
                input: "Avatar (2009) MVC",
                title: Some("Avatar"),
                year: Some(2009),
                tag: "MVC",
            },
            Case {
                input: "Avatar.2009.BD3D.1080p",
                title: Some("Avatar"),
                year: Some(2009),
                tag: "BD3D",
            },
            Case {
                input: "Avatar (2009) BluRay3D",
                title: Some("Avatar"),
                year: Some(2009),
                tag: "BluRay3D",
            },
            Case {
                input: "Creature from the Black Lagoon (1954) Anaglyph 1080p",
                title: Some("Creature from the Black Lagoon"),
                year: Some(1954),
                tag: "Anaglyph",
            },
            Case {
                input: "Avatar (2009) 3D 1080p",
                title: Some("Avatar"),
                year: Some(2009),
                tag: "3D",
            },
        ];

        for c in cases {
            let parsed = parse_filename(c.input, true);
            assert_eq!(
                parsed.title.as_deref(),
                c.title,
                "title mismatch for {:?}",
                c.input
            );
            assert_eq!(parsed.year, c.year, "year mismatch for {:?}", c.input);
            assert!(
                parsed.tags.iter().any(|t| t == c.tag),
                "expected tag {:?} in parsed.tags {:?} for {:?}",
                c.tag,
                parsed.tags,
                c.input
            );
        }
    }

    #[test]
    fn parse_video_3d_format_and_title_episodes() {
        // Episode 1: Doctor Who
        let ep1 = "Doctor Who - S07E14 - The Day of the Doctor [1080p.HTAB]";
        let (fmt1, src1) = parse_video_3d_format(ep1);
        assert_eq!(fmt1, Some(Video3DFormat::HalfTopAndBottom));
        assert_eq!(src1, Some("filename"));

        let parsed1 = parse_filename(ep1, false);
        assert_eq!(
            parsed1.title.as_deref(),
            Some("Doctor Who - S07E14 - The Day of the Doctor")
        );
        assert_eq!(parsed1.year, None);
        assert!(parsed1.tags.contains(&"1080p".to_string()));
        assert!(parsed1.tags.contains(&"HTAB".to_string()));

        // Episode 2: Show HSBS
        let ep2 = "Show.S01E02.1080p.HSBS";
        let (fmt2, src2) = parse_video_3d_format(ep2);
        assert_eq!(fmt2, Some(Video3DFormat::HalfSideBySide));
        assert_eq!(src2, Some("filename"));

        let parsed2 = parse_filename(ep2, false);
        assert_eq!(parsed2.title.as_deref(), Some("Show S01E02"));
        assert_eq!(parsed2.year, None);
        assert!(parsed2.tags.contains(&"1080p".to_string()));
        assert!(parsed2.tags.contains(&"HSBS".to_string()));

        // Episode 3: Show 3D MVC
        let ep3 = "Show.S01E02.3D.MVC.1080p";
        let (fmt3, src3) = parse_video_3d_format(ep3);
        assert_eq!(fmt3, Some(Video3DFormat::Mvc));
        assert_eq!(src3, Some("filename"));

        let parsed3 = parse_filename(ep3, false);
        assert_eq!(parsed3.title.as_deref(), Some("Show S01E02"));
        assert_eq!(parsed3.year, None);
        assert!(parsed3.tags.contains(&"3D".to_string()));
        assert!(parsed3.tags.contains(&"MVC".to_string()));
        assert!(parsed3.tags.contains(&"1080p".to_string()));
    }

    #[test]
    fn test_has_explicit_full_or_half_3d_token() {
        assert_eq!(
            has_explicit_full_or_half_3d_token("Avatar.FSBS.1080p"),
            Some(Video3DFormat::FullSideBySide)
        );
        assert_eq!(
            has_explicit_full_or_half_3d_token("Avatar.Full-SBS.1080p"),
            Some(Video3DFormat::FullSideBySide)
        );
        assert_eq!(
            has_explicit_full_or_half_3d_token("Avatar.HSBS.1080p"),
            Some(Video3DFormat::HalfSideBySide)
        );
        assert_eq!(
            has_explicit_full_or_half_3d_token("Avatar.Half-SBS.1080p"),
            Some(Video3DFormat::HalfSideBySide)
        );
        assert_eq!(has_explicit_full_or_half_3d_token("Avatar.SBS.1080p"), None);
        assert_eq!(
            has_explicit_full_or_half_3d_token("Avatar.FTAB.1080p"),
            Some(Video3DFormat::FullTopAndBottom)
        );
        assert_eq!(
            has_explicit_full_or_half_3d_token("Avatar.Full-TAB.1080p"),
            Some(Video3DFormat::FullTopAndBottom)
        );
        assert_eq!(
            has_explicit_full_or_half_3d_token("Avatar.HTAB.1080p"),
            Some(Video3DFormat::HalfTopAndBottom)
        );
        assert_eq!(
            has_explicit_full_or_half_3d_token("Avatar.Half-TAB.1080p"),
            Some(Video3DFormat::HalfTopAndBottom)
        );
        assert_eq!(has_explicit_full_or_half_3d_token("Avatar.TAB.1080p"), None);
        assert_eq!(
            has_explicit_full_or_half_3d_token("Avatar.FOU.1080p"),
            Some(Video3DFormat::FullTopAndBottom)
        );
        assert_eq!(
            has_explicit_full_or_half_3d_token("Avatar.HOU.1080p"),
            Some(Video3DFormat::HalfTopAndBottom)
        );
        assert_eq!(has_explicit_full_or_half_3d_token("Avatar.OU.1080p"), None);
        assert_eq!(
            has_explicit_full_or_half_3d_token("Avatar.3D.MVC.1080p"),
            None
        );
    }
}
