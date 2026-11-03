//! The audio and subtitle a viewer chose for a show, carried to its other
//! episodes. Stream indices belong to one file, so a choice is kept as what
//! the track is (its language, title and flags) and matched against each
//! episode's own tracks.

use pharos_client::api::{Detail, StartTracks, Track, TrackChoice, TrackPicks};
use serde::{Deserialize, Serialize};

use super::platform;
use super::playback::language_name;

/// What a chosen track is, independent of its position in a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kept {
    pub language: String,
    pub title: String,
    pub forced: bool,
    pub hearing_impaired: bool,
}

impl Kept {
    pub fn of(track: &Track) -> Self {
        Self {
            language: track.language.clone(),
            title: track.title.clone(),
            forced: track.is_forced,
            hearing_impaired: track.is_hearing_impaired,
        }
    }
}

/// A show's chosen subtitle: none, or one like `Kept`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeptSubtitle {
    Off,
    On(Kept),
}

/// A show's chosen tracks; `None` for either says the viewer has not
/// chosen it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShowTracks {
    pub audio: Option<Kept>,
    pub subtitle: Option<KeptSubtitle>,
}

/// Where a show's choice is kept in `localStorage`.
pub fn key(series: &str) -> String {
    format!("pharos.tracks.show.{series}")
}

pub fn parse(stored: Option<&str>) -> ShowTracks {
    stored
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default()
}

pub fn encode(show: &ShowTracks) -> String {
    serde_json::to_string(show).unwrap_or_default()
}

/// The show an item's choice is kept for: an episode's series.
pub fn show_of(detail: &Detail) -> Option<&str> {
    match detail.item.kind {
        pharos_client::api::ItemKind::Episode => detail.item.series_name.as_deref(),
        _ => None,
    }
}

/// The choice kept for `series`; nothing chosen when storage has none.
pub fn load(series: &str) -> ShowTracks {
    parse(
        platform::stored_theme(&key(series))
            .ok()
            .flatten()
            .as_deref(),
    )
}

/// Keeps `change` of `series`'s choice for its next episodes.
pub fn keep(series: &str, change: impl FnOnce(ShowTracks) -> ShowTracks) {
    let show = change(load(series));
    if let Err(e) = platform::store_theme(&key(series), &encode(&show)) {
        platform::warn(&format!("pharos: show tracks not kept: {e}"));
    }
}

/// `show` after the viewer picks `index` among `tracks` (`None`: off).
pub fn chose_audio(mut show: ShowTracks, tracks: &[Track], index: u32) -> ShowTracks {
    if let Some(t) = tracks.iter().find(|t| t.index == index) {
        show.audio = Some(Kept::of(t));
    }
    show
}

pub fn chose_subtitle(mut show: ShowTracks, tracks: &[Track], index: Option<u32>) -> ShowTracks {
    match index {
        None => show.subtitle = Some(KeptSubtitle::Off),
        Some(i) => {
            if let Some(t) = tracks.iter().find(|t| t.index == i) {
                show.subtitle = Some(KeptSubtitle::On(Kept::of(t)));
            }
        }
    }
    show
}

/// The track in `tracks` most like `kept`: the same language (by name, so
/// `fre` finds `fra`), then the same forced and SDH flags, then the same
/// title. Nothing when no track is in that language.
pub fn matching(tracks: &[Track], kept: &Kept) -> Option<u32> {
    let language = language_name(&kept.language);
    tracks
        .iter()
        .filter(|t| !t.language.is_empty() && language_name(&t.language) == language)
        .max_by_key(|t| {
            (
                t.is_forced == kept.forced,
                t.is_hearing_impaired == kept.hearing_impaired,
                t.title.eq_ignore_ascii_case(&kept.title),
                // The first of equals: max_by_key keeps the last.
                std::cmp::Reverse(t.index),
            )
        })
        .map(|t| t.index)
}

/// `asked` with each track the viewer left to the server replaced by the
/// show's choice, when this episode has a track like it.
pub fn apply(show: &ShowTracks, detail: &Detail, asked: TrackPicks) -> TrackPicks {
    let audio = match (asked.audio, &show.audio) {
        (TrackChoice::Default, Some(kept)) => {
            matching(&detail.audio_tracks, kept).map_or(TrackChoice::Default, TrackChoice::Index)
        }
        (asked, _) => asked,
    };
    let subtitle = match (asked.subtitle, &show.subtitle) {
        (TrackChoice::Default, Some(KeptSubtitle::Off)) => TrackChoice::Off,
        (TrackChoice::Default, Some(KeptSubtitle::On(kept))) => {
            matching(&detail.subtitle_tracks, kept).map_or(TrackChoice::Default, TrackChoice::Index)
        }
        (asked, _) => asked,
    };
    TrackPicks { audio, subtitle }
}

/// The tracks a title page's pickers start on: the show's choice where it
/// applies, else the server's.
pub fn shown(show: &ShowTracks, detail: &Detail, start: StartTracks) -> StartTracks {
    let picks = apply(show, detail, TrackPicks::default());
    StartTracks {
        audio: match picks.audio {
            TrackChoice::Index(i) => Some(i),
            _ => start.audio,
        },
        subtitle: match picks.subtitle {
            TrackChoice::Index(i) => Some(i),
            TrackChoice::Off => None,
            TrackChoice::Default => start.subtitle,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(index: u32, language: &str, title: &str, forced: bool) -> Track {
        Track {
            index,
            language: language.into(),
            title: title.into(),
            is_forced: forced,
            ..Track::default()
        }
    }

    fn detail(audio: Vec<Track>, subtitles: Vec<Track>) -> Detail {
        Detail {
            item: pharos_client::api::Item {
                id: pharos_client::api::ItemRef::Real(1),
                kind: pharos_client::api::ItemKind::Episode,
                name: String::new(),
                year: None,
                runtime_ms: 0,
                image: None,
                played: false,
                favorite: false,
                position_ms: 0,
                series_name: Some("Show".into()),
                season: None,
                episode: None,
                parent: None,
                library_kind: None,
                imdb_rating: None,
                critic_rating: None,
            },
            overview: String::new(),
            genres: Vec::new(),
            chapters: Vec::new(),
            audio_tracks: audio,
            subtitle_tracks: subtitles,
            trickplay: Vec::new(),
            segments: Vec::new(),
        }
    }

    #[test]
    fn a_choice_follows_the_language_not_the_index() {
        let first = [track(1, "eng", "", false), track(2, "jpn", "", false)];
        let show = chose_audio(ShowTracks::default(), &first, 2);
        let next = detail(
            vec![track(1, "jpn", "", false), track(2, "eng", "", false)],
            Vec::new(),
        );
        assert_eq!(
            apply(&show, &next, TrackPicks::default()).audio,
            TrackChoice::Index(1)
        );
    }

    #[test]
    fn a_subtitle_match_prefers_the_same_flags_and_title() {
        let kept = Kept {
            language: "eng".into(),
            title: "Full".into(),
            forced: false,
            hearing_impaired: false,
        };
        let subs = [
            track(3, "eng", "Forced", true),
            track(4, "en", "Signs", false),
            track(5, "eng", "Full", false),
            track(6, "fre", "", false),
        ];
        assert_eq!(matching(&subs, &kept), Some(5));
        let forced = Kept {
            forced: true,
            ..kept.clone()
        };
        assert_eq!(matching(&subs, &forced), Some(3));
        let french = Kept {
            language: "fra".into(),
            ..kept
        };
        assert_eq!(matching(&subs, &french), Some(6));
    }

    #[test]
    fn an_off_choice_or_a_missing_language_is_kept_as_such() {
        let subs = [track(3, "eng", "", false)];
        let off = chose_subtitle(ShowTracks::default(), &subs, None);
        let next = detail(Vec::new(), subs.to_vec());
        assert_eq!(
            apply(&off, &next, TrackPicks::default()).subtitle,
            TrackChoice::Off
        );
        let german = ShowTracks {
            subtitle: Some(KeptSubtitle::On(Kept::of(&track(9, "ger", "", false)))),
            ..ShowTracks::default()
        };
        assert_eq!(
            apply(&german, &next, TrackPicks::default()).subtitle,
            TrackChoice::Default
        );
    }

    #[test]
    fn a_pick_made_for_this_episode_beats_the_shows() {
        let subs = [track(3, "eng", "", false), track(4, "jpn", "", false)];
        let show = chose_subtitle(ShowTracks::default(), &subs, Some(3));
        let asked = TrackPicks {
            audio: TrackChoice::Default,
            subtitle: TrackChoice::Index(4),
        };
        let next = detail(Vec::new(), subs.to_vec());
        assert_eq!(apply(&show, &next, asked).subtitle, TrackChoice::Index(4));
        assert_eq!(
            shown(&show, &next, StartTracks::default()).subtitle,
            Some(3)
        );
    }

    #[test]
    fn a_choice_survives_storage_and_junk_reads_as_none() {
        let show = chose_subtitle(ShowTracks::default(), &[], None);
        assert_eq!(parse(Some(&encode(&show))), show);
        assert_eq!(parse(Some("not json")), ShowTracks::default());
        assert_eq!(parse(None), ShowTracks::default());
    }
}
