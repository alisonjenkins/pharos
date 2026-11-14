//! An audiobook's own player controls (ADR-0132, spec 046): speed, a sleep
//! timer and the chapter list. They show only while an audiobook plays; the
//! music player beside them is unchanged.

use std::cell::Cell;
use std::rc::Rc;

use dioxus::prelude::*;
use pharos_client::api::{Chapter, Item, ItemKind, ItemRef};

use super::app::{call, Ctx};
use super::platform;
use super::playback::{clock, MediaState};
use super::watch_view::Control;

/// The speeds offered, 1.0 being normal.
pub const SPEEDS: [f64; 6] = [0.75, 1.0, 1.25, 1.5, 1.75, 2.0];

/// Device-local: the speed audiobooks play at. Kept apart from a video's
/// speed, so a book at 1.5x never speeds up a film or a song.
const SPEED_KEY: &str = "pharos.audiobook.speed";

/// The speed audiobooks play at on this device; 1.0 when none is kept or
/// the kept one is not offered.
pub fn stored_speed() -> f64 {
    let kept = platform::stored_theme(SPEED_KEY).ok().flatten();
    parse_speed(kept.as_deref())
}

/// A kept speed, when it is one [`SPEEDS`] offers.
pub fn parse_speed(kept: Option<&str>) -> f64 {
    kept.and_then(|s| s.parse::<f64>().ok())
        .filter(|s| SPEEDS.contains(s))
        .unwrap_or(1.0)
}

/// "1.25×".
pub fn speed_label(speed: f64) -> String {
    format!("{speed}×")
}

/// When the sleep timer pauses the book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sleep {
    Off,
    Minutes(u32),
    /// At the end of the chapter playing when it was set.
    EndOfChapter,
}

impl Sleep {
    pub const ALL: [Self; 5] = [
        Self::Off,
        Self::Minutes(15),
        Self::Minutes(30),
        Self::Minutes(60),
        Self::EndOfChapter,
    ];

    pub fn label(self) -> String {
        match self {
            Self::Off => "Off".to_owned(),
            Self::Minutes(m) => format!("{m} min"),
            Self::EndOfChapter => "End of chapter".to_owned(),
        }
    }

    /// The token logged for the choice.
    pub fn token(self) -> String {
        match self {
            Self::Off => "off".to_owned(),
            Self::Minutes(m) => format!("{m}m"),
            Self::EndOfChapter => "chapter".to_owned(),
        }
    }
}

/// The chapter of `chapters` (in start order) playing at `ms`.
pub fn chapter_at(chapters: &[Chapter], ms: u64) -> Option<usize> {
    chapters.iter().rposition(|c| c.start_ms <= ms)
}

/// Where chapter `at` ends: the next one's start, else the book's end.
pub fn chapter_end(chapters: &[Chapter], at: usize, runtime_ms: u64) -> u64 {
    chapters
        .get(at.saturating_add(1))
        .map_or(runtime_ms, |c| c.start_ms)
}

/// How near its end a folder book's track pauses for "end of chapter", so
/// the queue does not move on to the next track first.
const TRACK_END_MARGIN_MS: u64 = 750;

/// Where an armed sleep timer pauses the book, as a position in the file
/// playing: `None` for no timer, or a timer kept by the clock.
pub fn sleep_at(
    sleep: Sleep,
    chapters: &[Chapter],
    position_ms: u64,
    runtime_ms: u64,
) -> Option<u64> {
    if sleep != Sleep::EndOfChapter {
        return None;
    }
    match chapter_at(chapters, position_ms) {
        Some(at) => Some(chapter_end(chapters, at, runtime_ms)),
        // A folder book's file is the chapter.
        None => Some(runtime_ms.saturating_sub(TRACK_END_MARGIN_MS)),
    }
}

/// Whether a sleep timer pausing at `at` stops the book at the end of the
/// file playing, where the player would otherwise move on to the next part.
pub fn ends_with_track(at: Option<u64>, runtime_ms: u64) -> bool {
    at.is_some_and(|at| at.saturating_add(TRACK_END_MARGIN_MS) >= runtime_ms)
}

/// Set while an armed sleep timer ends with the file playing. The player
/// reads it before a crossfade or a file's end moves the queue on: a
/// crossfade starts the next part seconds before the timer's pause.
#[derive(Debug, Clone, Default)]
pub struct StopAtEnd(Rc<Cell<bool>>);

impl PartialEq for StopAtEnd {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

impl StopAtEnd {
    pub fn arm(&self, on: bool) {
        self.0.set(on);
    }

    pub fn armed(&self) -> bool {
        self.0.get()
    }
}

/// A minute sleep timer, kept as the wall-clock time it ends at. It lives
/// above the per-part player slot (the app root provides it), so a hand-over
/// to the next part, which drops the old controls and their tasks, neither
/// cancels it nor resets it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SleepDeadline {
    /// The book it was set for, see [`book_key`].
    pub book: ItemRef,
    pub minutes: u32,
    pub ends_at_ms: f64,
}

/// The shared slot holding the armed minute timer, if any.
#[derive(Clone, Copy)]
pub struct SleepTimer(pub Signal<Option<SleepDeadline>>);

/// What a newly mounted set of controls does with the shared timer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Carry {
    None,
    /// Still running for this book: show it and wait out the rest.
    Resume {
        remaining_ms: u64,
    },
    /// Ran out while no controls were watching: dropped, not fired.
    Lapsed {
        late_ms: u64,
    },
    /// Set for another book: dropped.
    OtherBook,
}

impl Carry {
    pub fn token(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Resume { .. } => "resume",
            Self::Lapsed { .. } => "lapsed",
            Self::OtherBook => "other_book",
        }
    }
}

/// What identifies a book across its parts: the folder book's album, else
/// the file playing.
pub fn book_key(book: &Audiobook) -> ItemRef {
    book.book.unwrap_or(book.playing)
}

/// Whether a timer found at mount is carried on, by `book` at `now_ms`.
pub fn carry_over(timer: Option<SleepDeadline>, book: ItemRef, now_ms: f64) -> Carry {
    let Some(timer) = timer else {
        return Carry::None;
    };
    if timer.book != book {
        return Carry::OtherBook;
    }
    let left = timer.ends_at_ms - now_ms;
    if left > 0.0 {
        Carry::Resume {
            remaining_ms: left as u64,
        }
    } else {
        Carry::Lapsed {
            late_ms: (-left) as u64,
        }
    }
}

/// Waits out `deadline`, then pauses what `owned` names, unless a newer
/// choice replaced or cleared it meanwhile.
fn watch_deadline(
    mut timer: Signal<Option<SleepDeadline>>,
    mut sleep: Signal<Sleep>,
    owned: Rc<Cell<&'static str>>,
    deadline: SleepDeadline,
    remaining_ms: u64,
) {
    spawn(async move {
        platform::sleep_ms(u32::try_from(remaining_ms).unwrap_or(u32::MAX)).await;
        if *timer.peek() == Some(deadline) {
            platform::info(&format!(
                "pharos: audiobook sleep fired timer={}m book={}",
                deadline.minutes, deadline.book
            ));
            platform::pause(owned.get());
            timer.set(None);
            sleep.set(Sleep::Off);
        }
    });
}

/// Whether a key on the controls is a button's own activation. The player
/// reads Space as play / pause, so it must not see these.
pub fn activates_button(key: &Key) -> bool {
    matches!(key, Key::Named(NamedKey::Enter)) || matches!(key, Key::Character(k) if k == " ")
}

/// The track an audiobook's Play starts on: the last one begun and not
/// finished, else the first not finished, else the first (V1403).
pub fn resume_track(tracks: &[Item]) -> Option<&Item> {
    let songs = || tracks.iter().filter(|t| t.kind == ItemKind::Audio);
    songs()
        .rfind(|t| t.position_ms > 0 && !t.played)
        .or_else(|| songs().find(|t| !t.played))
        .or_else(|| songs().next())
}

/// What the audiobook controls need to know about the book playing.
#[derive(Debug, Clone, PartialEq)]
pub struct Audiobook {
    /// The file's own chapters; empty for a folder book's track.
    pub chapters: Vec<Chapter>,
    /// The book (its album), whose tracks are a folder book's chapters.
    pub book: Option<ItemRef>,
    /// The track playing.
    pub playing: ItemRef,
    /// Shared with the player playing it.
    pub stop_at_end: StopAtEnd,
}

/// Speed, sleep timer and chapters, under the music controls.
#[component]
pub fn AudiobookControls(
    book: Audiobook,
    media: MediaState,
    runtime_ms: u64,
    media_id: &'static str,
    on_control: EventHandler<Control>,
) -> Element {
    let ctx = try_use_context::<Ctx>();
    let mut speed = use_signal(stored_speed);
    let mut sleep = use_signal(|| Sleep::Off);
    // The position the armed "end of chapter" pauses at.
    let mut sleep_pos = use_signal(|| None::<u64>);
    let local_timer = use_signal(|| None::<SleepDeadline>);
    let mut timer = try_use_context::<SleepTimer>().map_or(local_timer, |t| t.0);
    let key = book_key(&book);
    let mut tracks = use_signal(Vec::<Item>::new);
    let folder = book.chapters.is_empty();

    // The element this instance drives now: a crossfade moves a part to
    // the outgoing element and gives the playing one to the next part.
    let owned = use_hook(|| Rc::new(Cell::new(media_id)));
    owned.set(media_id);

    // A new source starts at the element's default rate, which `set_speed`
    // also sets; a song after the book plays at normal speed again.
    let want = speed();
    use_effect({
        let owned = Rc::clone(&owned);
        move || {
            let want = speed();
            let media_id = owned.get();
            if platform::speed(media_id) != want {
                platform::set_speed(media_id, want);
            }
        }
    });
    use_drop({
        let owned = Rc::clone(&owned);
        move || platform::set_speed(owned.get(), 1.0)
    });

    // A minute timer set on an earlier part carries over the hand-over.
    use_hook({
        let owned = Rc::clone(&owned);
        move || {
            let found = *timer.peek();
            let carry = carry_over(found, key, platform::wall_clock_ms());
            if carry != Carry::None {
                platform::info(&format!(
                    "pharos: audiobook sleep carry={} book={key} timer={:?}",
                    carry.token(),
                    found.map(|d| d.minutes),
                ));
            }
            match (carry, found) {
                (Carry::Resume { remaining_ms }, Some(d)) => {
                    sleep.set(Sleep::Minutes(d.minutes));
                    watch_deadline(timer, sleep, owned, d, remaining_ms);
                }
                (Carry::Lapsed { .. } | Carry::OtherBook, _) => timer.set(None),
                _ => {}
            }
        }
    });
    let owned_sig = use_signal(|| Rc::clone(&owned));

    // A folder book's chapters are its tracks.
    let album = book.book;
    use_hook(move || {
        let (Some(album), true) = (album, folder) else {
            return;
        };
        let Some(api) = ctx.and_then(|c| c.api.peek().clone()) else {
            return;
        };
        spawn(async move {
            match super::browse::list_all(
                &api,
                &call("audiobook", "chapters"),
                album,
                None,
                super::browse::INDEX_ORDER,
            )
            .await
            {
                Ok(found) => tracks.set(found),
                Err(e) => platform::warn(&format!(
                    "pharos: audiobook chapters unavailable book={album:?} error={e}"
                )),
            }
        });
    });

    // "End of chapter" pauses once the playhead reaches the armed end.
    // A signal only so the click handlers stay `Copy`.
    let stop_at_end = use_signal(|| book.stop_at_end.clone());
    use_effect(use_reactive!(|(media,)| {
        let Some(at) = *sleep_pos.peek() else {
            return;
        };
        if !media.paused && media.position_ms >= at {
            platform::info(&format!(
                "pharos: audiobook sleep fired timer=chapter position_ms={} end_ms={at}",
                media.position_ms
            ));
            platform::pause(media_id);
            sleep_pos.set(None);
            sleep.set(Sleep::Off);
            stop_at_end.peek().arm(false);
        }
    }));

    let mut choose_sleep = move |next: Sleep, pos: Option<u64>| {
        timer.set(None);
        sleep.set(next);
        sleep_pos.set(pos);
        let holds = ends_with_track(pos, runtime_ms);
        stop_at_end.peek().arm(holds);
        platform::info(&format!(
            "pharos: audiobook sleep set timer={} position_ms={} end_ms={pos:?} holds_queue={holds}",
            next.token(),
            media.position_ms
        ));
        if let Sleep::Minutes(m) = next {
            let ms = u64::from(m).saturating_mul(60_000);
            let deadline = SleepDeadline {
                book: key,
                minutes: m,
                ends_at_ms: platform::wall_clock_ms() + ms as f64,
            };
            timer.set(Some(deadline));
            watch_deadline(timer, sleep, Rc::clone(&owned_sig.peek()), deadline, ms);
        }
    };

    let here = chapter_at(&book.chapters, media.position_ms);
    let rows: Vec<(String, String, Control, bool)> = if folder {
        tracks()
            .iter()
            .filter(|t| t.kind == ItemKind::Audio)
            .map(|t| {
                (
                    t.name.clone(),
                    t.episode.map(|n| n.to_string()).unwrap_or_default(),
                    Control::Play(t.id),
                    t.id == book.playing,
                )
            })
            .collect()
    } else {
        book.chapters
            .iter()
            .enumerate()
            .map(|(i, c)| {
                (
                    c.name.clone(),
                    clock(c.start_ms),
                    Control::Seek(c.start_ms),
                    Some(i) == here,
                )
            })
            .collect()
    };
    let current = sleep();
    let sleeps: Vec<(Sleep, Option<u64>)> = Sleep::ALL
        .into_iter()
        .map(|s| {
            (
                s,
                sleep_at(s, &book.chapters, media.position_ms, runtime_ms),
            )
        })
        .collect();
    rsx! {
        div {
            class: "pharos-audiobook",
            role: "group",
            aria_label: "Audiobook",
            // Keys on the controls' own buttons are theirs, not the player's.
            onkeydown: move |evt: KeyboardEvent| {
                if activates_button(&evt.key()) {
                    evt.stop_propagation();
                }
            },
            div { class: "pharos-audiobook-row", role: "group", aria_label: "Speed",
                span { class: "pharos-audiobook-label", "Speed" }
                for s in SPEEDS {
                    button {
                        key: "{s}",
                        r#type: "button",
                        class: "pharos-chip is-small",
                        aria_pressed: if want == s { "true" } else { "false" },
                        onclick: move |_| {
                            platform::info(&format!("pharos: audiobook speed from={} to={s}", *speed.peek()));
                            speed.set(s);
                            if let Err(e) = platform::store_theme(SPEED_KEY, &s.to_string()) {
                                platform::warn(&format!("pharos: audiobook speed not kept: {e}"));
                            }
                        },
                        {speed_label(s)}
                    }
                }
            }
            div { class: "pharos-audiobook-row", role: "group", aria_label: "Sleep timer",
                span { class: "pharos-audiobook-label", "Sleep" }
                for (s, pos) in sleeps {
                    button {
                        key: "{s.token()}",
                        r#type: "button",
                        class: "pharos-chip is-small",
                        aria_pressed: if current == s { "true" } else { "false" },
                        onclick: move |_| choose_sleep(s, pos),
                        {s.label()}
                    }
                }
            }
            if !rows.is_empty() {
                details { class: "pharos-audiobook-chapters",
                    summary { "Chapters ({rows.len()})" }
                    ol {
                        for (i, (name, at, control, on)) in rows.into_iter().enumerate() {
                            li { key: "{i}",
                                button {
                                    r#type: "button",
                                    class: "pharos-audiobook-chapter",
                                    aria_current: if on { "true" } else { "false" },
                                    onclick: move |_| on_control.call(control),
                                    span { class: "pharos-audiobook-chapter-name", "{name}" }
                                    span { class: "pharos-muted", "{at}" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn ch(name: &str, start_ms: u64) -> Chapter {
        Chapter {
            name: name.into(),
            start_ms,
        }
    }

    #[test]
    fn a_position_falls_in_its_chapter() {
        let chapters = [ch("One", 0), ch("Two", 60_000), ch("Three", 120_000)];
        assert_eq!(chapter_at(&chapters, 0), Some(0));
        assert_eq!(chapter_at(&chapters, 59_999), Some(0));
        assert_eq!(chapter_at(&chapters, 60_000), Some(1));
        assert_eq!(chapter_at(&chapters, 500_000), Some(2));
        assert_eq!(chapter_at(&[], 5), None);
        assert_eq!(chapter_end(&chapters, 1, 180_000), 120_000);
        assert_eq!(chapter_end(&chapters, 2, 180_000), 180_000);
    }

    #[test]
    fn end_of_chapter_pauses_at_the_chapter_or_the_track_end() {
        let chapters = [ch("One", 0), ch("Two", 60_000)];
        assert_eq!(
            sleep_at(Sleep::EndOfChapter, &chapters, 10_000, 90_000),
            Some(60_000)
        );
        assert_eq!(
            sleep_at(Sleep::EndOfChapter, &[], 10_000, 90_000),
            Some(89_250)
        );
        assert_eq!(
            sleep_at(Sleep::Minutes(15), &chapters, 10_000, 90_000),
            None
        );
        assert_eq!(sleep_at(Sleep::Off, &chapters, 10_000, 90_000), None);
    }

    #[test]
    fn a_timer_at_the_file_end_holds_the_queue() {
        let chapters = [ch("One", 0), ch("Two", 60_000)];
        let at = |pos| sleep_at(Sleep::EndOfChapter, &chapters, pos, 90_000);
        assert!(
            !ends_with_track(at(10_000), 90_000),
            "chapter one ends mid-file"
        );
        assert!(
            ends_with_track(at(70_000), 90_000),
            "the last chapter ends the file"
        );
        let folder = sleep_at(Sleep::EndOfChapter, &[], 10_000, 90_000);
        assert!(
            ends_with_track(folder, 90_000),
            "a folder book's file is the chapter"
        );
        assert!(!ends_with_track(None, 90_000));
    }

    /// A part handed to the outgoing element by a crossfade resets that
    /// element's speed when it goes, not the next part's.
    // `take_speed_sets` is the host platform's test hook; Android tests build the WebView platform.
    #[cfg(not(all(feature = "mobile", target_os = "android")))]
    #[test]
    fn leaving_resets_only_the_element_still_driven() {
        use super::super::watch_view::{OUTGOING_ID, VIDEO_ID};
        use dioxus::dioxus_core::{NoOpMutations, ScopeId};
        #[derive(Clone)]
        struct Props {
            id: Rc<Cell<&'static str>>,
            shown: Rc<Cell<bool>>,
        }
        impl PartialEq for Props {
            fn eq(&self, _: &Self) -> bool {
                false
            }
        }
        let props = Props {
            id: Rc::new(Cell::new(VIDEO_ID)),
            shown: Rc::new(Cell::new(true)),
        };
        let mut dom = VirtualDom::new_with_props(
            |p: Props| {
                let media_id = p.id.get();
                rsx! {
                    if p.shown.get() {
                        AudiobookControls {
                            book: Audiobook {
                                chapters: Vec::new(),
                                book: None,
                                playing: ItemRef::Real(1),
                                stop_at_end: StopAtEnd::default(),
                            },
                            media: MediaState::default(),
                            runtime_ms: 60_000,
                            media_id,
                            on_control: |_| {},
                        }
                    }
                }
            },
            props.clone(),
        );
        dom.rebuild_in_place();
        let mut render = || {
            dom.mark_dirty(ScopeId::APP);
            dom.render_immediate(&mut NoOpMutations);
        };
        props.id.set(OUTGOING_ID);
        render();
        platform::take_speed_sets();
        props.shown.set(false);
        render();
        assert_eq!(
            platform::take_speed_sets(),
            [(OUTGOING_ID.to_owned(), 1.0)],
            "the next part's element keeps its speed"
        );
    }

    fn timer(book: ItemRef, ends_at_ms: f64) -> Option<SleepDeadline> {
        Some(SleepDeadline {
            book,
            minutes: 30,
            ends_at_ms,
        })
    }

    #[test]
    fn a_running_timer_carries_to_the_next_part() {
        let book = ItemRef::Synth(5);
        assert_eq!(
            carry_over(timer(book, 10_000.0), book, 4_000.0),
            Carry::Resume {
                remaining_ms: 6_000
            }
        );
    }

    #[test]
    fn a_timer_that_ran_out_unwatched_or_for_another_book_is_dropped() {
        let book = ItemRef::Synth(5);
        assert_eq!(
            carry_over(timer(book, 10_000.0), book, 12_500.0),
            Carry::Lapsed { late_ms: 2_500 }
        );
        assert_eq!(
            carry_over(timer(ItemRef::Synth(6), 10_000.0), book, 4_000.0),
            Carry::OtherBook
        );
        assert_eq!(carry_over(None, book, 0.0), Carry::None);
    }

    #[test]
    fn carry_verdicts_log_distinct_tokens() {
        let tokens: std::collections::HashSet<_> = [
            Carry::None,
            Carry::Resume { remaining_ms: 1 },
            Carry::Lapsed { late_ms: 1 },
            Carry::OtherBook,
        ]
        .map(Carry::token)
        .into();
        assert_eq!(tokens.len(), 4);
    }

    /// A part's book is its album, so every part shares one key.
    #[test]
    fn every_part_of_a_folder_book_shares_a_key() {
        let part = |n| Audiobook {
            chapters: Vec::new(),
            book: Some(ItemRef::Synth(5)),
            playing: ItemRef::Real(n),
            stop_at_end: StopAtEnd::default(),
        };
        assert_eq!(book_key(&part(1)), book_key(&part(2)));
        let single = Audiobook {
            book: None,
            ..part(9)
        };
        assert_eq!(book_key(&single), ItemRef::Real(9));
    }

    /// Controls mounted on the next part show the timer left running.
    #[test]
    fn new_controls_show_a_carried_timer() {
        let mut dom = VirtualDom::new(|| {
            let book = ItemRef::Synth(5);
            use_context_provider(|| {
                SleepTimer(Signal::new(timer(
                    book,
                    platform::wall_clock_ms() + 600_000.0,
                )))
            });
            rsx! {
                AudiobookControls {
                    book: Audiobook {
                        chapters: Vec::new(),
                        book: Some(book),
                        playing: ItemRef::Real(2),
                        stop_at_end: StopAtEnd::default(),
                    },
                    media: MediaState::default(),
                    runtime_ms: 60_000,
                    media_id: "v",
                    on_control: |_| {},
                }
            }
        });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        let on = html
            .split("<button")
            .find(|b| b.contains("30 min"))
            .unwrap_or("");
        assert!(on.contains("aria-pressed=\"true\""), "{html}");
    }

    #[test]
    fn space_and_enter_stay_with_the_focused_control() {
        assert!(activates_button(&Key::Character(" ".into())));
        assert!(activates_button(&Key::Named(NamedKey::Enter)));
        assert!(!activates_button(&Key::Character("m".into())));
        assert!(!activates_button(&Key::Named(NamedKey::ArrowRight)));
    }

    #[test]
    fn only_an_offered_speed_is_kept() {
        assert_eq!(parse_speed(Some("1.5")), 1.5);
        assert_eq!(parse_speed(Some("3")), 1.0);
        assert_eq!(parse_speed(Some("fast")), 1.0);
        assert_eq!(parse_speed(None), 1.0);
        assert_eq!(speed_label(0.75), "0.75×");
    }

    #[test]
    fn sleep_tokens_are_distinct() {
        let tokens: std::collections::HashSet<String> =
            Sleep::ALL.iter().map(|s| s.token()).collect();
        assert_eq!(tokens.len(), Sleep::ALL.len());
    }

    fn track(n: u64, position_ms: u64, played: bool) -> Item {
        Item {
            id: ItemRef::Real(n),
            kind: ItemKind::Audio,
            name: format!("Part {n}"),
            year: None,
            runtime_ms: 600_000,
            image: None,
            played,
            favorite: false,
            position_ms,
            series_name: None,
            season: None,
            episode: Some(u32::try_from(n).unwrap()),
            parent: None,
            library_kind: None,
            imdb_rating: None,
            critic_rating: None,
            artist: None,
            stereo: None,
            damaged: false,
            new_children: 0,
            audiobook: true,
        }
    }

    #[test]
    fn play_resumes_the_track_last_begun() {
        let fresh = [track(1, 0, false), track(2, 0, false)];
        assert_eq!(resume_track(&fresh).unwrap().id, ItemRef::Real(1));
        let begun = [
            track(1, 0, true),
            track(2, 90_000, false),
            track(3, 0, false),
        ];
        assert_eq!(resume_track(&begun).unwrap().id, ItemRef::Real(2));
        let between = [track(1, 0, true), track(2, 0, true), track(3, 0, false)];
        assert_eq!(resume_track(&between).unwrap().id, ItemRef::Real(3));
        let done = [track(1, 0, true), track(2, 0, true)];
        assert_eq!(resume_track(&done).unwrap().id, ItemRef::Real(1));
        assert!(resume_track(&[]).is_none());
    }
}
