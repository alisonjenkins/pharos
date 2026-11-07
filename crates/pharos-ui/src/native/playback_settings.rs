//! The Playback settings page as a pure view: this device's quality ceiling,
//! subtitle size and crossfade. Data in through props, events out through
//! handlers, so it renders under `dioxus-ssr`.

use dioxus::prelude::*;

use super::appearance::Notice;
use super::playback::{
    parse_crossfade, MseEngine, SubtitleSize, CROSSFADE_CHOICES, MAX_BITRATE_CHOICES,
};
use super::settings::Choices;

#[component]
pub fn PlaybackView(
    notice: Option<Notice>,
    max_quality: u64,
    on_max_quality: EventHandler<u64>,
    subtitle_size: SubtitleSize,
    on_subtitle_size: EventHandler<SubtitleSize>,
    /// The crossfade between songs, in seconds; 0 is off.
    crossfade_s: u32,
    on_crossfade: EventHandler<u32>,
    /// The engine that plays HLS through Media Source Extensions here.
    engine: MseEngine,
    on_engine: EventHandler<MseEngine>,
) -> Element {
    let engines = MseEngine::ALL
        .iter()
        .map(|e| (e.as_str().to_owned(), e.label().to_owned()))
        .collect::<Vec<_>>();
    let qualities = MAX_BITRATE_CHOICES
        .iter()
        .map(|(bps, label)| (bps.to_string(), (*label).to_owned()))
        .collect::<Vec<_>>();
    let sizes = SubtitleSize::ALL
        .iter()
        .map(|size| (size.as_str().to_owned(), size.label().to_owned()))
        .collect::<Vec<_>>();
    let fades = CROSSFADE_CHOICES
        .iter()
        .map(|&s| {
            (
                s.to_string(),
                if s == 0 {
                    "Off".to_owned()
                } else {
                    format!("{s} s")
                },
            )
        })
        .collect::<Vec<_>>();
    rsx! {
        section { class: "pharos-settings-page pharos-playback-page",
            h2 { "Playback" }
            p { class: "pharos-settings-lede", "How video, subtitles and music play on this device." }
            if let Some(notice) = &notice {
                p {
                    class: "pharos-notice",
                    role: if notice.error { "alert" } else { "status" },
                    "{notice.text}"
                }
            }
            div { class: "pharos-settings-group",
                div { class: "pharos-setting pharos-max-quality",
                    div { class: "pharos-setting-text",
                        p { class: "pharos-setting-name", id: "max-quality-label", "Maximum quality" }
                        p { id: "max-quality-help", class: "pharos-setting-help",
                            "Auto adapts to your connection: it lowers the video quality when playback keeps stalling and raises it again when the link allows. A limit you pick is a ceiling Auto never goes above."
                        }
                    }
                    Choices {
                        id: "max-quality",
                        labelled_by: "max-quality-label",
                        described_by: "max-quality-help".to_owned(),
                        options: qualities,
                        selected: max_quality.to_string(),
                        on_pick: move |value: String| {
                            if let Ok(bps) = value.parse::<u64>() {
                                on_max_quality.call(bps);
                            }
                        },
                    }
                }
                div { class: "pharos-setting pharos-subtitle-size",
                    div { class: "pharos-setting-text",
                        p { class: "pharos-setting-name", id: "subtitle-size-label", "Subtitle size" }
                        p { id: "subtitle-size-help", class: "pharos-setting-help",
                            "Text size of subtitles. Saved on this device."
                        }
                    }
                    Choices {
                        id: "subtitle-size",
                        labelled_by: "subtitle-size-label",
                        described_by: "subtitle-size-help".to_owned(),
                        options: sizes,
                        selected: subtitle_size.as_str().to_owned(),
                        on_pick: move |value: String| on_subtitle_size.call(SubtitleSize::parse(&value)),
                    }
                }
            }
            div { class: "pharos-settings-group", id: "playback-settings",
                div { class: "pharos-setting pharos-crossfade",
                    div { class: "pharos-setting-text",
                        p { class: "pharos-setting-name", id: "crossfade-label", "Crossfade between songs" }
                        p { id: "crossfade-help", class: "pharos-setting-help",
                            "Off by default. When on, the next song in the queue starts under the end of this one as it fades out. Kept on this device."
                        }
                    }
                    Choices {
                        id: "crossfade",
                        labelled_by: "crossfade-label",
                        described_by: "crossfade-help".to_owned(),
                        options: fades,
                        selected: crossfade_s.to_string(),
                        on_pick: move |value: String| on_crossfade.call(parse_crossfade(&value)),
                    }
                }
                div { class: "pharos-setting pharos-engine",
                    div { class: "pharos-setting-text",
                        p { class: "pharos-setting-name", id: "engine-label", "Streaming engine" }
                        p { id: "engine-help", class: "pharos-setting-help",
                            "What plays streamed video in this browser. hls.js is the default; the Pharos engine is experimental and still hands playback to hls.js where it cannot play a stream. Kept on this device."
                        }
                    }
                    Choices {
                        id: "engine",
                        labelled_by: "engine-label",
                        described_by: "engine-help".to_owned(),
                        options: engines,
                        selected: engine.as_str().to_owned(),
                        on_pick: move |value: String| {
                            if let Some(engine) = MseEngine::parse(&value) {
                                on_engine.call(engine);
                            }
                        },
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(max_quality: u64, subtitle_size: SubtitleSize, crossfade_s: u32) -> String {
        page_with_engine(
            max_quality,
            subtitle_size,
            crossfade_s,
            MseEngine::default(),
        )
    }

    fn page_with_engine(
        max_quality: u64,
        subtitle_size: SubtitleSize,
        crossfade_s: u32,
        engine: MseEngine,
    ) -> String {
        let mut dom = VirtualDom::new_with_props(
            |(max_quality, subtitle_size, crossfade_s, engine): (
                u64,
                SubtitleSize,
                u32,
                MseEngine,
            )| {
                rsx! {
                    PlaybackView {
                        notice: None,
                        max_quality,
                        on_max_quality: |_| {},
                        subtitle_size,
                        on_subtitle_size: |_| {},
                        crossfade_s,
                        on_crossfade: |_| {},
                        engine,
                        on_engine: |_| {},
                    }
                }
            },
            (max_quality, subtitle_size, crossfade_s, engine),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// The `<button>` start tag with this id.
    fn button<'a>(html: &'a str, id: &str) -> &'a str {
        let at = html.find(&format!("id=\"{id}\"")).unwrap_or(0);
        let start = html[..at].rfind('<').unwrap_or(0);
        let end = html[at..].find('>').map_or(html.len(), |e| at + e);
        &html[start..=end.min(html.len() - 1)]
    }

    fn pressed(html: &str, group: &str) -> Vec<String> {
        html.split("<button")
            .filter(|b| {
                b.contains(&format!("id=\"{group}-")) && b.contains("aria-pressed=\"true\"")
            })
            .map(|b| b.to_owned())
            .collect()
    }

    #[test]
    fn max_quality_presses_only_the_stored_choice() {
        let html = page(2_000_000, SubtitleSize::default(), 0);
        assert_eq!(pressed(&html, "max-quality").len(), 1, "{html}");
        assert!(
            button(&html, "max-quality-2000000").contains("aria-pressed=\"true\""),
            "{html}"
        );
        for (_, label) in MAX_BITRATE_CHOICES {
            assert!(html.contains(label), "{label} missing: {html}");
        }
    }

    #[test]
    fn max_quality_defaults_to_auto_and_says_auto_adapts() {
        let html = page(0, SubtitleSize::default(), 0);
        assert!(
            button(&html, "max-quality-0").contains("aria-pressed=\"true\""),
            "{html}"
        );
        assert!(html.contains("adapts to your connection"), "{html}");
        assert!(html.contains("never goes above"), "{html}");
    }

    #[test]
    fn subtitle_size_presses_only_the_stored_choice() {
        let html = page(0, SubtitleSize::Large, 0);
        assert_eq!(pressed(&html, "subtitle-size").len(), 1, "{html}");
        assert!(
            button(&html, "subtitle-size-large").contains("aria-pressed=\"true\""),
            "{html}"
        );
        for size in SubtitleSize::ALL {
            assert!(
                html.contains(size.label()),
                "{} missing: {html}",
                size.label()
            );
        }
    }

    #[test]
    fn subtitle_size_defaults_to_medium() {
        let html = page(0, SubtitleSize::default(), 0);
        assert!(
            button(&html, "subtitle-size-medium").contains("aria-pressed=\"true\""),
            "{html}"
        );
        assert!(html.contains("Saved on this device."), "{html}");
    }

    #[test]
    fn crossfade_is_offered_and_starts_off() {
        let html = page(0, SubtitleSize::default(), 0);
        assert!(html.contains("Crossfade between songs"), "{html}");
        assert!(
            button(&html, "crossfade-0").contains("aria-pressed=\"true\""),
            "{html}"
        );
        assert!(html.contains("id=\"crossfade-12\""), "{html}");
        assert_eq!(pressed(&html, "crossfade").len(), 1, "{html}");
    }

    #[test]
    fn engine_defaults_to_hlsjs_and_offers_the_rust_engine() {
        let html = page(0, SubtitleSize::default(), 0);
        assert_eq!(pressed(&html, "engine").len(), 1, "{html}");
        assert!(
            button(&html, "engine-hlsjs").contains("aria-pressed=\"true\""),
            "{html}"
        );
        assert!(html.contains("Pharos (experimental)"), "{html}");
    }

    #[test]
    fn engine_presses_only_the_stored_choice() {
        let html = page_with_engine(0, SubtitleSize::default(), 0, MseEngine::Rust);
        assert_eq!(pressed(&html, "engine").len(), 1, "{html}");
        assert!(
            button(&html, "engine-rust").contains("aria-pressed=\"true\""),
            "{html}"
        );
    }
}
