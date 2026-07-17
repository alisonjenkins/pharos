//! B90 — embedded-container-tag metadata provider.
//!
//! Files without a sidecar `.nfo` still often carry descriptive tags baked
//! into the container: a movie MKV with a `SYNOPSIS`/`DESCRIPTION` tag, an
//! MP4 with an iTunes `synopsis` / `contentRating`, a TV rip with a `NETWORK`
//! tag. The [`FfmpegProber`] already lifts these into
//! [`MediaProbe`](pharos_core::MediaProbe) (`synopsis` / `content_rating` /
//! `network` / `release_date`, alongside the pre-existing `genre` / `year`);
//! this provider maps them into a [`MetadataResult`] so the resolver merges
//! them like any other source.
//!
//! Priority [`PRIORITY`] sits **below** NFO (100) and sidecar (50) but above
//! the filename provider (10): a user's curated NFO always wins a scalar, but
//! an embedded synopsis beats a bare filename. IO-free (reads only the already
//! -probed [`MetadataRequest::probe`]), so [`fetch`](EmbeddedTagProvider::fetch)
//! never errors — the closest structural sibling of the filename provider.

use pharos_core::{DomainResult, MediaKind, MetadataProvider, MetadataRequest, MetadataResult};

use super::nfo::parse_date_unix;

/// Merge priority — below NFO (100) / sidecar (50), above filename (10).
pub const PRIORITY: i32 = 30;

/// Stateless embedded-tag provider.
#[derive(Debug, Default, Clone, Copy)]
pub struct EmbeddedTagProvider;

impl EmbeddedTagProvider {
    pub fn new() -> Self {
        Self
    }
}

/// Whether a raw `content_rating` tag looks like a real certification
/// (`"PG-13"`, `"TV-14"`, `"R"`) rather than an iTunes numeric flag (`"0"` =
/// none, `"1"` = clean, `"2"` = explicit) or a star count. A purely-numeric
/// value is dropped so a music file's explicit-lyrics flag never surfaces as a
/// parental rating.
fn is_certification(raw: &str) -> bool {
    let t = raw.trim();
    !t.is_empty() && !t.bytes().all(|b| b.is_ascii_digit())
}

impl MetadataProvider for EmbeddedTagProvider {
    fn name(&self) -> &'static str {
        "embedded"
    }

    fn priority(&self) -> i32 {
        PRIORITY
    }

    fn supports(&self, _kind: MediaKind) -> bool {
        // Embedded descriptive tags exist across container types (a movie MKV,
        // an episode MP4, a tagged song); the per-field extraction is what
        // decides what's actually present.
        true
    }

    async fn fetch(&self, req: &MetadataRequest<'_>) -> DomainResult<MetadataResult> {
        let p = req.probe;
        let overview = p
            .synopsis
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let official_rating = p
            .content_rating
            .as_deref()
            .map(str::trim)
            .filter(|s| is_certification(s))
            .map(str::to_string);
        let studios = p
            .network
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| vec![s.to_string()])
            .unwrap_or_default();
        Ok(MetadataResult {
            overview,
            official_rating,
            studios,
            production_year: p.year,
            // PremiereDate only from a FULL date (YYYY-MM-DD); a year-only tag
            // leaves it unset (production_year already carries the year).
            premiere_date: p.release_date.as_deref().and_then(parse_date_unix),
            ..Default::default()
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use pharos_core::MediaProbe;

    fn req_with<'a>(probe: &'a MediaProbe, path: &'a std::path::Path) -> MetadataRequest<'a> {
        MetadataRequest {
            path,
            kind: MediaKind::Movie,
            probe,
            series: None,
        }
    }

    #[tokio::test]
    async fn maps_embedded_tags_into_result() {
        let probe = MediaProbe {
            synopsis: Some("  A hero's journey.  ".into()),
            content_rating: Some("PG-13".into()),
            network: Some("HBO".into()),
            year: Some(2003),
            release_date: Some("2003-09-22".into()),
            ..Default::default()
        };
        let path = std::path::Path::new("/m/x.mkv");
        let r = EmbeddedTagProvider::new()
            .fetch(&req_with(&probe, path))
            .await
            .unwrap();
        assert_eq!(r.overview.as_deref(), Some("A hero's journey."));
        assert_eq!(r.official_rating.as_deref(), Some("PG-13"));
        assert_eq!(r.studios, vec!["HBO".to_string()]);
        assert_eq!(r.production_year, Some(2003));
        // 2003-09-22 UTC midnight.
        assert_eq!(r.premiere_date, Some(1_064_188_800));
    }

    #[tokio::test]
    async fn drops_numeric_content_rating_and_year_only_date() {
        // iTunes explicit-lyrics flag "2" is not a certification; a year-only
        // release tag must not fabricate a PremiereDate.
        let probe = MediaProbe {
            content_rating: Some("2".into()),
            release_date: Some("1999".into()),
            year: Some(1999),
            ..Default::default()
        };
        let path = std::path::Path::new("/m/song.flac");
        let r = EmbeddedTagProvider::new()
            .fetch(&req_with(&probe, path))
            .await
            .unwrap();
        assert_eq!(r.official_rating, None, "numeric flag is not a rating");
        assert_eq!(
            r.premiere_date, None,
            "year-only tag leaves PremiereDate unset"
        );
        assert_eq!(r.production_year, Some(1999));
    }

    #[tokio::test]
    async fn empty_probe_yields_empty_result() {
        let probe = MediaProbe::default();
        let path = std::path::Path::new("/m/x.mkv");
        let r = EmbeddedTagProvider::new()
            .fetch(&req_with(&probe, path))
            .await
            .unwrap();
        assert_eq!(r, MetadataResult::default());
    }
}
