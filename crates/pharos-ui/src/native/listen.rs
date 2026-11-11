//! The now-playing screen for a song: art, title and the audio controls over
//! a shader visualiser (013 US7) drawn by the GPU layer (ADR-0039).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use dioxus::prelude::*;

use super::appearance::{not_saved, Lasts, Notice, NoticeLine};
use super::browse::track_length;
use super::keymap::{self, Mode, Resolved};
use super::lyrics::LyricsPanel;
use super::mixes::{MixBar, MixCtx};
use super::platform;
use super::playback::{clock, MediaEvent, MediaState};
use super::queue::{Entry, QueueAction, Source};
use super::style_picker::StylePicker;
use super::visualiser::{Analysis, AudioTap, Style, STYLE_KEY};
use super::watch_view::{
    not_transport, slider_key, total_ms, transport_key, use_shortcuts, Control, Volume,
    SEEK_STEP_MS, VIDEO_ID,
};
use pharos_client::api::{ClientEvent, ClientEventKind, ClientLevel, ItemRef, Lyrics};

use crate::render::{self, Fallback, Frame, Surface};

pub const CANVAS_ID: &str = "pharos-visualiser";
#[cfg_attr(all(feature = "mobile", target_os = "android"), allow(dead_code))]
const PREFERS_REDUCED_MOTION: &str = "(prefers-reduced-motion: reduce)";
/// How often the theme colours are re-read, so a theme change mid-song
/// retints without a style lookup every frame.
const PALETTE_REFRESH_MS: f64 = 1000.0;

/// How close the playhead must be to a requested seek to count as arrived.
const SEEK_SETTLE_MS: u64 = 1_000;

/// How long a released scrub may wait for its seek before the bar follows
/// the playhead again, so a seek that never lands cannot pin the thumb.
const SEEK_GIVE_UP_MS: u32 = 5_000;

/// Device-local: whether the listener hid the lyrics panel.
const LYRICS_KEY: &str = "pharos.listen.lyrics";
const LYRICS_HIDDEN: &str = "hidden";

/// Device-local: whether the listener turned karaoke mode on (B483/V427).
/// It starts off.
const KARAOKE_KEY: &str = "pharos.listen.karaoke";
const KARAOKE_ON: &str = "on";

/// Whether the listener has karaoke mode on on this device.
pub fn karaoke_on() -> bool {
    platform::stored_theme(KARAOKE_KEY)
        .ok()
        .flatten()
        .as_deref()
        == Some(KARAOKE_ON)
}

/// Device-local: whether the listener opened the queue. It starts closed,
/// so the visualiser keeps the screen (B435/V375).
const QUEUE_KEY: &str = "pharos.listen.queue";
const QUEUE_SHOWN: &str = "shown";
pub const QUEUE_PANEL_ID: &str = "pharos-queue";

/// Device-local: whether the listener docked the queue beside the mini bar
/// or floating window (B900/V852). It starts closed, so the page keeps the
/// room.
const DOCK_KEY: &str = "pharos.stage.queue";

/// Whether the queue is docked beside the compact player on this device.
pub fn dock_shown() -> bool {
    platform::stored_theme(DOCK_KEY).ok().flatten().as_deref() == Some(QUEUE_SHOWN)
}

/// Remember whether the queue is docked beside the compact player.
pub fn store_dock(shown: bool) {
    if let Err(e) = platform::store_theme(DOCK_KEY, if shown { QUEUE_SHOWN } else { "hidden" }) {
        platform::warn(&format!("pharos: docked queue choice not stored error={e}"));
    }
}

/// Shows the outcome of storing a listen-view choice: the shared notice on a
/// failure (logged as `<log> not stored`), nothing once a later one is stored.
fn note_stored(
    mut note: Signal<Option<Notice>>,
    result: Result<(), String>,
    log: &str,
    what: &str,
) {
    match result {
        Ok(()) => {
            if note.peek().is_some() {
                note.set(None);
            }
        }
        Err(e) => {
            platform::warn(&format!("pharos: {log} not stored error={e}"));
            note.set(Some(not_saved(what, &e, Lasts::ThisSong)));
        }
    }
}

/// The analyser on this screen's media element. Web Audio routes an element
/// once, so it outlives style changes and is shared with each visualiser.
#[derive(Clone, Default)]
struct Tap {
    tap: Rc<RefCell<Option<Box<dyn AudioTap>>>>,
    tried: Rc<Cell<bool>>,
    /// Why the last attempt waited, so each new reason is logged once.
    waiting: Rc<Cell<Option<&'static str>>>,
}

impl PartialEq for Tap {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.tap, &other.tap)
    }
}

#[cfg_attr(all(feature = "mobile", target_os = "android"), allow(dead_code))]
impl Tap {
    /// Route the media element `media_id` through an analyser once it is
    /// playing and the page has had a gesture; one attempt per screen. Asked
    /// from the element's `playing` as well as the visualiser's frames, so a
    /// song is tapped with the player shrunk too (B734, V682): the tap's
    /// running context is what keeps the output awake through a pause.
    fn connect(&self, media_id: &str) {
        if self.tried.get() {
            return;
        }
        let waiting = if !platform::media_playing(media_id) {
            Some("not_playing")
        } else if !platform::user_activated() {
            Some("no_activation")
        } else {
            None
        };
        if let Some(reason) = waiting {
            if self.waiting.replace(Some(reason)) != Some(reason) {
                platform::info(&format!("pharos: visualiser tap=waiting reason={reason}"));
            }
            return;
        }
        self.tried.set(true);
        match platform::audio_tap(media_id) {
            Ok(tap) => {
                platform::info("pharos: visualiser tap=connected");
                *self.tap.borrow_mut() = Some(tap);
            }
            Err(e) => platform::warn(&format!("pharos: visualiser tap=failed error={e}")),
        }
    }
}

/// A drag on the seek bar.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Scrub {
    ms: u64,
    /// The seek is requested; the scrub ends when it lands.
    released: bool,
}

impl Scrub {
    fn landed(self, position_ms: u64) -> bool {
        self.released && self.ms.abs_diff(position_ms) < SEEK_SETTLE_MS
    }
}

#[component]
pub fn ListenView(
    title: String,
    image: Option<String>,
    /// The track's artists, in display order.
    #[props(default)]
    artists: Vec<String>,
    #[props(default)] album: Option<String>,
    /// The track's number on its album (Jellyfin's `IndexNumber`).
    #[props(default)]
    track: Option<u32>,
    year: Option<u16>,
    genres: Vec<String>,
    runtime_ms: u64,
    media: MediaState,
    /// The tracks before and after this one on its album.
    previous: Option<ItemRef>,
    next: Option<ItemRef>,
    /// The song's lyrics; `None` while they load or when the server has none.
    lyrics: Option<Lyrics>,
    /// The lyrics are still being asked for, so `None` is not yet an answer.
    #[props(default)]
    lyrics_loading: bool,
    /// The lyrics lookup failed, so whether there are any is not known.
    #[props(default)]
    lyrics_failed: bool,
    /// What plays after this song (B435/V375).
    #[props(default)]
    queue: Vec<Entry>,
    #[props(default)] shuffled: bool,
    #[props(default)] on_queue: EventHandler<QueueAction>,
    /// The playing song, when the queue decides what follows it (not in a
    /// group): what "More like this song" starts a mix from (spec 024).
    #[props(default)]
    playing: Option<ItemRef>,
    on_media: EventHandler<MediaEvent>,
    on_control: EventHandler<Control>,
    /// The player is a floating window or a mini bar (spec 020): the
    /// screen is out of sight, so it draws no visualiser.
    #[props(default)]
    compact: bool,
    /// This screen's slot is the one in sight. A song handing over mounts
    /// two screens; only the one in sight draws the queue panel (V852).
    #[props(default = true)]
    in_sight: bool,
    /// The `<audio>` element's id: [`VIDEO_ID`], or another while this song
    /// hands over to the next.
    #[props(default = VIDEO_ID)]
    media_id: &'static str,
    /// The position as a signal, when the player keeps it apart from `media`
    /// so its own render does not run on every tick (B815).
    #[props(default)]
    live_position: Option<Signal<u64>>,
    /// The notice a choice this device could not store starts with; tests
    /// render it, the app leaves it unset.
    #[props(default)]
    not_stored: Option<Notice>,
) -> Element {
    let media = MediaState {
        position_ms: live_position.map_or(media.position_ms, |s| s()),
        ..media
    };
    let tap = use_hook(Tap::default);
    // A choice below that this device could not store (FR-705).
    let not_stored = use_signal(move || not_stored);
    let shortcuts = use_shortcuts(media, previous, next, on_control).for_song();
    let mut style =
        use_signal(|| Style::parse(platform::stored_theme(STYLE_KEY).ok().flatten().as_deref()));
    let mut fallback = use_signal(|| None::<Fallback>);
    let mut show_lyrics = use_signal(|| {
        platform::stored_theme(LYRICS_KEY).ok().flatten().as_deref() != Some(LYRICS_HIDDEN)
    });
    let mut toggle_lyrics = move || {
        let show = !show_lyrics();
        show_lyrics.set(show);
        note_stored(
            not_stored,
            platform::store_theme(LYRICS_KEY, if show { "shown" } else { LYRICS_HIDDEN }),
            "lyrics choice",
            "whether lyrics show",
        );
    };
    let lyrics = lyrics.filter(|l| !l.lines.is_empty());
    let mut karaoke = use_signal(karaoke_on);
    let mut toggle_karaoke = move || {
        let on = !karaoke();
        karaoke.set(on);
        platform::info(&format!("pharos: karaoke on={on}"));
        note_stored(
            not_stored,
            platform::store_theme(KARAOKE_KEY, if on { KARAOKE_ON } else { "off" }),
            "karaoke choice",
            "the karaoke choice",
        );
    };
    // Karaoke follows the words, so it is offered only for synced lyrics
    // that are on screen.
    let can_karaoke = show_lyrics() && lyrics.as_ref().is_some_and(|l| l.synced);
    let mut show_queue = use_signal(|| {
        platform::stored_theme(QUEUE_KEY).ok().flatten().as_deref() == Some(QUEUE_SHOWN)
    });
    let mut toggle_queue = move || {
        let show = !show_queue();
        show_queue.set(show);
        platform::info(&format!("pharos: queue panel shown={show}"));
        note_stored(
            not_stored,
            platform::store_theme(QUEUE_KEY, if show { QUEUE_SHOWN } else { "hidden" }),
            "queue choice",
            "whether the queue shows",
        );
    };
    let queued = queue.len();
    // The player's keys, and the queue's while the focus is in it (spec 029).
    super::keys::use_key_mode(Mode::Music);
    let choose = move |next: Style| {
        style.set(next);
        fallback.set(None);
        note_stored(
            not_stored,
            platform::store_theme(STYLE_KEY, next.label()),
            "visualiser style",
            "the visualiser style",
        );
    };
    // Where a drag on the seek bar is, so playback seeks once on release
    // rather than on every step and the thumb stays under the pointer.
    let mut scrub = use_signal(|| None::<Scrub>);
    let current = style();
    let note = fallback().map(|f| match f {
        Fallback::ReducedMotion => "The visualiser is still because reduced motion is on.",
        _ => "This device cannot draw the visualiser.",
    });
    let total = total_ms(&media, runtime_ms);
    // A released scrub whose seek has landed is over, even if the event that
    // clears it is missed.
    let dragging = scrub().filter(|s| !s.landed(media.position_ms));
    let at = dragging.map_or(media.position_ms, |s| s.ms);
    let at = if total > 0 { at.min(total) } else { at };
    let played = if total > 0 {
        at as f64 / total as f64 * 100.0
    } else {
        0.0
    };
    let meta = meta_line(&artists, album.as_deref(), track, year, &genres);
    let on_key = move |evt: KeyboardEvent| {
        // Ctrl+K opens the palette; it must not also toggle play.
        if not_transport(&evt) {
            return;
        }
        // Shift+P / Shift+N are page-wide (B562), not the player's own.
        if let Some(control) = transport_key(&evt.key(), &media) {
            evt.prevent_default();
            on_control.call(control);
        }
    };
    rsx! {
        section {
            class: "pharos-listen",
            tabindex: 0,
            onkeydown: on_key,
            onwheel: move |evt: WheelEvent| shortcuts.wheel(&evt),
            onclick: move |evt: MouseEvent| {
                shortcuts.click(&evt);
            },
            oncontextmenu: move |evt: MouseEvent| shortcuts.context_menu(&evt),
            onmousedown: move |evt: MouseEvent| shortcuts.mouse_down(&evt),
            "data-playing": if media.paused { "false" } else { "true" },
            if let (false, Some(program), None) = (compact, current.program(), fallback()) {
                Visualiser {
                    key: "{current.label()}",
                    media_id,
                    style: current,
                    program,
                    tap: tap.clone(),
                    on_fallback: move |f| fallback.set(Some(f)),
                }
            }
            div { class: "pharos-listen-top",
                div { class: "pharos-listen-nav",
                    button {
                        class: "pharos-listen-skip",
                        r#type: "button",
                        aria_label: "Back",
                        onclick: move |_| on_control.call(Control::Back),
                        Icon { path: ICON_LEAVE }
                    }
                    button {
                        class: "pharos-listen-skip pharos-shrink",
                        r#type: "button",
                        aria_label: "Shrink",
                        "data-key-action": "shrink",
                        title: "Shrink to a small window",
                        onclick: move |_| on_control.call(Control::Shrink),
                        Icon { path: ICON_FLOAT }
                    }
                }
                StylePicker { current, on_choose: choose }
                div { class: "pharos-listen-panels", role: "group", aria_label: "Panels",
                    // Always drawn, so the listener can see where lyrics
                    // live even on a song that has none (B437/V377). Its
                    // own words say there are none, since a tooltip never
                    // shows on a touch screen (B362/V302).
                    button {
                        class: "pharos-chip pharos-lyrics-toggle",
                        "data-key-action": "lyrics",
                        r#type: "button",
                        disabled: lyrics.is_none(),
                        title: lyrics_title(lyrics.is_some(), lyrics_loading, lyrics_failed),
                        aria_pressed: if lyrics.is_some() && show_lyrics() { "true" } else { "false" },
                        // Only while the panel is drawn (B625/V559): not in the mini bar.
                        aria_controls: (lyrics.is_some() && show_lyrics() && !compact).then_some(super::lyrics::PANEL_ID),
                        onclick: move |_| toggle_lyrics(),
                        {lyrics_label(lyrics.is_some(), lyrics_loading, lyrics_failed)}
                    }
                    if can_karaoke {
                        button {
                            class: "pharos-chip pharos-karaoke-toggle",
                            r#type: "button",
                            title: "Fill in each line as it is sung",
                            aria_pressed: if karaoke() { "true" } else { "false" },
                            onclick: move |_| toggle_karaoke(),
                            "Karaoke"
                        }
                    }
                    button {
                        class: "pharos-chip pharos-queue-toggle",
                        "data-key-action": "queue",
                        r#type: "button",
                        title: "Show or hide what plays next",
                        aria_pressed: if show_queue() { "true" } else { "false" },
                        aria_controls: (show_queue() && !compact && in_sight).then_some(QUEUE_PANEL_ID),
                        onclick: move |_| toggle_queue(),
                        Icon { path: ICON_QUEUE_NEXT }
                        "Queue"
                        if queued > 0 {
                            span { class: "pharos-queue-count", aria_label: "{queued} songs", "{queued}" }
                        }
                    }
                }
                if let Some(note) = note {
                    p { class: "pharos-muted pharos-listen-note", "{note}" }
                }
                if let Some(notice) = not_stored() {
                    NoticeLine { notice }
                }
            }
            // Not drawn while the screen is out of sight (B625/V559): a hidden
            // panel cannot scroll, so one kept from the mini bar opened at
            // the top with the sung line out of view until the next line.
            // Drawn fresh, it centres the line being sung as it appears.
            if let (Some(lyrics), true, false) = (lyrics.clone(), show_lyrics(), compact) {
                LyricsPanel {
                    lyrics,
                    position_ms: media.position_ms,
                    on_seek: move |ms| on_control.call(Control::Seek(ms)),
                    karaoke: karaoke(),
                }
            }
            // Not while the screen is out of sight: the compact player docks
            // its own (B900/V852), and two panels would share element ids.
            if show_queue() && !compact && in_sight {
                QueuePanel { queue: queue.clone(), shuffled, on_queue, playing }
            }
            div { class: "pharos-listen-body",
                div { class: "pharos-listen-cover",
                    div { class: "pharos-listen-disc", aria_hidden: "true" }
                    div { class: "pharos-listen-art",
                        match &image {
                            Some(src) => rsx! { img { src: platform::art_src(src), alt: "" } },
                            None => rsx! { div { class: "pharos-poster-placeholder", aria_hidden: "true", "♪" } },
                        }
                    }
                }
                div { class: "pharos-listen-main",
                    p { class: "pharos-listen-eyebrow", "Now playing" }
                    h2 { class: "pharos-listen-title", title: "{title}", "{title}" }
                    if let Some(meta) = meta {
                        p { class: "pharos-listen-meta", title: "{meta}", "{meta}" }
                    }
                    audio {
                        id: media_id,
                        class: "pharos-audio",
                        autoplay: true,
                        onplaying: {
                            let tap = tap.clone();
                            move |_| {
                                platform::wake_media_tap(media_id);
                                tap.connect(media_id);
                                on_media.call(MediaEvent::Playing);
                            }
                        },
                        onpause: move |_| on_media.call(MediaEvent::Pause),
                        onseeked: move |_| {
                            scrub.set(None);
                            on_media.call(MediaEvent::Seeked);
                        },
                        ontimeupdate: move |_| on_media.call(MediaEvent::TimeUpdate),
                        onended: move |_| on_media.call(MediaEvent::Ended),
                        onvolumechange: move |_| on_media.call(MediaEvent::Changed),
                        ondurationchange: move |_| on_media.call(MediaEvent::Changed),
                        onloadedmetadata: move |_| on_media.call(MediaEvent::Changed),
                    }
                    div { class: "pharos-listen-seek",
                        input {
                            class: "pharos-listen-range",
                            r#type: "range",
                            aria_label: "Seek",
                            min: 0,
                            max: "{total}",
                            // The end is reachable whatever the length.
                            step: "any",
                            value: "{at}",
                            aria_valuetext: "{clock(at)}",
                            disabled: total == 0,
                            style: "--pharos-fill: {played:.2}%",
                            onkeydown: move |evt| slider_key(evt, None),
                            oninput: move |evt: FormEvent| {
                                if let Ok(ms) = evt.value().parse::<f64>() {
                                    scrub.set(Some(Scrub { ms: ms as u64, released: false }));
                                }
                            },
                            // The scrub stays until the seek lands, so the
                            // thumb does not snap back and then jump.
                            onchange: move |evt: FormEvent| {
                                if let Ok(ms) = evt.value().parse::<f64>() {
                                    let ms = ms as u64;
                                    let asked = Scrub { ms, released: true };
                                    scrub.set(Some(asked));
                                    on_control.call(Control::Seek(ms));
                                    spawn(async move {
                                        platform::sleep_ms(SEEK_GIVE_UP_MS).await;
                                        if *scrub.peek() == Some(asked) {
                                            scrub.set(None);
                                        }
                                    });
                                }
                            },
                            // A drag that ends where it began fires no
                            // change; wait a tick for one before giving up.
                            onpointerup: move |_| {
                                spawn(async move {
                                    platform::sleep_ms(0).await;
                                    if scrub.peek().is_some_and(|s| !s.released) {
                                        scrub.set(None);
                                    }
                                });
                            },
                            onblur: move |_| scrub.set(None),
                        }
                        div { class: "pharos-listen-times",
                            span { "{clock(at)}" }
                            span { "{clock(total)}" }
                        }
                    }
                    div { class: "pharos-listen-transport",
                        // Always there: with nothing before, it starts the
                        // song again (B464/V406).
                        button {
                            class: "pharos-listen-skip",
                            r#type: "button",
                            aria_label: "Previous track",
                            title: "Previous track",
                            onclick: move |_| on_control.call(Control::Previous),
                            Icon { path: ICON_PREVIOUS }
                        }
                        button {
                            class: "pharos-listen-skip",
                            r#type: "button",
                            aria_label: "Back 10 seconds",
                            onclick: move |_| on_control.call(Control::Seek(media.position_ms.saturating_sub(SEEK_STEP_MS))),
                            Icon { path: ICON_BACK }
                        }
                        button {
                            class: "pharos-listen-play",
                            r#type: "button",
                            aria_label: if media.paused { "Play" } else { "Pause" },
                            onclick: move |_| on_control.call(Control::TogglePlay),
                            Icon { path: if media.paused { ICON_PLAY } else { ICON_PAUSE } }
                        }
                        button {
                            class: "pharos-listen-skip",
                            r#type: "button",
                            aria_label: "Forward 10 seconds",
                            onclick: move |_| on_control.call(Control::Seek(media.position_ms + SEEK_STEP_MS)),
                            Icon { path: ICON_FORWARD }
                        }
                        button {
                            class: "pharos-listen-skip",
                            r#type: "button",
                            aria_label: "Next track",
                            title: "Next track",
                            disabled: next.is_none(),
                            onclick: move |_| {
                                if let Some(id) = next {
                                    on_control.call(Control::Play(id));
                                }
                            },
                            Icon { path: ICON_NEXT }
                        }
                    }
                    div { class: "pharos-listen-volume",
                        Volume {
                            media,
                            on_control,
                            button_class: "pharos-listen-skip",
                            range_class: "pharos-listen-range",
                            icon: rsx! { Icon { path: if media.muted || media.volume == 0.0 { ICON_MUTED } else { ICON_VOLUME } } },
                            filled: true,
                        }
                    }
                }
            }
        }
    }
}

/// The confirm's Cancel, focused when it opens so Enter cannot clear by
/// accident.
const CLEAR_CANCEL_ID: &str = "pharos-queue-clear-cancel";
/// The drawer's Clear, which gets focus back when the confirm is cancelled.
const CLEAR_ID: &str = "pharos-queue-clear";
/// Where focus lands once a confirmed Clear has removed the Clear button.
const SHUFFLE_ID: &str = "pharos-queue-shuffle";
const CLEAR_NOTE_ID: &str = "pharos-queue-clear-note";

/// What plays next, over the visualiser's edge (B435/V375): pick a song to
/// skip to it, drag a row (or use its arrows) to move it, shuffle, remove
/// or clear.
#[component]
pub(super) fn QueuePanel(
    queue: Vec<Entry>,
    shuffled: bool,
    on_queue: EventHandler<QueueAction>,
    #[props(default)] playing: Option<ItemRef>,
    /// The video queue's panel (V853): it counts videos and has no mix.
    #[props(default)]
    videos: bool,
    /// Escape outside the Clear confirm, for a panel that closes on it (the
    /// video player's). The panel stops the key going further up, so its
    /// holder cannot hear it itself.
    #[props(default)]
    on_escape: Option<EventHandler<()>>,
) -> Element {
    // The row being dragged, by key, and the position it would drop at.
    let mut dragging = use_signal(|| None::<u64>);
    let mut over = use_signal(|| None::<usize>);
    let total = queue.len();
    let noun = if videos { "video" } else { "song" };
    let count = match total {
        0 => String::new(),
        1 => format!("1 {noun}"),
        n => format!("{n} {noun}s"),
    };
    let mut end_drag = move || {
        dragging.set(None);
        over.set(None);
    };
    // Clear asks first (spec 020 US3); an emptied queue has nothing to ask.
    let mut confirming = use_signal(|| false);
    // A queue that drains while the confirm is open has nothing left to
    // ask about, and must not ask again once a song is added.
    use_effect(use_reactive!(|(total,)| {
        if total == 0 && *confirming.peek() {
            confirming.set(false);
        }
    }));
    let asking = confirming() && total > 0;
    // Focus follows the confirm: to Cancel when it opens, back to Clear when
    // it is cancelled, and to Shuffle once a confirmed Clear has removed
    // both the dialog and the Clear button.
    let mut returning = use_signal(|| None::<&'static str>);
    use_effect(move || {
        if confirming() {
            platform::focus_by_id(CLEAR_CANCEL_ID);
        } else if let Some(id) = returning() {
            returning.set(None);
            platform::focus_by_id(id);
        }
    });
    let mut cancel = move || {
        returning.set(Some(CLEAR_ID));
        confirming.set(false);
        platform::info(&format!("pharos: queue clear cancelled upcoming={total}"));
    };
    // Escape cancels wherever focus has gone; the confirm does not hold it.
    let mut escape = use_signal(|| None::<platform::EscapeWatch>);
    use_effect(move || {
        let watch = confirming().then(|| {
            platform::on_escape(move || {
                if *confirming.peek() {
                    cancel();
                }
            })
        });
        escape.set(watch);
    });
    rsx! {
        aside {
            id: QUEUE_PANEL_ID,
            class: "pharos-queue",
            aria_label: "Queue",
            "data-keys": "list",
            "data-keys-list": "true",
            "data-key-area": "queue",
            // The screen's wheel moves the volume; here it scrolls the list.
            onwheel: move |evt: WheelEvent| evt.stop_propagation(),
            // Keys on the queue's own buttons are theirs, not the player's.
            onkeydown: move |evt: KeyboardEvent| {
                let key = evt.key();
                let own = matches!(key, Key::Named(NamedKey::Enter) | Key::Named(NamedKey::ArrowUp) | Key::Named(NamedKey::ArrowDown) | Key::Named(NamedKey::PageUp) | Key::Named(NamedKey::PageDown))
                    || matches!(&key, Key::Character(k) if k == " ")
                    // The queue's own keys (spec 029), not the player's k and h/l.
                    || !matches!(keymap::resolve(Mode::Queue, &[&key.to_string()]), Resolved::Unbound);
                if own {
                    evt.stop_propagation();
                }
                if key == Key::Named(NamedKey::Escape) && *confirming.peek() {
                    evt.prevent_default();
                    evt.stop_propagation();
                    cancel();
                } else if key == Key::Named(NamedKey::Escape) && !platform::escape_claimed() {
                    // The key help and fullscreen take Escape first.
                    if let Some(on_escape) = on_escape {
                        evt.prevent_default();
                        on_escape.call(());
                    }
                }
            },
            div { class: "pharos-queue-head",
                h2 { class: "pharos-queue-title", "Up next" }
                span { class: "pharos-muted pharos-queue-total", "{count}" }
                button {
                    id: SHUFFLE_ID,
                    "data-key-action": "shuffle",
                    class: "pharos-chip pharos-queue-shuffle",
                    r#type: "button",
                    aria_pressed: if shuffled { "true" } else { "false" },
                    onclick: move |_| on_queue.call(QueueAction::Shuffle(!shuffled)),
                    Icon { path: ICON_SHUFFLE }
                    "Shuffle"
                }
                if total > 0 && !confirming() {
                    button {
                        id: CLEAR_ID,
                        "data-key-action": "clear",
                        class: "pharos-chip",
                        r#type: "button",
                        aria_haspopup: "dialog",
                        onclick: move |_| confirming.set(true),
                        "Clear"
                    }
                }
            }
            if !videos {
                MixBar { playing }
            }
            if asking {
                div {
                    class: "pharos-queue-confirm",
                    role: "alertdialog",
                    aria_label: "Clear the queue?",
                    aria_describedby: CLEAR_NOTE_ID,
                    p { id: CLEAR_NOTE_ID, "Clear the queue? This removes {count}." }
                    div { class: "pharos-queue-confirm-actions",
                        button {
                            id: CLEAR_CANCEL_ID,
                            class: "pharos-chip",
                            r#type: "button",
                            onclick: move |_| cancel(),
                            "Cancel"
                        }
                        button {
                            class: "pharos-chip pharos-queue-confirm-clear",
                            r#type: "button",
                            onclick: move |_| {
                                returning.set(Some(SHUFFLE_ID));
                                confirming.set(false);
                                on_queue.call(QueueAction::Clear);
                            },
                            "Clear queue"
                        }
                    }
                }
            }
            if queue.is_empty() {
                p { class: "pharos-muted pharos-queue-empty",
                    if videos {
                        "Nothing queued. Use Play next or Add to queue on any film or episode."
                    } else {
                        "Nothing queued. Use Play next or Add to queue on any song or album."
                    }
                }
            }
            ol { class: "pharos-queue-list",
                for (at, entry) in queue.into_iter().enumerate() {
                    QueueRow {
                        key: "{entry.key}",
                        is_dragged: dragging() == Some(entry.key),
                        entry,
                        at,
                        total,
                        drop_here: over() == Some(at) && dragging().is_some(),
                        on_queue,
                        on_drag: move |key| dragging.set(Some(key)),
                        on_over: move |at| over.set(Some(at)),
                        on_drop: move |at: usize| {
                            if let Some(key) = *dragging.peek() {
                                platform::info(&format!("pharos: queue drag to={at}"));
                                on_queue.call(QueueAction::Move { key, to: at });
                            }
                            end_drag();
                        },
                        on_end: move |()| end_drag(),
                    }
                }
            }
        }
    }
}

#[component]
fn QueueRow(
    entry: Entry,
    at: usize,
    total: usize,
    is_dragged: bool,
    drop_here: bool,
    on_queue: EventHandler<QueueAction>,
    on_drag: EventHandler<u64>,
    on_over: EventHandler<usize>,
    on_drop: EventHandler<usize>,
    on_end: EventHandler<()>,
) -> Element {
    let key = entry.key;
    let name = entry.name.clone();
    let length = track_length(entry.runtime_ms);
    // Why a mix picked it, as the server said (spec 024).
    let mix = try_use_context::<MixCtx>();
    let why = (entry.source == Source::Mix)
        .then(|| {
            mix.and_then(|m| {
                m.0.read()
                    .as_ref()
                    .and_then(|m| m.reasons.get(&entry.id).cloned())
            })
        })
        .flatten()
        .filter(|r| !r.is_empty())
        .map(|r| r.join(" · "));
    rsx! {
        li {
            class: "pharos-queue-entry",
            draggable: "true",
            "data-key-row": "true",
            "data-source": entry.source.label(),
            "data-key": "{key}",
            "data-drop": if drop_here && !is_dragged { "true" } else { "false" },
            "data-dragging": if is_dragged { "true" } else { "false" },
            ondragstart: move |evt: DragEvent| {
                // Firefox starts a drag only once it carries some data.
                if let Err(e) = evt.data_transfer().set_data("text/plain", &key.to_string()) {
                    platform::warn(&format!("pharos: queue drag data not set error={e}"));
                }
                on_drag.call(key);
            },
            ondragover: move |evt: DragEvent| {
                evt.prevent_default();
                on_over.call(at);
            },
            ondrop: move |evt: DragEvent| {
                evt.prevent_default();
                on_drop.call(at);
            },
            ondragend: move |_| on_end.call(()),
            span { class: "pharos-queue-grip", aria_hidden: "true", "⠿" }
            button {
                class: "pharos-queue-name",
                r#type: "button",
                title: "Play {name} now",
                onclick: move |_| on_queue.call(QueueAction::Play(key)),
                "{name}"
                if let Some(why) = why {
                    span { class: "pharos-queue-why pharos-muted", "{why}" }
                }
            }
            span { class: "pharos-queue-length pharos-muted",
                if let Some(l) = length { "{l}" }
            }
            button {
                class: "pharos-queue-icon",
                r#type: "button",
                aria_label: "Move {name} up",
                "data-key-action": "move-up",
                title: "Move up",
                disabled: at == 0,
                onclick: move |_| on_queue.call(QueueAction::Move { key, to: at.saturating_sub(1) }),
                Icon { path: ICON_UP }
            }
            button {
                class: "pharos-queue-icon",
                r#type: "button",
                aria_label: "Move {name} down",
                "data-key-action": "move-down",
                title: "Move down",
                disabled: at + 1 >= total,
                onclick: move |_| on_queue.call(QueueAction::Move { key, to: at + 1 }),
                Icon { path: ICON_DOWN }
            }
            button {
                class: "pharos-queue-icon",
                r#type: "button",
                aria_label: "Remove {name} from the queue",
                "data-key-action": "remove",
                title: "Remove",
                onclick: move |_| on_queue.call(QueueAction::Remove(key)),
                Icon { path: ICON_REMOVE }
            }
        }
    }
}

/// The Lyrics chip's tooltip: what it does, or why it does nothing.
fn lyrics_title(have: bool, loading: bool, failed: bool) -> &'static str {
    match (have, loading, failed) {
        (true, _, _) => "Show or hide the lyrics",
        (false, true, _) => "Looking for lyrics",
        (false, false, true) => "The lyrics could not be loaded",
        (false, false, false) => "No lyrics for this song",
    }
}

/// The Lyrics chip's own words, since a touch screen shows no tooltip: a
/// pending lookup says it is looking, a finished one that found none says so,
/// and one that failed says the lyrics are unavailable, not that there are
/// none (B479).
fn lyrics_label(have: bool, loading: bool, failed: bool) -> &'static str {
    match (have, loading, failed) {
        (true, _, _) => "Lyrics",
        (false, true, _) => "Looking for lyrics",
        (false, false, true) => "Lyrics unavailable",
        (false, false, false) => "No lyrics",
    }
}

/// "Massive Attack · Mezzanine · Track 3 · 1998 · Trip-hop, Electronic",
/// leaving out what is not known; `None` when nothing is.
fn meta_line(
    artists: &[String],
    album: Option<&str>,
    track: Option<u32>,
    year: Option<u16>,
    genres: &[String],
) -> Option<String> {
    let parts: Vec<String> = [
        (!artists.is_empty()).then(|| artists.join(", ")),
        album.filter(|a| !a.is_empty()).map(str::to_owned),
        track.filter(|&n| n > 0).map(|n| format!("Track {n}")),
        year.map(|y| y.to_string()),
        (!genres.is_empty()).then(|| genres.join(", ")),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

const ICON_PLAY: &str =
    "M8 5.5v13a1 1 0 0 0 1.5.86l10.5-6.5a1 1 0 0 0 0-1.72L9.5 4.64A1 1 0 0 0 8 5.5Z";
const ICON_PAUSE: &str = "M7 5h3.5v14H7zM13.5 5H17v14h-3.5z";
const ICON_BACK: &str = "M11 7V4L5 9l6 5v-3a5 5 0 1 1-5 5H4a7 7 0 1 0 7-7Z";
const ICON_FORWARD: &str = "M13 7V4l6 5-6 5v-3a5 5 0 1 0 5 5h2a7 7 0 1 1-7-7Z";
/// A small window in a corner of a larger one: shrink the player.
pub const ICON_FLOAT: &str = "M3 5h18v14H3V5Zm2 2v10h14V7H5Zm7 5h6v4h-6v-4Z";
pub const ICON_PIP: &str = "M19 11h-8v6h8v-6Zm4 8V5a2 2 0 0 0-2-2H3a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h18a2 2 0 0 0 2-2Zm-2 0H3V5h18v14Z";
const ICON_PREVIOUS: &str = "M6 6h2v12H6zm3.5 6 8.5 6V6z";
const ICON_NEXT: &str = "M6 18l8.5-6L6 6v12zM16 6v12h2V6h-2z";
pub(super) const ICON_LEAVE: &str = "M20 11H7.8l5.6-5.6L12 4l-8 8 8 8 1.4-1.4L7.8 13H20Z";
pub(super) const ICON_VOLUME: &str = "M4 9v6h4l5 4V5L8 9H4Zm12.5 3a4.5 4.5 0 0 0-2.5-4v8a4.5 4.5 0 0 0 2.5-4ZM14 3.2v2.1a7 7 0 0 1 0 13.4v2.1a9 9 0 0 0 0-17.6Z";
/// The queue's icons (B435/V375).
pub const ICON_QUEUE_NEXT: &str = "M19 9H2v2h17V9Zm0-4H2v2h17V5ZM2 15h13v-2H2v2Zm15-2v6l5-3-5-3Z";
pub const ICON_QUEUE_ADD: &str =
    "M14 10H3v2h11v-2Zm0-4H3v2h11V6Zm4 8v-4h-2v4h-4v2h4v4h2v-4h4v-2h-4ZM3 16h7v-2H3v2Z";
const ICON_SHUFFLE: &str = "M10.6 9.2 5.4 4 4 5.4l5.2 5.2 1.4-1.4ZM14.5 4l2 2L4 18.6 5.4 20 18 7.5l2 2V4h-5.5Zm.3 9.4-1.4 1.4 3.1 3.1-2 2.1H20v-5.5l-2 2-3.2-3.1Z";
const ICON_REMOVE: &str = "M19 6.4 17.6 5 12 10.6 6.4 5 5 6.4 10.6 12 5 17.6 6.4 19 12 13.4 17.6 19 19 17.6 13.4 12 19 6.4Z";
const ICON_UP: &str = "m12 8-6 6 1.4 1.4 4.6-4.6 4.6 4.6L18 14l-6-6Z";
const ICON_DOWN: &str = "M16.6 8.6 12 13.2 7.4 8.6 6 10l6 6 6-6-1.4-1.4Z";
pub(super) const ICON_MUTED: &str = "M4 9v6h4l5 4V5L8 9H4Zm12.6 3 2.7-2.7-1.4-1.4-2.7 2.7-2.7-2.7-1.4 1.4 2.7 2.7-2.7 2.7 1.4 1.4 2.7-2.7 2.7 2.7 1.4-1.4L16.6 12Z";

#[component]
pub fn Icon(path: &'static str) -> Element {
    rsx! {
        svg {
            class: "pharos-icon",
            view_box: "0 0 24 24",
            "aria-hidden": "true",
            path { d: path, fill: "currentColor" }
        }
    }
}

/// The canvas and its frame loop; remounted per style, so each style gets a
/// fresh surface and a dropped one stops drawing.
#[component]
fn Visualiser(
    style: Style,
    program: render::Program,
    tap: Tap,
    /// The `<audio>` element the tap reads.
    media_id: &'static str,
    on_fallback: EventHandler<Fallback>,
) -> Element {
    // On the TV the page draws: this crate, built to wasm, runs the same
    // loop there (B1032). Dropped with this component, it stops.
    #[cfg(all(feature = "mobile", target_os = "android"))]
    {
        let _ = (program, &tap);
        use_hook(move || {
            Rc::new(platform::page_visualiser(style.label(), move |reason| {
                on_fallback.call(Fallback::from_label(&reason));
            }))
        });
    }
    #[cfg(not(all(feature = "mobile", target_os = "android")))]
    use_effect(move || {
        let tap = tap.clone();
        spawn(async move {
            let opened = render::open(
                CANVAS_ID,
                program,
                platform::prefers(PREFERS_REDUCED_MOTION),
            );
            let outcome = match opened {
                Ok(surface) => {
                    report(style, None);
                    draw(surface, &tap, media_id).await
                }
                Err(f) => Err(f),
            };
            if let Err(f) = outcome {
                report(style, Some(&f));
                on_fallback.call(f);
            }
        });
    });
    rsx! {
        canvas { id: CANVAS_ID, class: "pharos-visualiser", aria_hidden: "true" }
    }
}

/// The visualiser for `style_label` on this page's canvas, drawn until it
/// fails: what the TV app's page runs, this crate built to wasm, while the
/// native app shows the music screen (B1032, V992). `Ok` when the style
/// draws nothing.
#[cfg(all(feature = "web", target_arch = "wasm32"))]
pub async fn run_page_visualiser(style_label: &str) -> Result<(), Fallback> {
    let style = Style::parse(Some(style_label));
    let Some(program) = style.program() else {
        return Ok(());
    };
    let outcome = match render::open(
        CANVAS_ID,
        program,
        platform::prefers(PREFERS_REDUCED_MOTION),
    ) {
        Ok(surface) => {
            report(style, None);
            draw(surface, &Tap::default(), VIDEO_ID).await
        }
        Err(f) => Err(f),
    };
    if let Err(f) = &outcome {
        report(style, Some(f));
    }
    outcome
}

/// Log the renderer decision and send it as client events (016
/// `renderer_mode`, and `webgl_context_lost` when that is the reason).
#[cfg_attr(all(feature = "mobile", target_os = "android"), allow(dead_code))]
fn report(style: Style, fallback: Option<&Fallback>) {
    let (line, events) = renderer_signals(style, fallback);
    platform::info(&line);
    for event in events {
        platform::report_event(event);
    }
}

#[cfg_attr(all(feature = "mobile", target_os = "android"), allow(dead_code))]
fn renderer_signals(style: Style, fallback: Option<&Fallback>) -> (String, Vec<ClientEvent>) {
    let (mode, reason) = match fallback {
        None => ("webgl2", "none"),
        Some(f) => ("fallback", f.label()),
    };
    // A GLSL info log spans lines; keep the decision on one log line.
    let detail = match fallback {
        Some(Fallback::Shader(log)) => log.split_whitespace().collect::<Vec<_>>().join(" "),
        _ => String::new(),
    };
    let line = format!(
        "pharos: renderer mode={mode} reason={reason} style={} detail={detail}",
        style.label()
    );
    let mut events = vec![ClientEvent {
        kind: ClientEventKind::RendererMode,
        level: ClientLevel::Info,
        route: String::new(),
        fields: vec![
            ("mode", mode.to_owned()),
            ("reason", reason.to_owned()),
            ("style", style.label().to_owned()),
        ],
    }];
    if fallback == Some(&Fallback::ContextLost) {
        events.push(ClientEvent {
            kind: ClientEventKind::WebglContextLost,
            level: ClientLevel::Warn,
            route: String::new(),
            fields: vec![("phase", "lost".to_owned())],
        });
    }
    (line, events)
}

/// How long a paused visualiser waits between looks for the song playing
/// again: a few looks a second rather than every frame (FR-706).
const IDLE_CHECK_MS: u32 = 200;

/// Whether the visualiser has nothing to draw: a frame is up, the song is
/// paused and the surface needs no frame of its own.
fn idle(drawn: bool, playing: bool, needs_frame: bool) -> bool {
    drawn && !playing && !needs_frame
}

/// Draw every animation frame until the component unmounts (which drops
/// this future) or the surface fails. A paused song keeps its last frame,
/// so a headset spends no GPU time on a still picture, and the loop looks
/// for play again every [`IDLE_CHECK_MS`] rather than every frame.
#[cfg_attr(all(feature = "mobile", target_os = "android"), allow(dead_code))]
async fn draw(
    mut surface: Box<dyn Surface>,
    tap: &Tap,
    media_id: &'static str,
) -> Result<(), Fallback> {
    let start = platform::now_ms();
    let mut palette = platform::theme_palette();
    let mut palette_at = start;
    let mut analysis: Option<Analysis> = None;
    let mut drawn = false;
    loop {
        platform::animation_frame().await;
        if idle(
            drawn,
            platform::media_playing(media_id),
            surface.needs_frame(),
        ) {
            platform::sleep_ms(IDLE_CHECK_MS).await;
            continue;
        }
        let now = platform::now_ms();
        if now - palette_at >= PALETTE_REFRESH_MS {
            palette = platform::theme_palette();
            palette_at = now;
        }
        tap.connect(media_id);
        let mut slot = tap.tap.borrow_mut();
        let analysis = slot.as_mut().map(|t| {
            let a = analysis.get_or_insert_with(|| Analysis::new(t.bins(), t.sample_rate()));
            a.read(t.as_mut());
            &*a
        });
        surface.draw(&Frame {
            time_s: Frame::clock(now, start),
            palette,
            level: analysis.map_or(0.0, Analysis::level),
            data: analysis.map(Analysis::data),
        })?;
        drawn = true;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::native::queue::Source;

    /// FR-706: only a paused song with a frame up and a settled surface
    /// waits; anything else draws on the next frame.
    #[test]
    fn a_paused_visualiser_waits_once_its_frame_is_up() {
        assert!(idle(true, false, false));
        assert!(!idle(false, false, false), "the first frame is drawn");
        assert!(!idle(true, true, false), "a playing song draws");
        assert!(!idle(true, false, true), "a recovering surface draws");
    }

    fn render(title: &str, image: Option<&str>) -> String {
        render_with(title, image, MediaState::default())
    }

    fn render_with(title: &str, image: Option<&str>, media: MediaState) -> String {
        render_full(title, image, media, None)
    }

    fn render_full(
        title: &str,
        image: Option<&str>,
        media: MediaState,
        lyrics: Option<Lyrics>,
    ) -> String {
        render_sized(title, image, media, lyrics, false)
    }

    fn render_sized(
        title: &str,
        image: Option<&str>,
        media: MediaState,
        lyrics: Option<Lyrics>,
        compact: bool,
    ) -> String {
        let (title, image) = (title.to_owned(), image.map(str::to_owned));
        let mut dom = VirtualDom::new_with_props(
            move |()| {
                rsx! {
                    ListenView {
                        title: title.clone(),
                        image: image.clone(),
                        artists: vec!["Massive Attack".to_owned()],
                        album: Some("Mezzanine".to_owned()),
                        track: Some(3),
                        year: Some(1998),
                        genres: vec!["Trip-hop".to_owned()],
                        runtime_ms: 330_000,
                        media,
                        lyrics: lyrics.clone(),
                        on_media: |_| {},
                        on_control: |_| {},
                        compact,
                    }
                }
            },
            (),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// A choice stored after one that was not clears the notice, so it
    /// tells of the latest write.
    #[test]
    fn a_later_stored_choice_clears_the_notice() {
        let mut dom = VirtualDom::new(|| {
            let note = use_signal(|| None::<Notice>);
            use_hook(|| {
                note_stored(
                    note,
                    Err("QuotaExceededError".into()),
                    "lyrics choice",
                    "whether lyrics show",
                );
                assert!(note.peek().is_some());
                note_stored(note, Ok(()), "lyrics choice", "whether lyrics show");
                assert!(note.peek().is_none());
            });
            rsx! {}
        });
        dom.rebuild_in_place();
    }

    /// FR-705: a choice the device could not store shows the shared notice
    /// on the listen view, as Appearance shows it. Disarm check: dropping
    /// the `NoticeLine` from the view fails the first assertion.
    #[test]
    fn a_choice_not_stored_shows_the_shared_notice() {
        let mut dom = VirtualDom::new(|| {
            rsx! {
                ListenView {
                    title: "Song".to_owned(),
                    image: None,
                    artists: Vec::new(),
                    genres: Vec::new(),
                    runtime_ms: 1_000,
                    media: MediaState::default(),
                    on_media: |_| {},
                    on_control: |_| {},
                    not_stored: Some(not_saved("the visualiser style", "QuotaExceededError", Lasts::ThisSong)),
                }
            }
        });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(
            html.contains(
                "Could not save the visualiser style on this device: QuotaExceededError."
            ),
            "{html}"
        );
        assert!(html.contains("role=\"alert\""), "{html}");
        let plain = render("Song", None);
        assert!(!plain.contains("Could not save"), "{plain}");
    }

    #[test]
    fn the_lyrics_panel_is_drawn_only_while_the_screen_is_in_sight() {
        use pharos_client::api::LyricLine;
        // B625/V559: a panel kept hidden behind the mini bar cannot scroll to
        // the sung line, so it is drawn when the full player opens instead.
        let words = Lyrics {
            synced: true,
            lines: vec![LyricLine {
                start_ms: 1_000,
                text: "Love, love is a verb".to_owned(),
                words: Vec::new(),
            }],
            words_pending: false,
        };
        let mini = render_sized(
            "Teardrop",
            None,
            MediaState::default(),
            Some(words.clone()),
            true,
        );
        assert!(!mini.contains(r#"id="pharos-lyrics""#), "{mini}");
        // The chip points at no panel that is not drawn.
        assert!(!mini.contains(r#"aria-controls="pharos-lyrics""#), "{mini}");
        let full = render_sized("Teardrop", None, MediaState::default(), Some(words), false);
        assert!(full.contains(r#"id="pharos-lyrics""#), "{full}");
    }

    #[test]
    fn the_song_has_its_own_controls_not_the_browsers() {
        let html = render("Teardrop", None);
        let audio = html
            .split_once("<audio")
            .and_then(|(_, rest)| rest.split_once('>'))
            .map(|(tag, _)| tag)
            .expect("an audio element");
        assert!(
            !audio.split_whitespace().any(|a| a.starts_with("controls")),
            "{audio}"
        );
        for label in ["Seek", "Volume", "Back 10 seconds", "Forward 10 seconds"] {
            assert!(
                html.contains(&format!(r#"aria-label="{label}""#)),
                "{label}"
            );
        }
        assert!(
            html.contains("Massive Attack · Mezzanine · Track 3 · 1998 · Trip-hop"),
            "{html}"
        );
    }

    fn render_next(next: Option<ItemRef>) -> String {
        let mut dom = VirtualDom::new_with_props(
            move |next: Option<ItemRef>| {
                rsx! {
                    ListenView {
                        title: "Teardrop".to_owned(),
                        image: None,
                        year: None,
                        genres: Vec::new(),
                        runtime_ms: 330_000,
                        media: MediaState::default(),
                        next,
                        lyrics: None,
                        on_media: |_| {},
                        on_control: |_| {},
                    }
                }
            },
            next,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// The `<button …>` tag whose `aria-label` is `label`.
    fn button<'a>(html: &'a str, label: &str) -> &'a str {
        let at = html
            .find(&format!(r#"aria-label="{label}""#))
            .unwrap_or_else(|| panic!("no {label} in {html}"));
        let start = html[..at].rfind("<button").expect("a button");
        let end = at + html[at..].find('>').expect("a closed tag");
        &html[start..end]
    }

    #[test]
    fn the_transport_steps_tracks_either_side_of_the_seek_buttons() {
        let html = render_next(Some(ItemRef::Real(7)));
        let order: Vec<usize> = [
            "Previous track",
            "Back 10 seconds",
            "Play",
            "Forward 10 seconds",
            "Next track",
        ]
        .iter()
        .map(|l| html.find(&format!(r#"aria-label="{l}""#)).expect(l))
        .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{html}");
        assert!(!button(&html, "Next track").contains("disabled"), "{html}");
        // Previous can always start the song again.
        assert!(
            !button(&html, "Previous track").contains("disabled"),
            "{html}"
        );

        let last = render_next(None);
        assert!(button(&last, "Next track").contains("disabled"), "{last}");
    }

    #[test]
    fn the_full_window_player_has_a_way_back_and_keeps_the_picker_up_top() {
        let html = render("Teardrop", None);
        let (top, bar) = html.split_once("pharos-listen-body").expect("a bottom bar");
        assert!(top.contains(r#"aria-label="Back""#), "{top}");
        assert!(top.contains(r#"aria-label="Visualiser""#), "{top}");
        // A long title is cut to one line, so the whole of it is a tooltip.
        assert!(bar.contains(r#"title="Teardrop""#), "{bar}");
        assert!(!bar.contains(r#"aria-label="Visualiser""#), "{bar}");
    }

    #[test]
    fn the_play_button_and_clock_follow_the_media() {
        let paused = render("Teardrop", None);
        assert!(paused.contains(r#"aria-label="Play""#), "{paused}");
        assert!(paused.contains(r#"data-playing="false""#), "{paused}");
        // No duration yet: the runtime stands in for the length.
        assert!(paused.contains("5:30"), "{paused}");

        let playing = render_with(
            "Teardrop",
            None,
            MediaState {
                position_ms: 78_000,
                duration_ms: 241_000,
                paused: false,
                ..MediaState::default()
            },
        );
        assert!(playing.contains(r#"aria-label="Pause""#), "{playing}");
        assert!(playing.contains(r#"data-playing="true""#), "{playing}");
        assert!(
            playing.contains("1:18") && playing.contains("4:01"),
            "{playing}"
        );
    }

    #[test]
    fn a_scrub_ends_only_once_its_released_seek_has_landed() {
        let held = Scrub {
            ms: 90_000,
            released: false,
        };
        assert!(!held.landed(90_000));
        let released = Scrub {
            released: true,
            ..held
        };
        assert!(!released.landed(10_000));
        assert!(released.landed(90_400));
    }

    #[test]
    fn the_fill_never_passes_the_end() {
        let html = render_with(
            "Teardrop",
            None,
            MediaState {
                position_ms: 400_000,
                duration_ms: 241_000,
                ..MediaState::default()
            },
        );
        assert!(html.contains("--pharos-fill: 100.00%"), "{html}");
        assert!(html.contains(r#"step="any""#), "{html}");
    }

    #[test]
    fn lyrics_get_a_panel_only_when_there_are_some_but_the_toggle_always_shows() {
        use pharos_client::api::LyricLine;
        // B437/V377: the toggle is always there, greyed out with nothing to show.
        let none = render_full("Teardrop", None, MediaState::default(), None);
        assert!(!none.contains(r#"id="pharos-lyrics""#), "{none}");
        let toggle = lyrics_toggle(&none);
        assert!(toggle.contains("disabled"), "{toggle}");
        assert!(toggle.contains("No lyrics for this song"), "{toggle}");
        // B362/V302: the chip itself says so, since a touch screen shows no
        // tooltip.
        assert!(none.contains(">No lyrics</button>"), "{none}");
        let empty = render_full(
            "Teardrop",
            None,
            MediaState::default(),
            Some(Lyrics::default()),
        );
        assert!(!empty.contains(r#"id="pharos-lyrics""#), "{empty}");
        assert!(lyrics_toggle(&empty).contains("disabled"), "{empty}");

        let words = Lyrics {
            synced: true,
            lines: vec![LyricLine {
                start_ms: 1_000,
                text: "Love, love is a verb".to_owned(),
                words: Vec::new(),
            }],
            words_pending: false,
        };
        let html = render_full("Teardrop", None, MediaState::default(), Some(words));
        assert!(html.contains(r#"aria-label="Lyrics""#), "{html}");
        assert!(html.contains("Love, love is a verb"), "{html}");
        assert!(html.contains(r#"aria-controls="pharos-lyrics""#), "{html}");
        assert!(!lyrics_toggle(&html).contains("disabled"), "{html}");
        assert!(html.contains(">Lyrics</button>"), "{html}");
        // B483/V427: synced lyrics on screen offer karaoke, off to start.
        assert!(html.contains("pharos-karaoke-toggle"), "{html}");
        assert!(html.contains(r#"data-karaoke="false""#), "{html}");
        let plain = Lyrics {
            synced: false,
            lines: vec![LyricLine {
                start_ms: 0,
                text: "Words".to_owned(),
                words: Vec::new(),
            }],
            words_pending: false,
        };
        let html = render_full("Teardrop", None, MediaState::default(), Some(plain));
        assert!(!html.contains("pharos-karaoke-toggle"), "{html}");
        let none = render_full("Teardrop", None, MediaState::default(), None);
        assert!(!none.contains("pharos-karaoke-toggle"), "{none}");
    }

    /// The opening tag of the lyrics toggle.
    #[test]
    fn the_lyrics_chip_says_why_it_does_nothing() {
        assert_eq!(lyrics_title(true, false, false), "Show or hide the lyrics");
        assert_eq!(lyrics_title(false, true, false), "Looking for lyrics");
        assert_eq!(lyrics_title(false, false, false), "No lyrics for this song");
        assert_eq!(
            lyrics_title(false, false, true),
            "The lyrics could not be loaded"
        );
        assert_eq!(lyrics_label(true, false, false), "Lyrics");
        assert_eq!(lyrics_label(false, true, false), "Looking for lyrics");
        assert_eq!(lyrics_label(false, false, false), "No lyrics");
        // B479: a failed lookup never claims the song has none.
        assert_eq!(lyrics_label(false, false, true), "Lyrics unavailable");
    }

    fn lyrics_toggle(html: &str) -> &str {
        html.split_once("pharos-lyrics-toggle")
            .and_then(|(_, rest)| rest.split_once('>'))
            .map(|(tag, _)| tag)
            .expect("a lyrics toggle")
    }

    fn entry(key: u64, name: &str, source: Source) -> Entry {
        Entry {
            key,
            id: ItemRef::Real(key),
            name: name.to_owned(),
            runtime_ms: 187_000,
            source,
        }
    }

    #[test]
    fn the_queue_starts_closed_beside_the_lyrics_toggle_with_its_count() {
        let queue = vec![
            entry(1, "Alpha", Source::Album),
            entry(2, "Inertia Creeps", Source::Added),
        ];
        let mut dom = VirtualDom::new_with_props(
            move |()| {
                rsx! {
                    ListenView {
                        title: "Teardrop".to_owned(),
                        image: None,
                        year: None,
                        genres: Vec::new(),
                        runtime_ms: 330_000,
                        media: MediaState::default(),
                        lyrics: None,
                        queue: queue.clone(),
                        on_media: |_| {},
                        on_control: |_| {},
                    }
                }
            },
            (),
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        // The visualiser keeps the screen until the listener opens the queue.
        assert!(!html.contains(r#"id="pharos-queue""#), "{html}");
        let panels = html
            .split_once(r#"aria-label="Panels""#)
            .map(|(_, rest)| rest)
            .expect("the panel toggles");
        let (lyrics, queue) = (
            panels.find("pharos-lyrics-toggle").expect("lyrics"),
            panels.find("pharos-queue-toggle").expect("queue"),
        );
        assert!(lyrics < queue, "{panels}");
        assert!(html.contains(r#"aria-label="2 songs""#), "{html}");
        assert!(html.contains(r#"aria-pressed="false""#), "{html}");
    }

    fn render_panel(queue: Vec<Entry>, shuffled: bool) -> String {
        let mut dom = VirtualDom::new_with_props(
            move |()| {
                rsx! {
                    QueuePanel { queue: queue.clone(), shuffled, on_queue: |_| {} }
                }
            },
            (),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn the_queue_panel_lists_each_song_with_its_controls() {
        let html = render_panel(
            vec![
                entry(1, "Alpha", Source::Album),
                entry(2, "Inertia Creeps", Source::Added),
            ],
            true,
        );
        assert!(html.contains(r#"id="pharos-queue""#), "{html}");
        assert!(html.contains("2 songs"), "{html}");
        assert!(html.contains(r#"draggable="true""#), "{html}");
        assert!(html.contains(r#"data-source="added""#), "{html}");
        assert!(html.contains("3:07"), "{html}");
        for label in [
            "Move Alpha up",
            "Move Alpha down",
            "Remove Alpha from the queue",
            "Remove Inertia Creeps from the queue",
        ] {
            assert!(
                html.contains(&format!(r#"aria-label="{label}""#)),
                "{label}"
            );
        }
        // The first cannot go up, the last cannot go down.
        let up = html
            .split_once(r#"aria-label="Move Alpha up""#)
            .map(|(_, rest)| rest.split_once('>').map_or("", |(t, _)| t))
            .expect("move up");
        assert!(up.contains("disabled"), "{up}");
        let shuffle = html
            .split_once("pharos-queue-shuffle")
            .and_then(|(_, rest)| rest.split_once('>'))
            .map(|(tag, _)| tag)
            .expect("shuffle");
        assert!(shuffle.contains(r#"aria-pressed="true""#), "{shuffle}");
        assert!(html.contains("Clear"), "{html}");
    }

    #[test]
    fn an_empty_queue_says_how_to_fill_it() {
        let html = render_panel(Vec::new(), false);
        assert!(html.contains("Nothing queued."), "{html}");
        assert!(!html.contains("Clear"), "{html}");
    }

    #[test]
    fn the_meta_line_shows_what_there_is() {
        let none = |year, genres: &[String]| meta_line(&[], None, None, year, genres);
        assert_eq!(none(None, &[]), None);
        assert_eq!(none(Some(2003), &[]).as_deref(), Some("2003"));
        let genres = ["Rock".to_owned(), "Pop".to_owned()];
        assert_eq!(none(None, &genres).as_deref(), Some("Rock, Pop"));
        assert_eq!(
            none(Some(2003), &genres).as_deref(),
            Some("2003 · Rock, Pop")
        );
    }

    #[test]
    fn the_meta_line_names_the_artist_album_and_track_first() {
        let artists = ["Massive Attack".to_owned(), "Liz Fraser".to_owned()];
        let genres = ["Trip-hop".to_owned()];
        assert_eq!(
            meta_line(&artists, Some("Mezzanine"), Some(3), Some(1998), &genres).as_deref(),
            Some("Massive Attack, Liz Fraser · Mezzanine · Track 3 · 1998 · Trip-hop")
        );
        assert_eq!(
            meta_line(&artists[..1], None, Some(3), None, &[]).as_deref(),
            Some("Massive Attack · Track 3")
        );
        // A missing number or an empty album name is left out, not shown blank.
        assert_eq!(
            meta_line(&[], Some(""), Some(0), Some(1998), &[]).as_deref(),
            Some("1998")
        );
    }

    /// The attributes of the first `<tag ...>` in `html`, in whatever order
    /// the renderer writes them (dioxus-ssr 0.8 puts `class` before `id`).
    fn open_tag<'a>(html: &'a str, tag: &str) -> &'a str {
        html.split(&format!("<{tag} "))
            .nth(1)
            .and_then(|rest| rest.split('>').next())
            .unwrap_or("")
    }

    #[test]
    fn a_song_gets_art_title_audio_and_every_style() {
        let html = render("Teardrop", Some("/art.jpg"));
        assert!(html.contains("Teardrop"));
        assert!(html.contains(r#"src="/art.jpg""#));
        assert!(
            open_tag(&html, "audio").contains(&format!(r#"id="{VIDEO_ID}""#)),
            "{html}"
        );
        for style in Style::ALL {
            assert!(html.contains(style.name()), "{}", style.name());
        }
        assert_eq!(html.matches(r#"aria-pressed="true""#).count(), 1);
    }

    #[test]
    fn the_default_style_mounts_a_canvas_behind_the_controls() {
        let html = render("Teardrop", None);
        assert!(
            open_tag(&html, "canvas").contains(&format!(r#"id="{CANVAS_ID}""#)),
            "{html}"
        );
        assert!(html.contains("♪"));
    }

    #[test]
    fn each_renderer_decision_is_logged_and_sent_with_its_reason() {
        let (line, events) = renderer_signals(Style::Halo, None);
        assert_eq!(
            line,
            "pharos: renderer mode=webgl2 reason=none style=halo detail="
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, ClientEventKind::RendererMode);
        assert!(events[0].fields.contains(&("mode", "webgl2".to_owned())));

        let (line, events) = renderer_signals(Style::Scope, Some(&Fallback::ReducedMotion));
        assert!(line.contains("mode=fallback reason=reduced_motion style=scope"));
        assert!(events[0]
            .fields
            .contains(&("reason", "reduced_motion".to_owned())));
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn a_lost_context_also_sends_its_own_warning() {
        let (_, events) = renderer_signals(Style::Plasma, Some(&Fallback::ContextLost));
        let kinds: Vec<_> = events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            [
                ClientEventKind::RendererMode,
                ClientEventKind::WebglContextLost
            ]
        );
        assert_eq!(events[1].level, ClientLevel::Warn);
    }

    #[test]
    fn a_shader_failure_carries_the_compiler_log_on_one_line() {
        let f = Fallback::Shader("compile: ERROR 0:3 'x'\nERROR 0:4 'y'\n".into());
        let (line, _) = renderer_signals(Style::Spectrum, Some(&f));
        assert!(
            line.ends_with(
                "reason=shader style=spectrum detail=compile: ERROR 0:3 'x' ERROR 0:4 'y'"
            ),
            "{line}"
        );
    }

    /// B1360/V1331: the top bar never lays one control over another. The
    /// sides of the picker never shrink below their content, so a narrow
    /// screen moves the picker aside or wraps; an upright phone gives the
    /// toggles their own row, which may wrap again.
    #[test]
    fn the_top_bar_wraps_rather_than_overlapping_on_a_phone() {
        let sheet = include_str!("../../assets/pharos.css");
        let rule = |head: &str| {
            sheet
                .split(head)
                .nth(1)
                .and_then(|r| r.split('}').next())
                .unwrap_or_default()
                .to_owned()
        };
        let top = rule("\n.pharos-listen-top {");
        assert!(top.contains("display: flex;"), "{top}");
        assert!(top.contains("flex-wrap: wrap;"), "{top}");
        for side in ["\n.pharos-listen-nav {", "\n.pharos-listen-panels {"] {
            let r = rule(side);
            assert!(r.contains("min-width: max-content;"), "{side}: {r}");
            assert!(r.contains("flex: 1 1 0;"), "{side}: {r}");
        }
        assert!(rule("\n.pharos-listen-panels {").contains("flex-wrap: wrap;"));
        let phone = sheet
            .split("@media (max-width: 40rem) {")
            .find_map(|block| {
                block
                    .split("\n  .pharos-listen-top .pharos-listen-panels {")
                    .nth(1)
            })
            .and_then(|r| r.split('}').next())
            .unwrap_or_default();
        assert!(phone.contains("flex-basis: 100%;"), "{phone}");
        assert!(phone.contains("min-width: 0;"), "{phone}");
        let panel = sheet
            .split("@media (max-width: 40rem) {")
            .find_map(|block| {
                block
                    .split("\n  .pharos-style-picker .pharos-style-panel {")
                    .nth(1)
            })
            .and_then(|r| r.split('}').next())
            .unwrap_or_default();
        assert!(panel.contains("right: 0;"), "{panel}");
        assert!(panel.contains("box-sizing: border-box;"), "{panel}");
        // The open queue starts below the wrapped rows, so its toggle stays
        // in reach to close it.
        let queue = sheet
            .split("@media (max-width: 40rem) {")
            .find_map(|block| block.split("\n  .pharos-queue {").nth(1))
            .and_then(|r| r.split('}').next())
            .unwrap_or_default();
        assert!(queue.contains("top: 9.5rem;"), "{queue}");
    }
}
