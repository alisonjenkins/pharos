//! LIB-D2 — Kodi NFO reader as the high-priority [`MetadataProvider`].
//!
//! [`NfoProvider`] locates the Kodi-convention sidecar `.nfo` for a media
//! item and parses the common fields into a [`MetadataResult`]. NFOs are
//! **user-authored** (hand-edited or written by a scraper the user trusts),
//! so this provider sits at [`PRIORITY`] — *above* the filename provider
//! and above any future online provider — so a curated local edit always
//! wins the scalar merge.
//!
//! ## NFO location (Kodi conventions)
//! - **Movie**: `<basename>.nfo` beside the file, else `movie.nfo` in the
//!   same directory.
//! - **Episode**: `<basename>.nfo` beside the file.
//! - **Series-level** (an episode whose request carries a
//!   [`SeriesInfo::series_folder`]): `tvshow.nfo` in the show folder is
//!   *also* read and merged underneath the episode NFO, so show-level
//!   fields (studio, content rating, show overview) fill gaps the episode
//!   NFO leaves blank.
//! - **Audio**: best-effort `album.nfo` / `artist.nfo` in the track's
//!   directory.
//!
//! ## Fields mapped
//! `title`, `originaltitle` (fills `title` if `<title>` absent), `plot` /
//! `outline` → `overview`, `tagline`, `year` → `production_year`,
//! `premiered` / `aired` / `releasedate` → `premiere_date` (parsed to
//! unix-seconds), `rating` / `<ratings>` → `community_rating`,
//! `criticrating` → `critic_rating`, `mpaa` / `certification` →
//! `official_rating`, repeated `<genre>` → genres, repeated `<studio>` →
//! studios, repeated `<tag>` → tags, `<set>` / `<collection>` →
//! collections, `<actor>` → people (Actor), `<director>` / `<credits>` →
//! people, `<uniqueid type=...>` / `<id>` / `<imdbid>` → provider_ids,
//! `<thumb>` / `<fanart>` → artwork.
//!
//! ## V6 tolerance
//! Missing/extra/unknown elements are ignored. An **absent** NFO yields
//! `Ok(MetadataResult::default())` (a no-op the resolver merges to
//! nothing). A **malformed/truncated** NFO yields `Err` — the resolver
//! logs at `warn` and skips it, never aborting the scan. The parser never
//! panics.

use std::path::{Path, PathBuf};

use pharos_core::{
    ArtworkRef, ArtworkRole, ArtworkSource, DomainError, DomainResult, MediaKind, MetadataProvider,
    MetadataRequest, MetadataResult, PersonKind, PersonRef, ProviderIds,
};
use quick_xml::events::Event;
use quick_xml::Reader;

/// Merge priority for the NFO source — the highest local source. A
/// user-authored `<title>`/`<plot>`/`<rating>` beats the filename provider
/// (priority 10) and any future online provider, so local curation wins.
pub const PRIORITY: i32 = 100;

/// LIB-D2 — reads Kodi NFO sidecars into a [`MetadataResult`]. Stateless;
/// cheap to clone. The IO (locate + read the `.nfo`) runs in the scanner's
/// parallel probe phase (V5: off the async reactor).
#[derive(Debug, Clone, Copy, Default)]
pub struct NfoProvider;

impl NfoProvider {
    /// Construct the provider. Stateless — `NfoProvider` and
    /// `NfoProvider::new()` are equivalent.
    pub fn new() -> Self {
        Self
    }
}

impl MetadataProvider for NfoProvider {
    fn name(&self) -> &'static str {
        "nfo"
    }

    fn priority(&self) -> i32 {
        PRIORITY
    }

    fn supports(&self, _kind: MediaKind) -> bool {
        // NFO conventions exist for every kind (movie / episode / tvshow /
        // album / artist), so the provider supports all of them; the
        // per-kind file-location logic decides what to read.
        true
    }

    async fn fetch(&self, req: &MetadataRequest<'_>) -> DomainResult<MetadataResult> {
        // Episode NFO first (most specific), then merge the show-level
        // tvshow.nfo beneath it (fills gaps). For movies/audio there's a
        // single candidate set. A read/parse error on any candidate aborts
        // *this provider* with Err (resolver logs + skips, V6); an absent
        // file is simply skipped (no-op).
        let mut result = MetadataResult::default();

        for candidate in nfo_candidates(req) {
            match std::fs::read(&candidate) {
                Ok(bytes) => {
                    let parsed = parse_nfo(&bytes, &candidate)?;
                    fold_under(&mut result, parsed);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Expected miss — try the next candidate.
                    continue;
                }
                Err(e) => {
                    // A real IO error (permissions, etc.): surface as Err so
                    // the resolver logs + skips this provider for the item.
                    return Err(DomainError::Backend(format!(
                        "nfo read failed for {}: {e}",
                        candidate.display()
                    )));
                }
            }
        }

        Ok(result)
    }
}

/// The ordered NFO candidate paths for `req`. The first candidate is the
/// most specific (item-level); later candidates (e.g. `tvshow.nfo`) are
/// merged *underneath* (gap-filling). Files that don't exist are simply
/// skipped by [`NfoProvider::fetch`].
fn nfo_candidates(req: &MetadataRequest<'_>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let dir = req.path.parent();

    match req.kind {
        MediaKind::Movie => {
            if let Some(p) = sibling_nfo(req.path) {
                out.push(p);
            }
            if let Some(d) = dir {
                out.push(d.join("movie.nfo"));
            }
        }
        MediaKind::Episode => {
            if let Some(p) = sibling_nfo(req.path) {
                out.push(p);
            }
            // Series-level: tvshow.nfo in the show folder (merged under).
            if let Some(folder) = req.series.and_then(|s| s.series_folder.as_deref()) {
                out.push(Path::new(folder).join("tvshow.nfo"));
            }
        }
        MediaKind::Audio => {
            // Best-effort: track-level sidecar, then album/artist NFOs in
            // the track's directory.
            if let Some(p) = sibling_nfo(req.path) {
                out.push(p);
            }
            if let Some(d) = dir {
                out.push(d.join("album.nfo"));
                out.push(d.join("artist.nfo"));
            }
        }
    }

    out
}

/// `<basename>.nfo` beside `path` (same directory, file stem + `.nfo`).
fn sibling_nfo(path: &Path) -> Option<PathBuf> {
    Some(path.with_extension("nfo"))
}

/// Fold `under` beneath `acc`: `acc` (more-specific source) keeps its set
/// scalars; `under` fills the gaps and its `Vec`s are appended (deduped).
/// Mirrors the resolver's first-`Some`-wins / union semantics so a
/// `tvshow.nfo` can backfill an episode NFO without overriding it.
fn fold_under(acc: &mut MetadataResult, under: MetadataResult) {
    fill(&mut acc.title, under.title);
    fill(&mut acc.overview, under.overview);
    fill(&mut acc.tagline, under.tagline);
    fill(&mut acc.production_year, under.production_year);
    fill(&mut acc.premiere_date, under.premiere_date);
    fill(&mut acc.community_rating, under.community_rating);
    fill(&mut acc.critic_rating, under.critic_rating);
    fill(&mut acc.official_rating, under.official_rating);
    fill(&mut acc.provider_ids.tmdb, under.provider_ids.tmdb);
    fill(&mut acc.provider_ids.tvdb, under.provider_ids.tvdb);
    fill(&mut acc.provider_ids.imdb, under.provider_ids.imdb);
    fill(&mut acc.provider_ids.mbid, under.provider_ids.mbid);

    extend_str(&mut acc.genres, under.genres);
    extend_str(&mut acc.studios, under.studios);
    extend_str(&mut acc.tags, under.tags);
    extend_str(&mut acc.collections, under.collections);
    for p in under.people {
        if !acc
            .people
            .iter()
            .any(|e| e.name == p.name && e.kind == p.kind && e.character == p.character)
        {
            acc.people.push(p);
        }
    }
    for a in under.artwork {
        if !acc
            .artwork
            .iter()
            .any(|e| e.role == a.role && e.source == a.source)
        {
            acc.artwork.push(a);
        }
    }
}

fn fill<T>(slot: &mut Option<T>, value: Option<T>) {
    if slot.is_none() {
        *slot = value;
    }
}

fn extend_str(acc: &mut Vec<String>, next: Vec<String>) {
    for s in next {
        if !acc.iter().any(|e| e == &s) {
            acc.push(s);
        }
    }
}

/// Parse a Kodi NFO blob into a [`MetadataResult`]. Tolerant: unknown
/// elements are ignored; missing fields stay `None`/empty. Returns `Err`
/// only on a genuinely malformed/truncated document (the quick-xml reader
/// reports an unrecoverable syntax error). Never panics.
fn parse_nfo(bytes: &[u8], path: &Path) -> DomainResult<MetadataResult> {
    let mut reader = Reader::from_reader(bytes);
    let cfg = reader.config_mut();
    cfg.trim_text(true);
    cfg.check_end_names = false;

    let mut result = MetadataResult::default();
    // The text of the element we're currently inside (last `<tag>` opened).
    let mut cur_tag: Vec<u8> = Vec::new();
    // Original-title fallback (only applied to `title` if `<title>` absent).
    let mut original_title: Option<String> = None;
    // Pending actor being assembled across child elements.
    let mut actor: Option<ActorBuilder> = None;
    // `<uniqueid type="...">` type for the next text run.
    let mut uniqueid_type: Option<String> = None;
    // Whether we are inside a `<ratings>` block (Kodi v17+ structured form);
    // the default rating's value still arrives as `<value>` text — we take
    // the first `<value>` inside `<ratings>` as community_rating if a flat
    // `<rating>` wasn't already seen.
    let mut in_ratings = false;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.local_name();
                let tag = name.as_ref().to_ascii_lowercase();
                match tag.as_slice() {
                    b"actor" => actor = Some(ActorBuilder::default()),
                    b"ratings" => in_ratings = true,
                    b"uniqueid" | b"thumb" | b"fanart" => {
                        // Capture attributes for the upcoming text/child.
                        if tag.as_slice() == b"uniqueid" {
                            uniqueid_type = attr(&e, b"type");
                        }
                        if tag.as_slice() == b"thumb" {
                            // A <thumb> may carry the URL as text (handled in
                            // Text) or an `aspect` attr we ignore.
                        }
                    }
                    _ => {}
                }
                cur_tag = tag;
            }
            Ok(Event::Empty(e)) => {
                // Self-closing elements (e.g. `<thumb url="..."/>` is rare;
                // Kodi uses text, but tolerate an attr-only form).
                let name = e.local_name();
                let tag = name.as_ref().to_ascii_lowercase();
                match tag.as_slice() {
                    b"thumb" => {
                        if let Some(url) = attr(&e, b"url") {
                            push_artwork(&mut result, ArtworkRole::Primary, &url);
                        }
                    }
                    b"fanart" => {
                        if let Some(url) = attr(&e, b"url") {
                            push_artwork(&mut result, ArtworkRole::Backdrop, &url);
                        }
                    }
                    b"uniqueid" => {
                        // No text payload; nothing to record.
                        let _ = attr(&e, b"type");
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                let text = match t.unescape() {
                    Ok(c) => c.trim().to_string(),
                    Err(_) => continue, // tolerate a bad entity in one node
                };
                if text.is_empty() {
                    continue;
                }
                apply_text(
                    &cur_tag,
                    &text,
                    &mut result,
                    &mut original_title,
                    &mut actor,
                    &mut uniqueid_type,
                    in_ratings,
                );
            }
            Ok(Event::End(e)) => {
                let name = e.local_name();
                let tag = name.as_ref().to_ascii_lowercase();
                match tag.as_slice() {
                    b"actor" => {
                        if let Some(b) = actor.take() {
                            if let Some(p) = b.build() {
                                result.people.push(p);
                            }
                        }
                    }
                    b"ratings" => in_ratings = false,
                    b"uniqueid" => uniqueid_type = None,
                    _ => {}
                }
                cur_tag.clear();
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => {
                return Err(DomainError::Backend(format!(
                    "malformed NFO {}: {e}",
                    path.display()
                )));
            }
        }
        buf.clear();
    }

    // originaltitle fills title only if a real <title> was absent.
    if result.title.is_none() {
        result.title = original_title;
    }

    Ok(result)
}

/// Route a text run keyed by the element it sits inside.
#[allow(clippy::too_many_arguments)]
fn apply_text(
    tag: &[u8],
    text: &str,
    result: &mut MetadataResult,
    original_title: &mut Option<String>,
    actor: &mut Option<ActorBuilder>,
    uniqueid_type: &mut Option<String>,
    in_ratings: bool,
) {
    // If we're assembling an actor, its child elements (name/role/order)
    // take precedence over the top-level field names.
    if let Some(b) = actor.as_mut() {
        match tag {
            b"name" => {
                b.name = Some(text.to_string());
                return;
            }
            b"role" => {
                b.character = Some(text.to_string());
                return;
            }
            b"order" => {
                b.order = text.parse::<u32>().ok();
                return;
            }
            // Other actor children (thumb, etc.) are ignored.
            _ => return,
        }
    }

    match tag {
        b"title" => set_first(&mut result.title, text),
        b"originaltitle" => set_first(original_title, text),
        b"plot" => set_first(&mut result.overview, text),
        // Only use outline if no plot was seen.
        b"outline" => set_first(&mut result.overview, text),
        b"tagline" => set_first(&mut result.tagline, text),
        b"year" => set_first_with(&mut result.production_year, || parse_year(text)),
        b"premiered" | b"aired" | b"releasedate" => {
            set_first_with(&mut result.premiere_date, || parse_date_unix(text));
            // Backfill year from the date if <year> was absent.
            set_first_with(&mut result.production_year, || year_from_date(text));
        }
        b"rating" => set_first_with(&mut result.community_rating, || parse_rating(text)),
        // Structured <ratings><rating><value>..</value> — take the first
        // value as community rating if a flat <rating> didn't set one.
        b"value" if in_ratings => {
            set_first_with(&mut result.community_rating, || parse_rating(text))
        }
        b"criticrating" => set_first_with(&mut result.critic_rating, || parse_rating(text)),
        b"mpaa" | b"certification" => {
            set_first(&mut result.official_rating, &normalise_certification(text))
        }
        b"genre" => push_unique(&mut result.genres, text),
        b"studio" => push_unique(&mut result.studios, text),
        b"tag" => push_unique(&mut result.tags, text),
        b"set" | b"collection" => push_unique(&mut result.collections, text),
        b"director" => result.people.push(PersonRef {
            name: text.to_string(),
            role: None,
            kind: PersonKind::Director,
            character: None,
            sort_order: None,
        }),
        b"credits" => result.people.push(PersonRef {
            name: text.to_string(),
            role: None,
            kind: PersonKind::Writer,
            character: None,
            sort_order: None,
        }),
        b"uniqueid" => apply_uniqueid(&mut result.provider_ids, uniqueid_type.as_deref(), text),
        // Bare <id> — Kodi movies use TMDB by convention; only set tmdb if a
        // typed <uniqueid> hasn't already supplied one.
        b"id" => set_first(&mut result.provider_ids.tmdb, text),
        b"imdbid" | b"imdb_id" => set_first(&mut result.provider_ids.imdb, text),
        b"tmdbid" => set_first(&mut result.provider_ids.tmdb, text),
        b"tvdbid" => set_first(&mut result.provider_ids.tvdb, text),
        b"musicbrainztrackid" | b"musicbrainzalbumid" | b"musicbrainzartistid" => {
            set_first(&mut result.provider_ids.mbid, text)
        }
        b"thumb" => push_artwork(result, ArtworkRole::Primary, text),
        b"fanart" => push_artwork(result, ArtworkRole::Backdrop, text),
        _ => {}
    }
}

/// Set `slot` to `text` if it's still empty (first-wins within one NFO).
/// Blank text is ignored (the caller already trims, but a `<mpaa>` that
/// normalises to empty shouldn't clobber later sources).
fn set_first(slot: &mut Option<String>, text: &str) {
    if slot.is_none() && !text.is_empty() {
        *slot = Some(text.to_string());
    }
}

/// Set `slot` from a fallible parse, only when still empty and the parse
/// succeeds. Keeps the per-element arms free of nested `if let` blocks.
fn set_first_with<T>(slot: &mut Option<T>, parse: impl FnOnce() -> Option<T>) {
    if slot.is_none() {
        if let Some(v) = parse() {
            *slot = Some(v);
        }
    }
}

fn push_unique(acc: &mut Vec<String>, text: &str) {
    let v = text.trim();
    if v.is_empty() {
        return;
    }
    if !acc.iter().any(|e| e == v) {
        acc.push(v.to_string());
    }
}

/// Record a `<uniqueid type=...>` value into the matching provider slot.
/// Tolerant of casing and a missing/unknown `type` (an unknown type with a
/// `tt`-prefixed value is treated as IMDb; otherwise ignored).
fn apply_uniqueid(ids: &mut ProviderIds, ty: Option<&str>, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    match ty.map(str::to_ascii_lowercase).as_deref() {
        Some("tmdb") | Some("themoviedb") => {
            if ids.tmdb.is_none() {
                ids.tmdb = Some(value.to_string());
            }
        }
        Some("tvdb") | Some("thetvdb") => {
            if ids.tvdb.is_none() {
                ids.tvdb = Some(value.to_string());
            }
        }
        Some("imdb") => {
            if ids.imdb.is_none() {
                ids.imdb = Some(value.to_string());
            }
        }
        Some("musicbrainz") | Some("mbid") => {
            if ids.mbid.is_none() {
                ids.mbid = Some(value.to_string());
            }
        }
        _ => {
            // Unknown/absent type: an `tt…` id is unambiguously IMDb.
            if value.starts_with("tt") && ids.imdb.is_none() {
                ids.imdb = Some(value.to_string());
            }
        }
    }
}

/// Push one artwork ref. A `http(s)://` value is a [`ArtworkSource::Url`];
/// anything else is treated as a local sibling path
/// ([`ArtworkSource::LocalFile`]).
fn push_artwork(result: &mut MetadataResult, role: ArtworkRole, value: &str) {
    let v = value.trim();
    if v.is_empty() {
        return;
    }
    let source = if v.starts_with("http://") || v.starts_with("https://") {
        ArtworkSource::Url(v.to_string())
    } else {
        ArtworkSource::LocalFile(PathBuf::from(v))
    };
    let aref = ArtworkRef { role, source };
    if !result
        .artwork
        .iter()
        .any(|e| e.role == aref.role && e.source == aref.source)
    {
        result.artwork.push(aref);
    }
}

/// A `<year>` payload — first 4-digit run within the plausible window.
fn parse_year(text: &str) -> Option<u32> {
    let digits: String = text.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 4 {
        return None;
    }
    let y: u32 = digits.get(..4)?.parse().ok()?;
    (1800..3000).contains(&y).then_some(y)
}

/// `YYYY` from a `YYYY-MM-DD` (or `YYYY/MM/DD`) date string.
fn year_from_date(text: &str) -> Option<u32> {
    let y: u32 = text.get(..4)?.parse().ok()?;
    (1800..3000).contains(&y).then_some(y)
}

/// Parse a `<rating>` / `<criticrating>` float, tolerating a trailing
/// `/10`-style suffix and comma decimals. `None` on garbage.
fn parse_rating(text: &str) -> Option<f32> {
    let token = text.split_whitespace().next().unwrap_or(text);
    let token = token.split('/').next().unwrap_or(token);
    let token = token.replace(',', ".");
    let v: f32 = token.trim().parse().ok()?;
    if v.is_finite() {
        Some(v)
    } else {
        None
    }
}

/// Normalise a Kodi `<mpaa>` like `"Rated PG-13"` / `"US:PG-13"` to the
/// bare rating token Jellyfin's `OfficialRating` expects (`"PG-13"`).
/// Best-effort: an unrecognised shape is passed through trimmed.
fn normalise_certification(text: &str) -> String {
    let t = text.trim();
    // Strip a leading `Rated ` prefix (US mpaa convention).
    let t = t.strip_prefix("Rated ").unwrap_or(t);
    // `US:PG-13` / `gb:15` → take the part after the country `:`.
    let t = t.rsplit(':').next().unwrap_or(t);
    t.trim().to_string()
}

/// Parse a Kodi date (`YYYY-MM-DD`, optionally with a time) into
/// unix-seconds (UTC midnight of that date). Pure integer arithmetic — no
/// chrono dependency. Returns `None` on an unparseable / out-of-range date.
fn parse_date_unix(text: &str) -> Option<i64> {
    // Take the date portion before any whitespace/`T`.
    let date = text.split(|c: char| c == 'T' || c.is_whitespace()).next()?;
    let mut parts = date.split(['-', '/', '.']);
    let y: i64 = parts.next()?.trim().parse().ok()?;
    let m: i64 = parts.next()?.trim().parse().ok()?;
    let d: i64 = parts.next()?.trim().parse().ok()?;
    if !(1800..3000).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86_400)
}

/// Days since the unix epoch (1970-01-01) for a proleptic-Gregorian date,
/// via Howard Hinnant's `days_from_civil` algorithm. Valid for any date in
/// our 1800–2999 guard window.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Read a UTF-8 attribute value off a start/empty tag, lower-casing the
/// match on the attribute key. `None` if absent or non-UTF-8.
fn attr(e: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<String> {
    for a in e.attributes().flatten() {
        if a.key.local_name().as_ref().eq_ignore_ascii_case(key) {
            return a
                .unescape_value()
                .ok()
                .map(|c| c.trim().to_string())
                .filter(|s| !s.is_empty());
        }
    }
    None
}

/// Accumulates an `<actor>`'s child fields across events.
#[derive(Default)]
struct ActorBuilder {
    name: Option<String>,
    character: Option<String>,
    order: Option<u32>,
}

impl ActorBuilder {
    /// A built actor needs at least a name; otherwise it's dropped.
    fn build(self) -> Option<PersonRef> {
        let name = self.name?;
        if name.trim().is_empty() {
            return None;
        }
        Some(PersonRef {
            name,
            role: None,
            kind: PersonKind::Actor,
            character: self.character,
            sort_order: self.order,
        })
    }
}

#[cfg(test)]
mod tests;
