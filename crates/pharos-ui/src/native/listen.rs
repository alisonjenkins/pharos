//! The now-playing screen for a song: art, title and the audio controls over
//! a shader visualiser (013 US7) drawn by the GPU layer (ADR-0039).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use dioxus::prelude::*;

use super::platform;
use super::playback::MediaEvent;
use super::visualiser::{Analysis, AudioTap, Style, STYLE_KEY};
use super::watch_view::VIDEO_ID;
use pharos_client::api::{ClientEvent, ClientEventKind, ClientLevel};

use crate::render::{self, Fallback, Frame, Surface};

pub const CANVAS_ID: &str = "pharos-visualiser";
const PREFERS_REDUCED_MOTION: &str = "(prefers-reduced-motion: reduce)";
/// How often the theme colours are re-read, so a theme change mid-song
/// retints without a style lookup every frame.
const PALETTE_REFRESH_MS: f64 = 1000.0;

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

#[component]
pub fn ListenView(
    title: String,
    image: Option<String>,
    on_media: EventHandler<MediaEvent>,
) -> Element {
    let tap = use_hook(Tap::default);
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
    let current = style();
    let note = fallback().map(|f| match f {
        Fallback::ReducedMotion => "The visualiser is still because reduced motion is on.",
        _ => "This device cannot draw the visualiser.",
    });
    rsx! {
        section { class: "pharos-listen",
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
                div { class: "pharos-listen-art",
                    match &image {
                        Some(src) => rsx! { img { src: "{src}", alt: "" } },
                        None => rsx! { div { class: "pharos-poster-placeholder", aria_hidden: "true", "♪" } },
                    }
                }
                h2 { class: "pharos-listen-title", "{title}" }
                audio {
                    id: VIDEO_ID,
                    class: "pharos-audio",
                    controls: true,
                    autoplay: true,
                    onplaying: move |_| on_media.call(MediaEvent::Playing),
                    onpause: move |_| on_media.call(MediaEvent::Pause),
                    onseeked: move |_| on_media.call(MediaEvent::Seeked),
                    ontimeupdate: move |_| on_media.call(MediaEvent::TimeUpdate),
                    onended: move |_| on_media.call(MediaEvent::Ended),
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
                    p { class: "pharos-muted", "{note}" }
                }
            }
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
        let (title, image) = (title.to_owned(), image.map(str::to_owned));
        let mut dom = VirtualDom::new_with_props(
            move |()| {
                rsx! { ListenView { title: title.clone(), image: image.clone(), on_media: |_| {} } }
            },
            (),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
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
