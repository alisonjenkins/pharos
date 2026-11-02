//! The now-playing screen for a song: art, title and the audio controls over
//! a shader visualiser (013 US7) drawn by the GPU layer (ADR-0039).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use dioxus::prelude::*;

use super::platform;
use super::playback::MediaEvent;
use super::views::VIDEO_ID;
use super::visualiser::{Analysis, AudioTap, Style, STYLE_KEY};
use crate::render::{self, Fallback, Frame};

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
        if self.tried.get() || !platform::media_playing(VIDEO_ID) || !platform::user_activated() {
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
            if let Err(f) = run(style, program, &tap).await {
                let detail = match &f {
                    Fallback::Shader(log) => log.as_str(),
                    _ => "",
                };
                platform::info(&format!(
                    "pharos: renderer mode=fallback reason={} style={} detail={detail}",
                    f.label(),
                    style.label()
                ));
                on_fallback.call(f);
            }
        });
    });
    rsx! {
        canvas { id: CANVAS_ID, class: "pharos-visualiser", aria_hidden: "true" }
    }
}

/// Draw every animation frame until the component unmounts (which drops
/// this future) or the surface fails.
async fn run(style: Style, program: render::Program, tap: &Tap) -> Result<(), Fallback> {
    let reduced = platform::prefers(PREFERS_REDUCED_MOTION);
    let mut surface = render::open(CANVAS_ID, program, reduced)?;
    platform::info(&format!(
        "pharos: renderer mode=webgl2 style={}",
        style.label()
    ));
    let start = platform::now_ms();
    let mut palette = platform::theme_palette();
    let mut palette_at = start;
    let mut analysis: Option<Analysis> = None;
    loop {
        platform::animation_frame().await;
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
}
