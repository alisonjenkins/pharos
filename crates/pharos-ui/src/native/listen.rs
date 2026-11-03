//! The now-playing screen for a song: art, title and the audio controls over
//! a shader visualiser (013 US7) drawn by the GPU layer (ADR-0039).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use dioxus::prelude::*;

use super::platform;
use super::playback::{clock, MediaEvent, MediaState};
use super::visualiser::{Analysis, AudioTap, Style, STYLE_KEY};
use super::watch_view::{
    total_ms, transport_key, use_shortcuts, Control, Volume, SEEK_STEP_MS, VIDEO_ID,
};
use pharos_client::api::{ClientEvent, ClientEventKind, ClientLevel, ItemRef};

use crate::render::{self, Fallback, Frame, Surface};

pub const CANVAS_ID: &str = "pharos-visualiser";
const PREFERS_REDUCED_MOTION: &str = "(prefers-reduced-motion: reduce)";
/// How often the theme colours are re-read, so a theme change mid-song
/// retints without a style lookup every frame.
const PALETTE_REFRESH_MS: f64 = 1000.0;

/// How close the playhead must be to a requested seek to count as arrived.
const SEEK_SETTLE_MS: u64 = 1_000;

/// How long a released scrub may wait for its seek before the bar follows
/// the playhead again, so a seek that never lands cannot pin the thumb.
const SEEK_GIVE_UP_MS: u32 = 5_000;

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

impl Tap {
    /// Route the media element through an analyser once it is playing and
    /// the page has had a gesture; one attempt per screen.
    fn connect(&self) {
        if self.tried.get() {
            return;
        }
        let waiting = if !platform::media_playing(VIDEO_ID) {
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
        match platform::audio_tap(VIDEO_ID) {
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
    year: Option<u16>,
    genres: Vec<String>,
    runtime_ms: u64,
    media: MediaState,
    /// The tracks before and after this one on its album.
    previous: Option<ItemRef>,
    next: Option<ItemRef>,
    on_media: EventHandler<MediaEvent>,
    on_control: EventHandler<Control>,
) -> Element {
    let tap = use_hook(Tap::default);
    let shortcuts = use_shortcuts(media, previous, next, on_control);
    let mut style =
        use_signal(|| Style::parse(platform::stored_theme(STYLE_KEY).ok().flatten().as_deref()));
    let mut fallback = use_signal(|| None::<Fallback>);
    let mut choose = move |next: Style| {
        style.set(next);
        fallback.set(None);
        if let Err(e) = platform::store_theme(STYLE_KEY, next.label()) {
            platform::warn(&format!("pharos: visualiser style not stored error={e}"));
        }
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
    let meta = meta_line(year, &genres);
    let on_key = move |evt: KeyboardEvent| {
        let key = evt.key();
        let control = shortcuts
            .key(&key, evt.modifiers().shift())
            .or_else(|| transport_key(&key, &media));
        if let Some(control) = control {
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
            onmouseup: move |evt: MouseEvent| shortcuts.mouse_up(&evt),
            "data-playing": if media.paused { "false" } else { "true" },
            if let (Some(program), None) = (current.program(), fallback()) {
                Visualiser {
                    key: "{current.label()}",
                    style: current,
                    program,
                    tap: tap.clone(),
                    on_fallback: move |f| fallback.set(Some(f)),
                }
            }
            div { class: "pharos-listen-body",
                div { class: "pharos-listen-cover",
                    div { class: "pharos-listen-disc", aria_hidden: "true" }
                    div { class: "pharos-listen-art",
                        match &image {
                            Some(src) => rsx! { img { src: "{src}", alt: "" } },
                            None => rsx! { div { class: "pharos-poster-placeholder", aria_hidden: "true", "♪" } },
                        }
                    }
                }
                div { class: "pharos-listen-main",
                    p { class: "pharos-listen-eyebrow", "Now playing" }
                    h2 { class: "pharos-listen-title", "{title}" }
                    if let Some(meta) = meta {
                        p { class: "pharos-listen-meta", "{meta}" }
                    }
                    audio {
                        id: VIDEO_ID,
                        class: "pharos-audio",
                        autoplay: true,
                        onplaying: move |_| on_media.call(MediaEvent::Playing),
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
                            onkeydown: move |evt: KeyboardEvent| evt.stop_propagation(),
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
                div { class: "pharos-visualiser-styles", role: "group", aria_label: "Visualiser",
                    for s in Style::ALL {
                        button {
                            class: "pharos-chip",
                            r#type: "button",
                            aria_pressed: if s == current { "true" } else { "false" },
                            onclick: move |_| choose(s),
                            "{s.name()}"
                        }
                    }
                }
                if let Some(note) = note {
                    p { class: "pharos-muted pharos-listen-note", "{note}" }
                }
            }
        }
    }
}

/// "2003 · Trip-hop, Electronic"; `None` when there is neither.
fn meta_line(year: Option<u16>, genres: &[String]) -> Option<String> {
    let parts: Vec<String> = year
        .map(|y| y.to_string())
        .into_iter()
        .chain((!genres.is_empty()).then(|| genres.join(", ")))
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

const ICON_PLAY: &str =
    "M8 5.5v13a1 1 0 0 0 1.5.86l10.5-6.5a1 1 0 0 0 0-1.72L9.5 4.64A1 1 0 0 0 8 5.5Z";
const ICON_PAUSE: &str = "M7 5h3.5v14H7zM13.5 5H17v14h-3.5z";
const ICON_BACK: &str = "M11 7V4L5 9l6 5v-3a5 5 0 1 1-5 5H4a7 7 0 1 0 7-7Z";
const ICON_FORWARD: &str = "M13 7V4l6 5-6 5v-3a5 5 0 1 0 5 5h2a7 7 0 1 1-7-7Z";
const ICON_VOLUME: &str = "M4 9v6h4l5 4V5L8 9H4Zm12.5 3a4.5 4.5 0 0 0-2.5-4v8a4.5 4.5 0 0 0 2.5-4ZM14 3.2v2.1a7 7 0 0 1 0 13.4v2.1a9 9 0 0 0 0-17.6Z";
const ICON_MUTED: &str = "M4 9v6h4l5 4V5L8 9H4Zm12.6 3 2.7-2.7-1.4-1.4-2.7 2.7-2.7-2.7-1.4 1.4 2.7 2.7-2.7 2.7 1.4 1.4 2.7-2.7 2.7 2.7 1.4-1.4L16.6 12Z";

#[component]
fn Icon(path: &'static str) -> Element {
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
    on_fallback: EventHandler<Fallback>,
) -> Element {
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
                    draw(surface, &tap).await
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

/// Log the renderer decision and send it as client events (016
/// `renderer_mode`, and `webgl_context_lost` when that is the reason).
fn report(style: Style, fallback: Option<&Fallback>) {
    let (line, events) = renderer_signals(style, fallback);
    platform::info(&line);
    for event in events {
        platform::report_event(event);
    }
}

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

/// Draw every animation frame until the component unmounts (which drops
/// this future) or the surface fails. A paused song keeps its last frame,
/// so a headset spends no GPU time on a still picture.
async fn draw(mut surface: Box<dyn Surface>, tap: &Tap) -> Result<(), Fallback> {
    let start = platform::now_ms();
    let mut palette = platform::theme_palette();
    let mut palette_at = start;
    let mut analysis: Option<Analysis> = None;
    let mut drawn = false;
    loop {
        platform::animation_frame().await;
        if drawn && !platform::media_playing(VIDEO_ID) {
            continue;
        }
        let now = platform::now_ms();
        if now - palette_at >= PALETTE_REFRESH_MS {
            palette = platform::theme_palette();
            palette_at = now;
        }
        tap.connect();
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

    fn render(title: &str, image: Option<&str>) -> String {
        render_with(title, image, MediaState::default())
    }

    fn render_with(title: &str, image: Option<&str>, media: MediaState) -> String {
        let (title, image) = (title.to_owned(), image.map(str::to_owned));
        let mut dom = VirtualDom::new_with_props(
            move |()| {
                rsx! {
                    ListenView {
                        title: title.clone(),
                        image: image.clone(),
                        year: Some(1998),
                        genres: vec!["Trip-hop".to_owned()],
                        runtime_ms: 330_000,
                        media,
                        on_media: |_| {},
                        on_control: |_| {},
                    }
                }
            },
            (),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
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
        assert!(html.contains("1998 · Trip-hop"), "{html}");
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
    fn the_meta_line_shows_what_there_is() {
        assert_eq!(meta_line(None, &[]), None);
        assert_eq!(meta_line(Some(2003), &[]).as_deref(), Some("2003"));
        let genres = ["Rock".to_owned(), "Pop".to_owned()];
        assert_eq!(meta_line(None, &genres).as_deref(), Some("Rock, Pop"));
        assert_eq!(
            meta_line(Some(2003), &genres).as_deref(),
            Some("2003 · Rock, Pop")
        );
    }

    #[test]
    fn a_song_gets_art_title_audio_and_every_style() {
        let html = render("Teardrop", Some("/art.jpg"));
        assert!(html.contains("Teardrop"));
        assert!(html.contains(r#"src="/art.jpg""#));
        assert!(
            html.contains(&format!(r#"<audio id="{VIDEO_ID}""#)),
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
            html.contains(&format!(r#"<canvas id="{CANVAS_ID}""#)),
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
}
