//! Group tracks (014 US7): whether a member applies the group's audio and
//! subtitle picks, and where a menu pick goes. Pure.

pub use pharos_client::api::Track;
pub use pharos_client::group::TrackChoice;

use super::activity::TrackKind;

/// What the group is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GroupTracksView {
    pub audio: TrackChoice,
    pub subtitle: TrackChoice,
}

/// Picks to apply to the local player. `None` leaves that track as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackPicks {
    pub audio: Option<u32>,
    /// `Some(None)` turns subtitles off.
    pub subtitle: Option<Option<u32>>,
}

/// The picks a member applies for the group's tracks, or `None` when there is
/// nothing to apply: the member is on their own tracks, or every pick is unset
/// or names a stream this item does not have.
pub fn follows_group(
    own_tracks: bool,
    group: GroupTracksView,
    audio_indexes: &[u32],
    subtitle_indexes: &[u32],
) -> Option<TrackPicks> {
    if own_tracks {
        return None;
    }
    let audio = match group.audio {
        TrackChoice::Index(i) if audio_indexes.contains(&i) => Some(i),
        _ => None,
    };
    let subtitle = match group.subtitle {
        TrackChoice::Off => Some(None),
        TrackChoice::Index(i) if subtitle_indexes.contains(&i) => Some(Some(i)),
        _ => None,
    };
    (audio.is_some() || subtitle.is_some()).then_some(TrackPicks { audio, subtitle })
}

/// Where a track menu pick goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickRoute {
    /// Tell the group; every following member, this one included, applies it.
    SendToGroup,
    /// Change only this player ("Just me", or no group at all).
    ApplyLocally,
}

pub fn route_pick(in_group: bool, own_tracks: bool) -> PickRoute {
    if in_group && !own_tracks {
        PickRoute::SendToGroup
    } else {
        PickRoute::ApplyLocally
    }
}

/// The audio stream this player switches to for a menu pick of `index`, or
/// `None` when nothing is switched here: it is already playing, or the group
/// took the pick (`to_group` returns true) and will apply it to every
/// following player, this one included.
pub fn audio_pick_here(
    playing: Option<u32>,
    index: u32,
    to_group: impl FnOnce(u32) -> bool,
) -> Option<u32> {
    if playing == Some(index) || to_group(index) {
        return None;
    }
    Some(index)
}

/// The group's audio pick `pick` when this playback must be negotiated again
/// to carry it: not what the server chose to play (`playing`), and not what
/// this playback was already asked for (`asked`). Without the second test a
/// server that answers another stream, or none, is asked again by every
/// remount, forever.
pub fn audio_to_renegotiate(
    pick: Option<u32>,
    playing: Option<u32>,
    asked: Option<u32>,
) -> Option<u32> {
    pick.filter(|a| playing != Some(*a) && asked != Some(*a))
}

/// Whether this player switches itself to the subtitle `to` (`None` is off) for
/// a menu pick, `shown` being what it shows now. False when the group took the
/// pick, as for [`audio_pick_here`].
pub fn subtitle_pick_here(
    shown: Option<u32>,
    to: Option<u32>,
    to_group: impl FnOnce(Option<u32>) -> bool,
) -> bool {
    !(shown != to && to_group(to))
}

/// How the streams of the version playing line up with the item's primary
/// file, whose indexes the group's picks name (ADR-0089, V902). Two streams
/// match when they share language, forced and hearing-impaired flags and
/// stand at the same place among the streams of their file that share them.
/// On the primary every stream matches itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackMap {
    /// (this version's index, the primary's index) for each matched stream.
    audio: Vec<(u32, u32)>,
    subtitles: Vec<(u32, u32)>,
}

impl TrackMap {
    pub fn new(
        (audio, subtitles): (&[Track], &[Track]),
        (primary_audio, primary_subtitles): (&[Track], &[Track]),
    ) -> Self {
        Self {
            audio: pair(audio, primary_audio),
            subtitles: pair(subtitles, primary_subtitles),
        }
    }

    fn pairs(&self, kind: TrackKind) -> &[(u32, u32)] {
        match kind {
            TrackKind::Audio => &self.audio,
            TrackKind::Subtitles => &self.subtitles,
        }
    }

    /// The primary's index for this version's stream `own`.
    pub fn to_primary(&self, kind: TrackKind, own: u32) -> Option<u32> {
        self.pairs(kind)
            .iter()
            .find(|(o, _)| *o == own)
            .map(|(_, p)| *p)
    }

    /// This version's index for the primary's stream `primary`.
    pub fn to_own(&self, kind: TrackKind, primary: u32) -> Option<u32> {
        self.pairs(kind)
            .iter()
            .find(|(_, p)| *p == primary)
            .map(|(o, _)| *o)
    }

    /// The primary's indexes of the streams this version has.
    pub fn primary_indexes(&self, kind: TrackKind) -> Vec<u32> {
        self.pairs(kind).iter().map(|(_, p)| *p).collect()
    }

    /// Picks naming the primary's streams, as this version's. A stream this
    /// version lacks becomes `None` (left as it is); turning subtitles off
    /// always carries.
    pub fn picks_to_own(&self, picks: TrackPicks) -> TrackPicks {
        TrackPicks {
            audio: picks.audio.and_then(|i| self.to_own(TrackKind::Audio, i)),
            subtitle: picks.subtitle.and_then(|to| match to {
                None => Some(None),
                Some(i) => self.to_own(TrackKind::Subtitles, i).map(Some),
            }),
        }
    }
}

/// The streams' matching key: language (case-folded) and the two flags.
fn track_key(t: &Track) -> (String, bool, bool) {
    (
        t.language.to_ascii_lowercase(),
        t.is_forced,
        t.is_hearing_impaired,
    )
}

/// Each stream of `own` with the stream of `primary` sharing its key and its
/// place among the streams with that key.
fn pair(own: &[Track], primary: &[Track]) -> Vec<(u32, u32)> {
    let ranked = |tracks: &[Track]| -> Vec<((String, bool, bool), usize, u32)> {
        let mut seen: Vec<(String, bool, bool)> = Vec::new();
        tracks
            .iter()
            .map(|t| {
                let key = track_key(t);
                let rank = seen.iter().filter(|k| **k == key).count();
                seen.push(key.clone());
                (key, rank, t.index)
            })
            .collect()
    };
    let primary = ranked(primary);
    ranked(own)
        .into_iter()
        .filter_map(|(key, rank, index)| {
            primary
                .iter()
                .find(|(k, r, _)| *k == key && *r == rank)
                .map(|(_, _, p)| (index, *p))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use TrackChoice::{Index, Off, Unset};

    #[test]
    fn a_following_member_sends_the_pick_to_the_group_and_changes_nothing_here() {
        let sent = std::cell::RefCell::new(Vec::new());
        let take = |i| {
            sent.borrow_mut().push(Some(i));
            true
        };
        assert_eq!(audio_pick_here(Some(1), 2, take), None);
        let take = |to| {
            sent.borrow_mut().push(to);
            true
        };
        assert!(!subtitle_pick_here(None, Some(3), take));
        assert_eq!(*sent.borrow(), [Some(2), Some(3)]);
    }

    #[test]
    fn an_audio_pick_the_server_did_not_play_is_asked_for_once() {
        assert_eq!(audio_to_renegotiate(Some(2), Some(1), None), Some(2));
        // Asked for 2 and the server played 1, or nothing: the remount that
        // follows must not ask again.
        assert_eq!(audio_to_renegotiate(Some(2), Some(1), Some(2)), None);
        assert_eq!(audio_to_renegotiate(Some(2), None, Some(2)), None);
        assert_eq!(audio_to_renegotiate(Some(2), Some(2), None), None);
        assert_eq!(audio_to_renegotiate(None, Some(1), None), None);
    }

    #[test]
    fn a_just_me_member_switches_only_its_own_player() {
        assert_eq!(audio_pick_here(Some(1), 2, |_| false), Some(2));
        assert!(subtitle_pick_here(None, Some(3), |_| false));
        assert!(subtitle_pick_here(Some(3), None, |_| false));
    }

    #[test]
    fn the_stream_already_playing_is_not_picked_again() {
        let asked = std::cell::Cell::new(false);
        let ask = |_| {
            asked.set(true);
            true
        };
        assert_eq!(audio_pick_here(Some(2), 2, ask), None);
        assert!(!asked.get(), "the group is not asked");
    }

    #[test]
    fn a_subtitle_already_shown_is_not_sent_to_the_group() {
        let asked = std::cell::Cell::new(false);
        let ask = |_| {
            asked.set(true);
            true
        };
        assert!(subtitle_pick_here(Some(3), Some(3), ask));
        assert!(!asked.get(), "the group is not asked");
    }

    const AUDIO: &[u32] = &[1, 2];
    const SUBS: &[u32] = &[3, 4];

    fn tracks(audio: TrackChoice, subtitle: TrackChoice) -> GroupTracksView {
        GroupTracksView { audio, subtitle }
    }

    #[test]
    fn a_follower_applies_both_picks() {
        assert_eq!(
            follows_group(false, tracks(Index(2), Index(4)), AUDIO, SUBS),
            Some(TrackPicks {
                audio: Some(2),
                subtitle: Some(Some(4)),
            })
        );
    }

    #[test]
    fn own_tracks_ignore_the_group() {
        assert_eq!(
            follows_group(true, tracks(Index(2), Index(4)), AUDIO, SUBS),
            None
        );
    }

    #[test]
    fn a_pick_the_item_lacks_is_dropped_alone() {
        assert_eq!(
            follows_group(false, tracks(Index(9), Index(4)), AUDIO, SUBS),
            Some(TrackPicks {
                audio: None,
                subtitle: Some(Some(4)),
            })
        );
        assert_eq!(
            follows_group(false, tracks(Index(9), Index(8)), AUDIO, SUBS),
            None
        );
    }

    #[test]
    fn subtitles_off_applies_without_any_subtitle_stream() {
        assert_eq!(
            follows_group(false, tracks(Unset, Off), AUDIO, &[]),
            Some(TrackPicks {
                audio: None,
                subtitle: Some(None),
            })
        );
    }

    #[test]
    fn an_unset_group_changes_nothing() {
        assert_eq!(
            follows_group(false, GroupTracksView::default(), AUDIO, SUBS),
            None
        );
    }

    #[test]
    fn a_subtitle_index_is_not_taken_from_the_audio_list() {
        assert_eq!(
            follows_group(false, tracks(Unset, Index(1)), AUDIO, SUBS),
            None
        );
    }

    #[test]
    fn menu_picks_go_to_the_group_only_while_following_in_one() {
        assert_eq!(route_pick(true, false), PickRoute::SendToGroup);
        assert_eq!(route_pick(true, true), PickRoute::ApplyLocally);
        assert_eq!(route_pick(false, false), PickRoute::ApplyLocally);
        assert_eq!(route_pick(false, true), PickRoute::ApplyLocally);
    }

    fn track(index: u32, language: &str) -> Track {
        Track {
            index,
            language: language.to_owned(),
            ..Track::default()
        }
    }

    /// B941: the 2D file has English at 1 and Japanese at 2; the 3D rip has
    /// Japanese at 1, English at 2 and a commentary in English at 3.
    fn two_versions() -> TrackMap {
        let primary_audio = [track(1, "eng"), track(2, "jpn")];
        let primary_subs = [track(3, "eng"), track(4, "eng")];
        let audio = [track(1, "jpn"), track(2, "ENG"), track(3, "eng")];
        let subs = [
            Track {
                is_forced: true,
                ..track(5, "eng")
            },
            track(6, "eng"),
        ];
        TrackMap::new((&audio, &subs), (&primary_audio, &primary_subs))
    }

    #[test]
    fn a_group_pick_lands_on_the_same_language_in_another_version() {
        let map = two_versions();
        assert_eq!(map.to_own(TrackKind::Audio, 2), Some(1), "Japanese");
        assert_eq!(map.to_own(TrackKind::Audio, 1), Some(2), "English");
        assert_eq!(map.to_primary(TrackKind::Audio, 1), Some(2));
        assert_eq!(map.to_primary(TrackKind::Audio, 3), None, "second English");
    }

    #[test]
    fn streams_match_by_flags_and_order_too() {
        let map = two_versions();
        assert_eq!(
            map.to_own(TrackKind::Subtitles, 3),
            Some(6),
            "first plain English"
        );
        assert_eq!(
            map.to_own(TrackKind::Subtitles, 4),
            None,
            "no second plain English"
        );
        assert_eq!(map.to_primary(TrackKind::Subtitles, 5), None, "forced");
        assert_eq!(map.primary_indexes(TrackKind::Audio), vec![2, 1]);
    }

    #[test]
    fn picks_carry_where_they_match_and_off_always_carries() {
        let map = two_versions();
        let to_own = |audio, subtitle| map.picks_to_own(TrackPicks { audio, subtitle });
        assert_eq!(
            to_own(Some(2), Some(None)),
            TrackPicks {
                audio: Some(1),
                subtitle: Some(None)
            }
        );
        assert_eq!(
            to_own(Some(9), Some(Some(4))),
            TrackPicks {
                audio: None,
                subtitle: None
            }
        );
    }

    #[test]
    fn on_the_primary_every_stream_is_itself() {
        let audio = [track(1, "eng"), track(2, "eng"), track(4, "")];
        let subs = [track(7, "fre")];
        let map = TrackMap::new((&audio, &subs), (&audio, &subs));
        for i in [1, 2, 4] {
            assert_eq!(map.to_own(TrackKind::Audio, i), Some(i));
            assert_eq!(map.to_primary(TrackKind::Audio, i), Some(i));
        }
        assert_eq!(map.to_own(TrackKind::Subtitles, 7), Some(7));
    }
}
