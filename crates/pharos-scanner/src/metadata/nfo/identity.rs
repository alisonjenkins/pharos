//! B558 — an episode's NFOs are trusted only while they name the show the
//! episode is filed under.
//!
//! A scraper that misidentifies a show folder writes NFOs, and stills beside
//! them, for a different show. Jellyfin did this to this library's
//! `TV/Dragon Ball`: its `tvshow.nfo` and every episode NFO describe Super
//! Dragon Ball Heroes, and each `<episode>-thumb.jpg` is that show's still.
//! Read as authoritative, the episodes took that show's titles, plots and
//! provider ids. The folder name is the user's own statement of what the show
//! is, so an NFO whose show name does not resemble it is set aside, along with
//! the images the same scrape saved beside it, and the episode falls back to
//! its filename and the online match (V468).

use std::path::Path;

use pharos_core::{title_similarity, MediaKind, MetadataRequest};
use quick_xml::events::Event;
use quick_xml::Reader;

use super::{predefined_entity, sibling_nfo};

/// Lowest [`title_similarity`] at which an NFO's show name is taken to name
/// the episode's show. Above the online matcher's 0.7 default because this
/// is a veto on local data that is otherwise authoritative; it still passes
/// every legitimate variant in the deployed library (punctuation, a `: Sub
/// title` the folder drops, `Mrs Browns Boys` for `Mrs Brown's Boys`).
const SHOW_NAME_MIN_SIMILARITY: f32 = 0.8;

/// Which of an episode's NFOs describe a different show.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ForeignNfos {
    /// The episode's own `<basename>.nfo`.
    pub episode: bool,
    /// The show folder's `tvshow.nfo`.
    pub show: bool,
}

/// Judge the NFOs of episode `req` against its series name. Anything but an
/// episode with a series name has nothing to judge against and is trusted.
pub(crate) fn foreign_nfos(req: &MetadataRequest<'_>) -> ForeignNfos {
    let mut out = ForeignNfos::default();
    let Some(series) = req.series.filter(|_| req.kind == MediaKind::Episode) else {
        return out;
    };
    let series_name = series.series_name.as_str();
    if series_name.trim().is_empty() {
        return out;
    }
    let show_path = series
        .series_folder
        .as_deref()
        .map(|f| Path::new(f).join("tvshow.nfo"));
    let show_nfo = show_path.as_deref().and_then(read_names);
    if let (Some(path), Some(names)) = (show_path.as_deref(), &show_nfo) {
        let titles = [&names.title, &names.original_title];
        if names_another_show(series_name, titles.into_iter().flatten()) {
            out.show = true;
            tracing::info!(
                series = series_name,
                nfo_title = names.title.as_deref().unwrap_or(""),
                nfo = %path.display(),
                "nfo: tvshow.nfo names another show; ignoring it and its art"
            );
        }
    }
    let episode_path = sibling_nfo(req.path);
    if let Some(path) = episode_path
        .as_deref()
        .filter(|p| Some(*p) != show_path.as_deref())
    {
        if let Some(names) = read_names(path) {
            out.episode = match &names.show_title {
                Some(show_title) => names_another_show(series_name, [show_title]),
                // No <showtitle>: the episode NFO came from the same scrape
                // as the show's, so it shares the show NFO's verdict.
                None => out.show,
            };
            if out.episode {
                tracing::info!(
                    series = series_name,
                    nfo_show_title = names.show_title.as_deref().unwrap_or(""),
                    nfo = %path.display(),
                    "nfo: episode NFO names another show; ignoring it and its art"
                );
            }
        }
    }
    out
}

/// True when the NFO offers at least one show name and none resembles
/// `series_name`. An NFO with no show name at all is not evidence either way.
pub(super) fn names_another_show<'a>(
    series_name: &str,
    names: impl IntoIterator<Item = &'a String>,
) -> bool {
    let mut offered = false;
    for name in names {
        offered = true;
        if title_similarity(series_name, name) >= SHOW_NAME_MIN_SIMILARITY {
            return false;
        }
    }
    offered
}

/// The show-identifying fields of one NFO.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct NfoNames {
    /// Root-level `<title>`: the show's name in a `tvshow.nfo`.
    pub title: Option<String>,
    pub original_title: Option<String>,
    /// `<showtitle>`: the show an episode NFO says it belongs to.
    pub show_title: Option<String>,
}

/// Read `path`'s show names. `None` when the file is absent, unreadable or
/// malformed: the NFO provider reports those itself, and this check only
/// ever sets an NFO aside, never fails a scan.
fn read_names(path: &Path) -> Option<NfoNames> {
    let bytes = std::fs::read(path).ok()?;
    parse_names(&bytes)
}

pub(super) fn parse_names(bytes: &[u8]) -> Option<NfoNames> {
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut names = NfoNames::default();
    // Open element names from the root down.
    let mut stack: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf).ok()? {
            Event::Start(e) => {
                let name = e.local_name();
                stack.push(name.as_ref().to_ascii_lowercase());
                text.clear();
            }
            Event::Text(t) => text.push_str(&t),
            Event::GeneralRef(r) => {
                if let Ok(Some(c)) = r.resolve_char_ref() {
                    text.push(c);
                } else if let Some(c) = predefined_entity(r.as_ref()) {
                    text.push(c);
                }
            }
            Event::End(_) => {
                let value = text.trim();
                if !value.is_empty() {
                    let slot = match stack.as_slice() {
                        [_, tag] if tag == "title" => Some(&mut names.title),
                        [_, tag] if tag == "originaltitle" => Some(&mut names.original_title),
                        [_, tag] if tag == "showtitle" => Some(&mut names.show_title),
                        _ => None,
                    };
                    if let Some(slot) = slot {
                        slot.get_or_insert_with(|| value.to_string());
                    }
                }
                stack.pop();
                text.clear();
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Some(names)
}
