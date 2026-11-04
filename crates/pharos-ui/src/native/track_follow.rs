//! Group tracks (014 US7): whether a member applies the group's audio and
//! subtitle picks, and where a menu pick goes. Pure.

pub use pharos_client::group::TrackChoice;

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
}
