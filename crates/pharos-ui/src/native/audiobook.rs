//! An audiobook's own player controls (ADR-0132, spec 046): speed, a sleep
//! timer and the chapter list. They show only while an audiobook plays; the
//! music player beside them is unchanged.

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
    // Bumped by every choice, so an older timer that wakes does nothing.
    let mut sleep_gen = use_signal(|| 0u64);
    let mut tracks = use_signal(Vec::<Item>::new);
    let folder = book.chapters.is_empty();

    // A new source starts at the element's default rate, which `set_speed`
    // also sets; a song after the book plays at normal speed again.
    let want = speed();
    use_effect(move || {
        let want = speed();
        if platform::speed(media_id) != want {
            platform::set_speed(media_id, want);
        }
    });
    use_drop(move || platform::set_speed(media_id, 1.0));

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
        }
    }));

    let mut choose_sleep = move |next: Sleep, pos: Option<u64>| {
        let gen = *sleep_gen.peek() + 1;
        sleep_gen.set(gen);
        sleep.set(next);
        sleep_pos.set(pos);
        platform::info(&format!(
            "pharos: audiobook sleep set timer={} position_ms={} end_ms={pos:?}",
            next.token(),
            media.position_ms
        ));
        if let Sleep::Minutes(m) = next {
            spawn(async move {
                platform::sleep_ms(m.saturating_mul(60_000)).await;
                if *sleep_gen.peek() == gen {
                    platform::info(&format!("pharos: audiobook sleep fired timer={m}m"));
                    platform::pause(media_id);
                    sleep.set(Sleep::Off);
                }
            });
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
        div { class: "pharos-audiobook", role: "group", aria_label: "Audiobook",
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
