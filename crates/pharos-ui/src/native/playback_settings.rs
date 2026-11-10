//! The Playback settings page as a pure view: this device's quality ceiling,
//! subtitle size and crossfade. Data in through props, events out through
//! handlers, so it renders under `dioxus-ssr`.

use dioxus::prelude::*;

use super::account_settings::DESCRIPTION_LANGUAGES;
use super::appearance::Notice;
use super::playback::{
    parse_crossfade, MseEngine, SubtitleSize, CROSSFADE_CHOICES, MAX_BITRATE_CHOICES,
};
use super::settings::Choices;
use super::turn::EpisodeTurn;

/// The `Prefs` keys the server maps onto the user's Jellyfin
/// `AudioLanguagePreference`, `SubtitleLanguagePreference` and
/// `SubtitleMode`; match the server's allow-list.
pub const AUDIO_LANGUAGE_PREF: &str = "playback.audio_language";
pub const SUBTITLE_LANGUAGE_PREF: &str = "playback.subtitle_language";
pub const SUBTITLE_MODE_PREF: &str = "playback.subtitle_mode";

/// The stored value that means "the language this title was made in".
const ORIGINAL_LANGUAGE: &str = "OriginalLanguage";

/// The languages a track preference offers, as (stored value, label): no
/// preference first, then English and the languages the description
/// selector offers. The audio preference alone may also be the title's
/// original language.
pub fn track_language_options(with_original: bool, stored: &str) -> Vec<(String, String)> {
    let mut out = vec![
        (String::new(), "No preference".to_owned()),
        ("eng".to_owned(), "English".to_owned()),
    ];
    out.extend(
        DESCRIPTION_LANGUAGES
            .iter()
            .filter(|(value, _)| !value.is_empty() && *value != ORIGINAL_LANGUAGE)
            .map(|(value, label)| ((*value).to_owned(), (*label).to_owned())),
    );
    if with_original {
        out.push((
            ORIGINAL_LANGUAGE.to_owned(),
            "The title's original language".to_owned(),
        ));
    }
    // jellyfin-web can store any language: show it so the page does not
    // look unset while the server applies it.
    let stored = stored.trim();
    if !stored.is_empty() && !out.iter().any(|(v, _)| v == stored) {
        out.push((stored.to_owned(), stored.to_owned()));
    }
    out
}

/// The subtitle modes, as (stored value, label), in the words of what
/// `stream_select` does for each.
pub const SUBTITLE_MODES: [(&str, &str); 5] = [
    ("Default", "Use the file's default"),
    ("Always", "Always"),
    ("OnlyForced", "Only forced (signs and foreign parts)"),
    ("None", "Off"),
    ("Smart", "When the audio isn't in my language"),
];

/// The selector value for a stored subtitle mode: unset, or a value that
/// names no mode, behaves as Default on the server.
pub fn subtitle_mode_option(stored: &str) -> &'static str {
    SUBTITLE_MODES
        .iter()
        .map(|(value, _)| *value)
        .find(|value| value.eq_ignore_ascii_case(stored.trim()))
        .unwrap_or("Default")
}

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
    /// Whether opening an episode or moving to the next turns like a cube.
    #[props(default)]
    episode_turn: EpisodeTurn,
    #[props(default)] on_episode_turn: EventHandler<EpisodeTurn>,
    /// The account's preferred audio language, as stored: empty, a
    /// three-letter code or `OriginalLanguage`.
    #[props(default)]
    audio_language: String,
    #[props(default)] on_audio_language: EventHandler<String>,
    /// The account's preferred subtitle language: empty or a code.
    #[props(default)]
    subtitle_language: String,
    #[props(default)] on_subtitle_language: EventHandler<String>,
    /// The account's subtitle mode, as stored.
    #[props(default)]
    subtitle_mode: String,
    #[props(default)] on_subtitle_mode: EventHandler<String>,
) -> Element {
    let audio_languages = track_language_options(true, &audio_language);
    let subtitle_languages = track_language_options(false, &subtitle_language);
    let modes = SUBTITLE_MODES
        .iter()
        .map(|(value, label)| ((*value).to_owned(), (*label).to_owned()))
        .collect::<Vec<_>>();
    let turns = EpisodeTurn::ALL
        .iter()
        .map(|t| (t.as_str().to_owned(), t.label().to_owned()))
        .collect::<Vec<_>>();
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
                div { class: "pharos-setting pharos-episode-turn",
                    div { class: "pharos-setting-text",
                        p { class: "pharos-setting-name", id: "episode-turn-label", "Episode transitions" }
                        p { id: "episode-turn-help", class: "pharos-setting-help",
                            "Opening an episode, or moving to the next, turns like a cube that follows the load, landing as the video starts. Off, or with reduced motion on, the video simply appears. Kept on this device."
                        }
                    }
                    Choices {
                        id: "episode-turn",
                        labelled_by: "episode-turn-label",
                        described_by: "episode-turn-help".to_owned(),
                        options: turns,
                        selected: episode_turn.as_str().to_owned(),
                        on_pick: move |value: String| on_episode_turn.call(EpisodeTurn::parse(&value)),
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
            div { class: "pharos-settings-group", id: "track-settings",
                div { class: "pharos-setting pharos-audio-language",
                    div { class: "pharos-setting-text",
                        p { class: "pharos-setting-name", id: "audio-language-label", "Preferred audio language" }
                        p { id: "audio-language-help", class: "pharos-setting-help",
                            "The audio track a title starts on when it has one in this language. Saved to your account, so it applies on every device. A track you pick for a show is still remembered and wins."
                        }
                    }
                    Choices {
                        id: "audio-language",
                        labelled_by: "audio-language-label",
                        described_by: "audio-language-help".to_owned(),
                        options: audio_languages,
                        selected: audio_language,
                        on_pick: move |value: String| on_audio_language.call(value),
                    }
                }
                div { class: "pharos-setting pharos-subtitle-language",
                    div { class: "pharos-setting-text",
                        p { class: "pharos-setting-name", id: "subtitle-language-label", "Preferred subtitle language" }
                        p { id: "subtitle-language-help", class: "pharos-setting-help",
                            "The language subtitles start in when the mode below turns them on. Saved to your account, so it applies on every device. A track you pick for a show is still remembered and wins."
                        }
                    }
                    Choices {
                        id: "subtitle-language",
                        labelled_by: "subtitle-language-label",
                        described_by: "subtitle-language-help".to_owned(),
                        options: subtitle_languages,
                        selected: subtitle_language,
                        on_pick: move |value: String| on_subtitle_language.call(value),
                    }
                }
                div { class: "pharos-setting pharos-subtitle-mode",
                    div { class: "pharos-setting-text",
                        p { class: "pharos-setting-name", id: "subtitle-mode-label", "Subtitle mode" }
                        p { id: "subtitle-mode-help", class: "pharos-setting-help",
                            "When subtitles start on. Always picks a track in your subtitle language, else a forced one. Only forced shows just signs and foreign-language parts. When the audio isn't in my language shows subtitles in your language only if the audio is not already in it. Saved to your account, so it applies on every device. A track you pick for a show is still remembered and wins."
                        }
                    }
                    Choices {
                        id: "subtitle-mode",
                        labelled_by: "subtitle-mode-label",
                        described_by: "subtitle-mode-help".to_owned(),
                        options: modes,
                        selected: subtitle_mode_option(&subtitle_mode).to_owned(),
                        on_pick: move |value: String| on_subtitle_mode.call(value),
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

    fn tracks_page(audio: &str, sub_lang: &str, mode: &str) -> String {
        let mut dom = VirtualDom::new_with_props(
            |(audio, sub_lang, mode): (String, String, String)| {
                rsx! {
                    PlaybackView {
                        notice: None,
                        max_quality: 0,
                        on_max_quality: |_| {},
                        subtitle_size: SubtitleSize::default(),
                        on_subtitle_size: |_| {},
                        crossfade_s: 0,
                        on_crossfade: |_| {},
                        engine: MseEngine::default(),
                        on_engine: |_| {},
                        audio_language: audio,
                        subtitle_language: sub_lang,
                        subtitle_mode: mode,
                    }
                }
            },
            (audio.to_owned(), sub_lang.to_owned(), mode.to_owned()),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn only_the_audio_language_offers_the_titles_original() {
        let audio = track_language_options(true, "");
        let subtitles = track_language_options(false, "");
        assert_eq!(audio[0], (String::new(), "No preference".to_owned()));
        assert_eq!(subtitles[0], audio[0]);
        assert!(audio.iter().any(|(v, _)| v == ORIGINAL_LANGUAGE));
        assert!(!subtitles.iter().any(|(v, _)| v == ORIGINAL_LANGUAGE));
        assert!(subtitles.iter().any(|(v, _)| v == "eng"));
        let values: Vec<_> = audio.iter().map(|(v, _)| v).collect();
        let mut unique = values.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(values.len(), unique.len(), "values are distinct");
    }

    #[test]
    fn a_stored_language_outside_the_list_is_still_offered() {
        let audio = track_language_options(true, "swe");
        assert_eq!(audio.last(), Some(&("swe".to_owned(), "swe".to_owned())));
        let known = track_language_options(false, "eng");
        assert_eq!(known.len(), track_language_options(false, "").len());
    }

    #[test]
    fn subtitle_mode_options_name_the_server_modes() {
        let values: Vec<_> = SUBTITLE_MODES.iter().map(|(v, _)| *v).collect();
        assert_eq!(values, ["Default", "Always", "OnlyForced", "None", "Smart"]);
        assert_eq!(subtitle_mode_option("smart"), "Smart");
        assert_eq!(subtitle_mode_option(""), "Default");
        assert_eq!(subtitle_mode_option("nonsense"), "Default");
    }

    #[test]
    fn the_track_settings_press_only_the_stored_choices() {
        let html = tracks_page("OriginalLanguage", "fra", "OnlyForced");
        for group in ["audio-language", "subtitle-language", "subtitle-mode"] {
            let own = html
                .split("<button")
                .filter_map(|b| b.split('>').next())
                .filter(|tag| {
                    tag.contains(&format!("id=\"{group}-")) && tag.contains("aria-pressed=\"true\"")
                })
                .count();
            assert_eq!(own, 1, "{group}: {html}");
        }
        assert!(
            button(&html, "audio-language-OriginalLanguage").contains("aria-pressed=\"true\""),
            "{html}"
        );
        assert!(
            button(&html, "subtitle-language-fra").contains("aria-pressed=\"true\""),
            "{html}"
        );
        assert!(
            button(&html, "subtitle-mode-OnlyForced").contains("aria-pressed=\"true\""),
            "{html}"
        );
        assert!(
            !html.contains("id=\"subtitle-language-OriginalLanguage\""),
            "{html}"
        );
    }

    #[test]
    fn nothing_stored_shows_no_preference_and_the_default_mode() {
        let html = tracks_page("", "", "");
        assert!(
            button(&html, "audio-language-").contains("aria-pressed=\"true\""),
            "{html}"
        );
        assert!(
            button(&html, "subtitle-language-").contains("aria-pressed=\"true\""),
            "{html}"
        );
        assert!(
            button(&html, "subtitle-mode-Default").contains("aria-pressed=\"true\""),
            "{html}"
        );
        assert!(html.contains("applies on every device"), "{html}");
        assert!(html.contains("still remembered and wins"), "{html}");
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
    fn episode_turn_is_offered_and_starts_on() {
        let html = page(0, SubtitleSize::Medium, 0);
        assert!(html.contains("Episode transitions"), "{html}");
        assert!(
            button(&html, "episode-turn-turn").contains("aria-pressed=\"true\""),
            "{html}"
        );
        assert!(html.contains("id=\"episode-turn-off\""), "{html}");
        assert_eq!(pressed(&html, "episode-turn").len(), 1, "{html}");
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
