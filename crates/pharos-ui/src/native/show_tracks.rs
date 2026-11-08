//! The audio and subtitle a viewer chose for a show, carried to its other
//! episodes. Stream indices belong to one file, so a choice is kept as what
//! the track is (its language, title and flags) and matched against each
//! episode's own tracks.

use pharos_client::api::{Detail, Track, TrackChoice, TrackPicks};

use super::platform;
use super::playback::language_name;
use super::track_follow;
use serde::{Deserialize, Serialize};

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

/// The stored choice read back; nothing chosen when the value is absent
/// or does not parse.
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

/// `show` after the viewer picks `index` among `tracks`.
pub fn chose_audio(mut show: ShowTracks, tracks: &[Track], index: u32) -> ShowTracks {
    if let Some(t) = tracks.iter().find(|t| t.index == index) {
        show.audio = Some(Kept::of(t));
    }
    show
}

/// `show` after the viewer picks `index` among `tracks` (`None`: off).
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

/// A show's kept choice, read from this device's storage; nothing chosen
/// when storage cannot be read.
pub fn load(series: &str) -> ShowTracks {
    match platform::stored_theme(&key(series)) {
        Ok(stored) => parse(stored.as_deref()),
        Err(e) => {
            platform::warn(&format!(
                "pharos: show tracks storage read failed error={e}"
            ));
            ShowTracks::default()
        }
    }
}

/// Keep what `change` makes of a show's choice on this device.
pub fn keep(series: &str, change: impl FnOnce(ShowTracks) -> ShowTracks) {
    let next = change(load(series));
    if let Err(e) = platform::store_theme(&key(series), &encode(&next)) {
        platform::warn(&format!(
            "pharos: show tracks storage write failed error={e}"
        ));
    }
}

/// The tracks a playback of an episode asks for, highest precedence first:
///
/// 1. the group's pick, for each track `group` says this member follows. It
///    is applied to the player once it loads, so the show's choice is left
///    out of that track rather than negotiated and then undone;
/// 2. a pick made for this episode (`asked`: handed from the title page, or
///    by a renegotiation);
/// 3. the show's choice, for a track still `Default` and only when this
///    episode has a track like it;
/// 4. `Default`, which leaves the server to decide.
pub fn start_picks(
    group: Option<&track_follow::TrackPicks>,
    show: &ShowTracks,
    detail: &Detail,
    asked: TrackPicks,
) -> TrackPicks {
    let carried = apply(show, detail, asked);
    TrackPicks {
        audio: if group.is_some_and(|g| g.audio.is_some()) {
            asked.audio
        } else {
            carried.audio
        },
        subtitle: if group.is_some_and(|g| g.subtitle.is_some()) {
            asked.subtitle
        } else {
            carried.subtitle
        },
    }
}

/// The `(audio, subtitle)` a title page's pickers start on: the show's
/// choice where it applies, else the server's `start`.
pub fn shown(
    show: &ShowTracks,
    detail: &Detail,
    start: (Option<u32>, Option<u32>),
) -> (Option<u32>, Option<u32>) {
    let picks = apply(show, detail, TrackPicks::default());
    let audio = match picks.audio {
        TrackChoice::Index(i) => Some(i),
        _ => start.0,
    };
    let subtitle = match picks.subtitle {
        TrackChoice::Index(i) => Some(i),
        TrackChoice::Off => None,
        TrackChoice::Default => start.1,
    };
    (audio, subtitle)
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
                artist: None,
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
                stereo: None,
            },
            overview: String::new(),
            genres: Vec::new(),
            chapters: Vec::new(),
            audio_tracks: audio,
            subtitle_tracks: subtitles,
            trickplay: Vec::new(),
            segments: Vec::new(),
            fonts: Vec::new(),
            artists: Vec::new(),
            album: None,
            cast: Vec::new(),
            versions: Vec::new(),
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
        assert_eq!(shown(&show, &next, (None, None)).1, Some(3));
    }

    fn jpn_and_signs() -> (ShowTracks, Detail) {
        let first = [track(1, "eng", "", false), track(2, "jpn", "", false)];
        let show = chose_audio(ShowTracks::default(), &first, 2);
        let show = chose_subtitle(show, &[track(3, "eng", "", false)], Some(3));
        let next = detail(
            vec![track(1, "jpn", "", false), track(2, "eng", "", false)],
            vec![track(5, "eng", "", false), track(6, "fre", "", false)],
        );
        (show, next)
    }

    #[test]
    fn the_shows_choice_starts_an_episode_left_to_the_server() {
        let (show, next) = jpn_and_signs();
        let picks = start_picks(None, &show, &next, TrackPicks::default());
        assert_eq!(picks.audio, TrackChoice::Index(1));
        assert_eq!(picks.subtitle, TrackChoice::Index(5));
    }

    #[test]
    fn a_pick_for_this_episode_beats_the_shows_in_a_start() {
        let (show, next) = jpn_and_signs();
        let asked = TrackPicks {
            audio: TrackChoice::Index(2),
            subtitle: TrackChoice::Off,
        };
        assert_eq!(start_picks(None, &show, &next, asked), asked);
    }

    #[test]
    fn the_groups_pick_is_never_overridden_by_the_shows() {
        let (show, next) = jpn_and_signs();
        let group = track_follow::TrackPicks {
            audio: Some(2),
            subtitle: None,
        };
        let picks = start_picks(Some(&group), &show, &next, TrackPicks::default());
        assert_eq!(picks.audio, TrackChoice::Default, "the group takes audio");
        assert_eq!(
            picks.subtitle,
            TrackChoice::Index(5),
            "the show still carries what the group leaves unset"
        );
        let group = track_follow::TrackPicks {
            audio: None,
            subtitle: Some(None),
        };
        let picks = start_picks(Some(&group), &show, &next, TrackPicks::default());
        assert_eq!(picks.audio, TrackChoice::Index(1));
        assert_eq!(
            picks.subtitle,
            TrackChoice::Default,
            "the group takes subtitles"
        );
    }

    #[test]
    fn an_episode_without_a_track_like_the_shows_is_left_to_the_server() {
        let (show, _) = jpn_and_signs();
        let bare = detail(vec![track(1, "ger", "", false)], Vec::new());
        assert_eq!(
            start_picks(None, &show, &bare, TrackPicks::default()),
            TrackPicks::default()
        );
    }

    #[test]
    fn a_choice_survives_storage_and_junk_reads_as_none() {
        let show = chose_subtitle(ShowTracks::default(), &[], None);
        assert_eq!(parse(Some(&encode(&show))), show);
        assert_eq!(parse(Some("not json")), ShowTracks::default());
        assert_eq!(parse(None), ShowTracks::default());
    }

    #[test]
    fn a_show_is_kept_under_its_own_key() {
        assert_eq!(key("Show"), "pharos.tracks.show.Show");
    }
}
