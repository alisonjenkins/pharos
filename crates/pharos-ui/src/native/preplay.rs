//! The audio and subtitle pickers a title page shows before playback. They
//! start on the tracks the server would play (the user's last choice for the
//! item, else their language preferences) and hand only a changed pick to
//! the player, so an untouched picker leaves the server's rules in charge.

use dioxus::prelude::*;
use pharos_client::api::{ItemRef, StartTracks, Track, TrackChoice, TrackPicks};

use super::watch_view::TrackPicker;

/// The picks the title page last made for an item, read once by the watch
/// screen that plays it.
#[derive(Clone, Copy)]
pub struct PlayPicks(pub Signal<Option<(ItemRef, TrackPicks)>>);

/// What to ask the server for: `Default` for a track left on the one it
/// would start on, else the track picked (`None` subtitle is off).
pub fn picks(start: StartTracks, audio: Option<u32>, subtitle: Option<u32>) -> TrackPicks {
    TrackPicks {
        audio: match audio {
            Some(i) if audio != start.audio => TrackChoice::Index(i),
            _ => TrackChoice::Default,
        },
        subtitle: match subtitle {
            _ if subtitle == start.subtitle => TrackChoice::Default,
            Some(i) => TrackChoice::Index(i),
            None => TrackChoice::Off,
        },
    }
}

/// The picks the watch screen for `id` starts with: the title page's, when
/// they were made for this item.
pub fn picks_for(id: ItemRef, pending: Option<(ItemRef, TrackPicks)>) -> TrackPicks {
    pending
        .filter(|(item, _)| *item == id)
        .map(|(_, picks)| picks)
        .unwrap_or_default()
}

/// Audio and subtitle pickers for `id`, starting on `start`. Nothing for an
/// item with one audio track and no subtitles.
#[component]
pub fn TrackChoices(
    id: ItemRef,
    audio: Vec<Track>,
    subtitles: Vec<Track>,
    start: StartTracks,
) -> Element {
    let mut pending = use_context::<PlayPicks>().0;
    let mut audio_pick = use_signal(|| start.audio);
    let mut subtitle_pick = use_signal(|| start.subtitle);
    if audio.len() < 2 && subtitles.is_empty() {
        return rsx! {};
    }
    rsx! {
        div { class: "pharos-title-tracks",
            TrackPicker {
                label: "Audio",
                tracks: audio,
                pick: audio_pick(),
                offer_off: false,
                on_pick: move |p: Option<u32>| {
                    if p.is_some() {
                        audio_pick.set(p);
                        pending.set(Some((id, picks(start, p, *subtitle_pick.peek()))));
                    }
                },
            }
            TrackPicker {
                label: "Subtitles",
                tracks: subtitles,
                pick: subtitle_pick(),
                offer_off: true,
                on_pick: move |p: Option<u32>| {
                    subtitle_pick.set(p);
                    pending.set(Some((id, picks(start, *audio_pick.peek(), p))));
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: StartTracks = StartTracks {
        audio: Some(1),
        subtitle: None,
    };

    #[test]
    fn an_untouched_pick_leaves_the_server_to_choose() {
        assert_eq!(picks(START, Some(1), None), TrackPicks::default());
    }

    #[test]
    fn a_changed_pick_names_its_track_or_turns_subtitles_off() {
        assert_eq!(
            picks(START, Some(2), Some(4)),
            TrackPicks {
                audio: TrackChoice::Index(2),
                subtitle: TrackChoice::Index(4),
            }
        );
        let on = StartTracks {
            audio: Some(1),
            subtitle: Some(4),
        };
        assert_eq!(
            picks(on, Some(1), None),
            TrackPicks {
                audio: TrackChoice::Default,
                subtitle: TrackChoice::Off,
            }
        );
    }

    #[test]
    fn only_the_item_picked_for_plays_on_its_picks() {
        let chosen = TrackPicks {
            audio: TrackChoice::Index(2),
            subtitle: TrackChoice::Off,
        };
        let pending = Some((ItemRef::Real(7), chosen));
        assert_eq!(picks_for(ItemRef::Real(7), pending), chosen);
        assert_eq!(picks_for(ItemRef::Real(8), pending), TrackPicks::default());
        assert_eq!(picks_for(ItemRef::Real(7), None), TrackPicks::default());
    }

    #[test]
    fn pickers_show_only_for_a_choice_to_make() {
        let one = vec![Track {
            index: 1,
            language: "eng".into(),
            ..Track::default()
        }];
        let with = |audio: Vec<Track>, subtitles: Vec<Track>| {
            let mut dom = VirtualDom::new_with_props(
                |(audio, subtitles): (Vec<Track>, Vec<Track>)| {
                    use_context_provider(|| PlayPicks(Signal::new(None)));
                    rsx! { TrackChoices { id: ItemRef::Real(1), audio, subtitles, start: START } }
                },
                (audio, subtitles),
            );
            dom.rebuild_in_place();
            dioxus_ssr::render(&dom)
        };
        assert!(!with(one.clone(), Vec::new()).contains("<select"));
        let html = with(one.clone(), one);
        assert!(html.contains(r#"aria-label="Subtitles""#), "{html}");
        assert!(!html.contains(r#"aria-label="Audio""#), "{html}");
    }
}
