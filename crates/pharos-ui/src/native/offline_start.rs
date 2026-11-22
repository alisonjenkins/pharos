//! The phone's start with no server (spec 044): when the app opens cut off
//! and music or books are downloaded on the phone, it lists them and plays
//! them. With nothing downloaded the usual waiting page stays.
//!
//! The list and the decision are plain functions over the
//! [`OfflineStore`](super::ports::OfflineStore) port's data, so they run
//! without a phone. Everything shown comes from the store and the names
//! noted when an item was kept (`offline_names`); nothing asks the server.

use std::sync::{Arc, Mutex, PoisonError};

use dioxus::prelude::*;
use tokio::sync::watch;

use super::offline_names::Names;
use super::offline_ui::{container_title, format_bytes, track_title};
use super::platform;
use super::ports::{
    native_audio, offline_store, ContainerKind, ContainerRef, Download, NativeAudio, OfflineStore,
    PlayState, PlayerSnapshot, TrackState,
};
use super::views::Brand;

/// What the start screen shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Connected, or still connecting: the usual screens.
    Normal,
    /// Cut off with downloads to play: the offline home.
    OfflineHome,
    /// Cut off with nothing to play: the waiting page.
    Waiting,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::OfflineHome => "offline_home",
            Self::Waiting => "waiting_page",
        }
    }
}

/// The verdict, why, and how many downloaded items it counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub verdict: Verdict,
    pub reason: &'static str,
    pub items: usize,
}

impl Decision {
    /// The line the phone logs for the decision.
    pub fn log_line(&self) -> String {
        format!(
            "pharos: offline start verdict={} reason={} items={}",
            self.verdict.label(),
            self.reason,
            self.items
        )
    }
}

/// Which screen a start gets (V1413). `cut_off` is the app's own verdict that the
/// server cannot be reached; `can_play` says the phone has both ports;
/// `items` is how many downloaded items could play.
pub fn decide(cut_off: bool, can_play: bool, items: usize) -> Decision {
    let (verdict, reason) = if !cut_off {
        (Verdict::Normal, "connected")
    } else if !can_play {
        (Verdict::Waiting, "no_offline_store")
    } else if items == 0 {
        (Verdict::Waiting, "nothing_downloaded")
    } else {
        (Verdict::OfflineHome, "downloads_available")
    };
    Decision {
        verdict,
        reason,
        items,
    }
}

/// Whether `now` differs from the decision last logged, and records it. The
/// start screen redraws on every retry of the server; one line per change.
pub fn is_new(last: &mut Option<(Verdict, usize)>, now: &Decision) -> bool {
    let key = (now.verdict, now.items);
    if *last == Some(key) {
        return false;
    }
    *last = Some(key);
    true
}

static LAST_LOGGED: Mutex<Option<(Verdict, usize)>> = Mutex::new(None);

fn log_decision(decision: &Decision) {
    let mut last = LAST_LOGGED.lock().unwrap_or_else(PoisonError::into_inner);
    if is_new(&mut last, decision) {
        platform::info(&decision.log_line());
    }
}

/// One downloaded album, mix or book that can play now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineItem {
    pub container: ContainerRef,
    pub title: String,
    pub subtitle: Option<String>,
    /// The tracks on the phone, in play order.
    pub track_ids: Vec<String>,
    /// Index into `track_ids` of the part with a saved place, if any.
    pub resume_at: Option<usize>,
    pub bytes: u64,
}

impl OfflineItem {
    /// What Play hands the host: the queue and where it starts. A saved
    /// place starts at that part; the host seeks inside it.
    pub fn play_request(&self) -> (Vec<String>, usize) {
        (self.track_ids.clone(), self.resume_at.unwrap_or(0))
    }

    /// What Start over hands the host: the same queue from its first track.
    pub fn restart_request(&self) -> (Vec<String>, usize) {
        (self.track_ids.clone(), 0)
    }

    /// "Artist one · 3 tracks · 12 MiB".
    pub fn detail(&self) -> String {
        let count = self.track_ids.len();
        let tracks = if count == 1 {
            "1 track".to_owned()
        } else {
            format!("{count} tracks")
        };
        let mut parts = Vec::new();
        parts.extend(self.subtitle.clone());
        parts.push(tracks);
        parts.push(format_bytes(self.bytes));
        parts.join(" \u{b7} ")
    }
}

/// The name for an item the host and the device record both leave blank.
fn unnamed(container: &ContainerRef) -> String {
    match container.kind {
        ContainerKind::Album => "Saved album".to_owned(),
        ContainerKind::Mix => "Saved mix".to_owned(),
    }
}

/// The items that can play now: containers with at least one whole track.
/// `saved` is the host's saved place for a track id; the last part with one
/// is where a book resumes.
pub fn build_items(
    downloads: &[Download],
    saved: impl Fn(&str) -> Option<u64>,
) -> Vec<OfflineItem> {
    downloads
        .iter()
        .filter_map(|d| {
            let whole: Vec<_> = d
                .tracks
                .iter()
                .filter(|t| t.state == TrackState::Cached)
                .collect();
            if whole.is_empty() {
                return None;
            }
            let title = if d.title.trim().is_empty() {
                unnamed(&d.container)
            } else {
                container_title(d)
            };
            Some(OfflineItem {
                container: d.container.clone(),
                title,
                subtitle: d.subtitle.clone().filter(|s| !s.trim().is_empty()),
                resume_at: whole.iter().rposition(|t| saved(&t.id).is_some()),
                track_ids: whole.iter().map(|t| t.id.clone()).collect(),
                bytes: whole
                    .iter()
                    .fold(0u64, |sum, t| sum.saturating_add(t.bytes)),
            })
        })
        .collect()
}

/// The song now playing, named from the downloads.
pub fn now_playing_title(downloads: &[Download], current_id: Option<&str>) -> Option<String> {
    let id = current_id?;
    downloads
        .iter()
        .flat_map(|d| d.tracks.iter())
        .find(|t| t.id == id)
        .map(track_title)
}

/// The one button that follows the player's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Pause,
    Resume,
}

pub fn transport(state: PlayState) -> Option<Transport> {
    match state {
        PlayState::Playing | PlayState::Buffering => Some(Transport::Pause),
        PlayState::Paused => Some(Transport::Resume),
        PlayState::Idle | PlayState::Ended => None,
    }
}

/// The list the phone would show now, with the device's noted names filled
/// in. Empty when nothing is downloaded.
fn current_items(store: &dyn OfflineStore) -> (Vec<Download>, Vec<OfflineItem>) {
    let downloads = Names::load().fill(store.downloads());
    let items = build_items(&downloads, |id| store.saved_position_ms(id));
    (downloads, items)
}

/// Of the items that can play now, the queue a song `id` starts: its
/// download's whole tracks, from that song.
pub fn song_queue(items: &[OfflineItem], id: &str) -> Option<(Vec<String>, usize)> {
    items.iter().find_map(|item| {
        let at = item.track_ids.iter().position(|t| t == id)?;
        Some((item.track_ids.clone(), at))
    })
}

/// A cached song asked for with no server (spec 049): the queue it starts
/// on the phone's own player, or `None` when it is not cached or there is
/// no player.
pub fn cached_song(id: &str) -> Option<(Vec<String>, usize)> {
    let store = offline_store()?;
    native_audio()?;
    let (_, items) = current_items(&*store);
    song_queue(&items, id)
}

/// Plays a cached song on the phone's own player, once, and shows what is
/// downloaded with the song playing and its controls.
#[component]
pub fn OfflineSong(id: String, ids: Vec<String>, at: usize) -> Element {
    use_hook(move || match native_audio() {
        Some(audio) => {
            platform::info(&format!(
                "pharos: offline song verdict=play item={id} tracks={} start_index={at}",
                ids.len()
            ));
            audio.play(ids, at);
        }
        None => platform::warn(&format!(
            "pharos: offline song verdict=none item={id} reason=no_player"
        )),
    });
    rsx! { OfflineStart {} }
}

/// Whether anything is downloaded on this phone, for the slow-connect timer.
pub fn any_downloaded() -> bool {
    offline_store().is_some_and(|store| {
        store
            .downloads()
            .iter()
            .any(|d| d.tracks.iter().any(|t| t.state == TrackState::Cached))
    })
}

/// The offline home, or `None` for the waiting page. Logs the decision.
pub fn start() -> Option<Element> {
    let store = offline_store();
    let can_play = store.is_some() && native_audio().is_some();
    let items = match (&store, can_play) {
        (Some(store), true) => current_items(&**store).1.len(),
        _ => 0,
    };
    let decision = decide(true, can_play, items);
    log_decision(&decision);
    (decision.verdict == Verdict::OfflineHome).then(|| rsx! { OfflineStart {} })
}

#[derive(Clone)]
pub struct Audio(pub Arc<dyn NativeAudio>);

impl PartialEq for Audio {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// The offline home over the registered ports; nothing without them.
#[component]
pub fn OfflineStart() -> Element {
    match (offline_store(), native_audio()) {
        (Some(store), Some(audio)) => rsx! {
            OfflineHome { store: super::offline_view::Store(store), audio: Audio(audio) }
        },
        _ => rsx! {},
    }
}

#[component]
pub fn OfflineHome(store: super::offline_view::Store, audio: Audio) -> Element {
    let snap = super::offline_view::use_snapshot(&store);
    let mut player = use_signal(|| audio.0.state().borrow().clone());
    let follow = audio.clone();
    use_future(move || {
        let mut rx: watch::Receiver<PlayerSnapshot> = follow.0.state();
        async move {
            loop {
                let now = rx.borrow_and_update().clone();
                if *player.peek() != now {
                    player.set(now);
                }
                if rx.changed().await.is_err() {
                    break;
                }
            }
        }
    });
    let names = use_hook(Names::load);
    let downloads = names.fill(snap().downloads);
    let items = build_items(&downloads, |id| store.0.saved_position_ms(id));
    let now = player();
    let playing = now_playing_title(&downloads, now.current_id.as_deref());
    let button = transport(now.state);
    rsx! {
        div { class: "pharos-app",
            header { class: "pharos-banner", Brand {} }
            section { class: "pharos-downloads", aria_labelledby: "offline-start-title",
                h2 { id: "offline-start-title", "Downloads" }
                p { class: "pharos-offline-banner", role: "status",
                    "No connection. Playing from downloads."
                }
                p { class: "pharos-muted", "Trying the server again in the background." }
                if let Some(text) = now.error.clone() {
                    p { class: "pharos-notice", role: "alert", "{text}" }
                }
                if let Some(title) = playing {
                    div { class: "pharos-offline-confirm", role: "group", aria_label: "Now playing",
                        span { class: "pharos-download-name", "{title}" }
                        button {
                            class: "pharos-button pharos-button-secondary is-small",
                            r#type: "button",
                            onclick: {
                                let audio = audio.clone();
                                move |_| audio.0.previous()
                            },
                            "Previous"
                        }
                        if let Some(t) = button {
                            button {
                                class: "pharos-button pharos-button-secondary is-small",
                                r#type: "button",
                                onclick: {
                                    let audio = audio.clone();
                                    move |_| match t {
                                        Transport::Pause => audio.0.pause(),
                                        Transport::Resume => audio.0.resume(),
                                    }
                                },
                                if t == Transport::Pause { "Pause" } else { "Resume" }
                            }
                        }
                        button {
                            class: "pharos-button pharos-button-secondary is-small",
                            r#type: "button",
                            onclick: {
                                let audio = audio.clone();
                                move |_| audio.0.next()
                            },
                            "Next"
                        }
                    }
                }
                ul { class: "pharos-download-list",
                    for item in items {
                        OfflineRow {
                            key: "{item.container.kind.label()}-{item.container.id}",
                            item,
                            audio: audio.clone(),
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn OfflineRow(item: OfflineItem, audio: Audio) -> Element {
    let detail = item.detail();
    let resumes = item.resume_at.is_some();
    rsx! {
        li { class: "pharos-download", "data-kind": item.container.kind.label(),
            div { class: "pharos-download-head",
                div { class: "pharos-download-name",
                    span { "{item.title}" }
                    span { class: "pharos-muted", "{detail}" }
                }
                button {
                    class: "pharos-button is-small",
                    r#type: "button",
                    aria_label: if resumes { "Resume {item.title}" } else { "Play {item.title}" },
                    onclick: {
                        let (audio, item) = (audio.clone(), item.clone());
                        move |_| {
                            let (ids, at) = item.play_request();
                            platform::info(&format!(
                                "pharos: offline start play kind={} tracks={} start_index={at} resumes={resumes}",
                                item.container.kind.label(),
                                ids.len()
                            ));
                            audio.0.play(ids, at);
                        }
                    },
                    if resumes { "Resume" } else { "Play" }
                }
                if resumes {
                    button {
                        class: "pharos-button pharos-button-secondary is-small",
                        r#type: "button",
                        aria_label: "Start {item.title} over",
                        onclick: {
                            let (audio, item) = (audio.clone(), item.clone());
                            move |_| {
                                let (ids, at) = item.restart_request();
                                platform::info(&format!(
                                    "pharos: offline start play kind={} tracks={} start_index={at} resumes=false",
                                    item.container.kind.label(),
                                    ids.len()
                                ));
                                audio.0.play_from_start(ids, at);
                            }
                        },
                        "Start over"
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! V1413: the offline start's decision, list and wording.
    use super::*;
    use crate::native::offline_view::Store;
    use crate::native::ports::fake::{FakeAudio, FakeOfflineStore};
    use crate::native::ports::TrackDownload;

    fn album(id: &str) -> ContainerRef {
        ContainerRef {
            kind: ContainerKind::Album,
            id: id.into(),
        }
    }

    fn track(id: &str, state: TrackState, bytes: u64) -> TrackDownload {
        TrackDownload {
            id: id.into(),
            title: format!("Track {id}"),
            state,
            bytes,
        }
    }

    fn download(id: &str, title: &str, tracks: Vec<TrackDownload>) -> Download {
        Download {
            container: album(id),
            title: title.into(),
            subtitle: Some("Author one".into()),
            tracks,
        }
    }

    fn no_saved(_: &str) -> Option<u64> {
        None
    }

    #[test]
    fn connected_is_normal_whatever_is_downloaded() {
        let d = decide(false, true, 3);
        assert_eq!(d.verdict, Verdict::Normal);
        assert_eq!(d.reason, "connected");
    }

    #[test]
    fn cut_off_with_an_empty_store_keeps_the_waiting_page() {
        let d = decide(true, true, 0);
        assert_eq!(d.verdict, Verdict::Waiting);
        assert_eq!(d.reason, "nothing_downloaded");
    }

    #[test]
    fn cut_off_with_downloads_shows_the_offline_home() {
        let d = decide(true, true, 2);
        assert_eq!(d.verdict, Verdict::OfflineHome);
        assert_eq!(d.reason, "downloads_available");
        assert_eq!(d.items, 2);
    }

    #[test]
    fn cut_off_without_the_phone_ports_keeps_the_waiting_page() {
        let d = decide(true, false, 2);
        assert_eq!(d.verdict, Verdict::Waiting);
        assert_eq!(d.reason, "no_offline_store");
    }

    #[test]
    fn the_log_line_names_verdict_reason_and_count() {
        assert_eq!(
            decide(true, true, 2).log_line(),
            "pharos: offline start verdict=offline_home reason=downloads_available items=2"
        );
        assert_eq!(
            decide(true, true, 0).log_line(),
            "pharos: offline start verdict=waiting_page reason=nothing_downloaded items=0"
        );
    }

    #[test]
    fn the_verdict_labels_are_distinct() {
        let mut labels = vec![
            Verdict::Normal.label(),
            Verdict::OfflineHome.label(),
            Verdict::Waiting.label(),
        ];
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), 3);
    }

    #[test]
    fn a_repeated_decision_is_logged_once_and_a_change_again() {
        let mut last = None;
        assert!(is_new(&mut last, &decide(true, true, 0)));
        assert!(!is_new(&mut last, &decide(true, true, 0)));
        assert!(is_new(&mut last, &decide(true, true, 2)));
        assert!(!is_new(&mut last, &decide(true, true, 2)));
        assert!(is_new(&mut last, &decide(false, true, 2)));
    }

    #[test]
    fn only_containers_with_a_whole_track_are_listed_and_only_those_tracks_queue() {
        let items = build_items(
            &[
                download(
                    "1",
                    "Album one",
                    vec![
                        track("a", TrackState::Cached, 100),
                        track("b", TrackState::Queued, 0),
                        track("c", TrackState::Cached, 50),
                    ],
                ),
                download("2", "Album two", vec![track("d", TrackState::Queued, 0)]),
                download("3", "Album three", vec![track("e", TrackState::Failed, 0)]),
            ],
            no_saved,
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].track_ids, ["a", "c"]);
        assert_eq!(items[0].bytes, 150);
        assert_eq!(items[0].resume_at, None);
    }

    #[test]
    fn nothing_downloaded_lists_nothing() {
        assert!(build_items(&[], no_saved).is_empty());
    }

    #[test]
    fn a_book_resumes_at_its_last_part_with_a_saved_place() {
        let d = download(
            "1",
            "Book one",
            vec![
                track("p1", TrackState::Cached, 1),
                track("p2", TrackState::Cached, 1),
                track("p3", TrackState::Cached, 1),
            ],
        );
        let items = build_items(&[d], |id| (id == "p1" || id == "p2").then_some(90_000));
        assert_eq!(items[0].resume_at, Some(1));
        assert_eq!(
            items[0].play_request(),
            (vec!["p1".into(), "p2".into(), "p3".into()], 1)
        );
        assert_eq!(items[0].restart_request().1, 0);
    }

    #[test]
    fn the_resume_index_counts_whole_parts_only() {
        let d = download(
            "1",
            "Book one",
            vec![
                track("p1", TrackState::Queued, 0),
                track("p2", TrackState::Cached, 1),
            ],
        );
        let items = build_items(&[d], |id| (id == "p2").then_some(5_000));
        assert_eq!(items[0].track_ids, ["p2"]);
        assert_eq!(items[0].resume_at, Some(0));
    }

    #[test]
    fn an_item_with_no_saved_place_plays_from_its_first_track() {
        let d = download("1", "Album one", vec![track("a", TrackState::Cached, 1)]);
        let items = build_items(&[d], no_saved);
        assert_eq!(items[0].play_request(), (vec!["a".to_owned()], 0));
    }

    #[test]
    fn an_unnamed_item_is_called_by_its_kind_not_its_id() {
        let mut d = download("987", " ", vec![track("a", TrackState::Cached, 1)]);
        let items = build_items(std::slice::from_ref(&d), no_saved);
        assert_eq!(items[0].title, "Saved album");
        d.container.kind = ContainerKind::Mix;
        let items = build_items(&[d], no_saved);
        assert_eq!(items[0].title, "Saved mix");
    }

    #[test]
    fn the_detail_line_joins_author_count_and_size() {
        let d = download(
            "1",
            "Album one",
            vec![
                track("a", TrackState::Cached, 1024),
                track("b", TrackState::Cached, 1024),
            ],
        );
        let item = &build_items(&[d], no_saved)[0];
        assert_eq!(item.detail(), "Author one \u{b7} 2 tracks \u{b7} 2 KiB");
        let mut one = item.clone();
        one.subtitle = None;
        one.track_ids.truncate(1);
        assert_eq!(one.detail(), "1 track \u{b7} 2 KiB");
    }

    #[test]
    fn the_playing_song_is_named_from_the_downloads() {
        let d = download("1", "Album one", vec![track("a", TrackState::Cached, 1)]);
        assert_eq!(
            now_playing_title(std::slice::from_ref(&d), Some("a")).as_deref(),
            Some("Track a")
        );
        assert_eq!(
            now_playing_title(std::slice::from_ref(&d), Some("zz")),
            None
        );
        assert_eq!(now_playing_title(&[d], None), None);
    }

    #[test]
    fn the_transport_button_follows_the_player_state() {
        assert_eq!(transport(PlayState::Playing), Some(Transport::Pause));
        assert_eq!(transport(PlayState::Buffering), Some(Transport::Pause));
        assert_eq!(transport(PlayState::Paused), Some(Transport::Resume));
        assert_eq!(transport(PlayState::Idle), None);
        assert_eq!(transport(PlayState::Ended), None);
    }

    fn render(store: &Arc<FakeOfflineStore>, audio: &Arc<FakeAudio>) -> String {
        let (s, a): (Arc<dyn OfflineStore>, Arc<dyn NativeAudio>) = (store.clone(), audio.clone());
        let mut dom = VirtualDom::new_with_props(
            |p: (Store, Audio)| rsx! { OfflineHome { store: p.0, audio: p.1 } },
            (Store(s), Audio(a)),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn kept_store() -> Arc<FakeOfflineStore> {
        let fake = Arc::new(FakeOfflineStore::new());
        fake.add_titled(
            album("7"),
            "Book one",
            Some("Author one"),
            vec![
                ("7-1".into(), "Part one".into(), 1024),
                ("7-2".into(), "Part two".into(), 2048),
            ],
        );
        assert_eq!(fake.pin(&album("7")), Ok(()));
        fake
    }

    #[test]
    fn the_home_says_it_is_offline_and_lists_the_downloads_with_play() {
        let html = render(&kept_store(), &Arc::new(FakeAudio::new()));
        assert!(
            html.contains("No connection. Playing from downloads."),
            "{html}"
        );
        assert!(html.contains("Book one"), "{html}");
        assert!(html.contains("Author one"), "{html}");
        assert!(html.contains("2 tracks"), "{html}");
        assert!(html.contains("Play Book one"), "{html}");
        assert!(!html.contains("Start over"), "no saved place: {html}");
    }

    #[test]
    fn a_saved_place_offers_resume_and_start_over() {
        let store = kept_store();
        store.set_saved_position("7-2", 600_000);
        let html = render(&store, &Arc::new(FakeAudio::new()));
        assert!(html.contains("Resume Book one"), "{html}");
        assert!(html.contains("Start Book one over"), "{html}");
    }

    #[test]
    fn the_home_names_the_song_playing_and_offers_pause() {
        let store = kept_store();
        let audio = Arc::new(FakeAudio::new());
        audio.play(vec!["7-1".into(), "7-2".into()], 0);
        let html = render(&store, &audio);
        assert!(html.contains("Part one"), "{html}");
        assert!(html.contains("Pause"), "{html}");
        assert!(html.contains("Next"), "{html}");
    }

    #[test]
    fn the_buttons_are_the_slim_pill_family_and_the_rows_wrap() {
        let store = kept_store();
        store.set_saved_position("7-1", 600_000);
        let html = render(&store, &Arc::new(FakeAudio::new()));
        assert!(html.contains("pharos-button is-small"), "{html}");
        assert!(
            html.contains("pharos-button pharos-button-secondary is-small"),
            "{html}"
        );
        assert!(!html.contains("is-icon"), "{html}");
        assert!(html.contains("pharos-download-head"), "{html}");
    }

    /// A cached song from the device's library starts its download's
    /// queue at that song; one not cached starts nothing.
    #[test]
    fn a_cached_song_starts_its_download_from_that_song() {
        let store = kept_store();
        let items = build_items(&store.downloads(), |_| None);
        assert_eq!(
            song_queue(&items, "7-2"),
            Some((vec!["7-1".to_owned(), "7-2".to_owned()], 1))
        );
        assert_eq!(song_queue(&items, "8-1"), None);
        // No ports registered in a test: nothing to play on.
        assert_eq!(cached_song("7-1"), None);
    }

    #[test]
    fn without_registered_ports_the_start_falls_back_to_the_waiting_page() {
        assert!(start().is_none());
        let mut dom = VirtualDom::new(|| rsx! { OfflineStart {} });
        dom.rebuild_in_place();
        assert_eq!(dioxus_ssr::render(&dom), "");
        assert!(!any_downloaded());
    }
}
