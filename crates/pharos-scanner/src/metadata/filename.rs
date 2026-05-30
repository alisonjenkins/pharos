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

use pharos_core::{DomainResult, MediaKind, MetadataProvider, MetadataRequest, MetadataResult};

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
        for (idx, word) in words.iter().enumerate() {
            if let Some(tag) = quality_source_tag(word) {
                push_unique(&mut tags, tag);
                if !boundary_hit {
                    title_end = idx;
                    boundary_hit = true;
                }
                continue;
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
                    continue;
                }
                // No parenthesised year: a leading year is part of the title;
                // the first later year delimits it and becomes the year.
                if idx > 0 && !boundary_hit {
                    bare_year_val = Some(y);
                    title_end = idx;
                    boundary_hit = true;
                }
                continue;
            }
            // A standalone known-edition word (e.g. "Uncut", "Remastered"
            // not in the trailing ` - ` form) also bounds the title. The
            // edition label itself isn't surfaced here (only the trailing
            // ` - <edition>` form populates `edition`); it's simply trimmed.
            if !boundary_hit && idx > 0 && is_edition_word(word) {
                title_end = idx;
                boundary_hit = true;
                continue;
            }
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
        matches!(kind, MediaKind::Movie | MediaKind::Episode)
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
}
