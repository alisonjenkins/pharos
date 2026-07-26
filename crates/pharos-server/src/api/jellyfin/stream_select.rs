//! Which audio and subtitle track a client should start on.
//!
//! A port of Jellyfin's `Emby.Server.Implementations.Library.MediaStreamSelector`,
//! deliberately faithful rather than improved: the preferences that drive it
//! (`AudioLanguagePreference`, `SubtitleLanguagePreference`, `SubtitleMode`,
//! `PlayDefaultAudioTrack`) are configured from stock jellyfin-web, so a client
//! that sets them there must get the behaviour that UI describes. Where the
//! upstream behaviour is surprising it is documented, not corrected — the
//! surprises are the settings doing what they say.
//!
//! The surprise worth knowing: with `PlayDefaultAudioTrack` on (Jellyfin's
//! default), a track flagged `default` in the container BEATS the language
//! preference. Aliens carries a Ukrainian AC-3 flagged default and an English
//! DTS-HD MA that is not, so "prefer English" alone keeps playing Ukrainian —
//! upstream's own test pins exactly this (`["eng"], preferDefaultTrack: true`
//! selects the French default track). Turning that setting OFF is what makes
//! the language preference decide.
//!
//! Scores are upstream's, digit for digit, because they are observable through
//! `MediaStream.Score` and because the ordering they induce is the contract.

/// The stream properties selection depends on — everything else about a track
/// is irrelevant here, and taking a narrow view keeps this testable without
/// building a whole `MediaStreamDto`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamFacts {
    pub index: u32,
    pub language: Option<String>,
    pub is_default: bool,
    pub is_forced: bool,
    pub is_external: bool,
    pub supports_external_stream: bool,
    pub is_text_subtitle_stream: bool,
}

/// Jellyfin's `SubtitlePlaybackMode`. Parsed from the user's configuration
/// string; an unknown value takes `Default`, which is what upstream's enum
/// binding does with one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubtitleMode {
    #[default]
    Default,
    Always,
    OnlyForced,
    None,
    Smart,
}

impl SubtitleMode {
    pub fn from_config(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "always" => Self::Always,
            "onlyforced" => Self::OnlyForced,
            "none" => Self::None,
            "smart" => Self::Smart,
            _ => Self::Default,
        }
    }
}

impl StreamFacts {
    /// The selection-relevant properties of an advertised stream.
    fn from_dto(s: &pharos_jellyfin_api::dto::MediaStreamDto) -> Self {
        Self {
            index: s.index,
            language: s.language.clone(),
            is_default: s.is_default,
            is_forced: s.is_forced,
            is_external: s.is_external,
            supports_external_stream: s.supports_external_stream,
            is_text_subtitle_stream: s.is_text_subtitle_stream,
        }
    }
}

/// The audio tracks of an advertised stream list, in container order.
pub fn audio_facts(streams: &[pharos_jellyfin_api::dto::MediaStreamDto]) -> Vec<StreamFacts> {
    streams
        .iter()
        .filter(|s| s.kind == "Audio")
        .map(StreamFacts::from_dto)
        .collect()
}

/// The subtitle tracks of an advertised stream list, in container order.
pub fn subtitle_facts(streams: &[pharos_jellyfin_api::dto::MediaStreamDto]) -> Vec<StreamFacts> {
    streams
        .iter()
        .filter(|s| s.kind == "Subtitle")
        .map(StreamFacts::from_dto)
        .collect()
}

/// Upstream's `GetStreamScore`, digit for digit.
///
/// The language term dominates by three orders of magnitude, so the flag terms
/// only ever break ties WITHIN a language — which is why a preferred-language
/// track sorts above a default-flagged one, and why `prefer_default_track`
/// has to be a separate override rather than another term here.
pub fn stream_score(stream: &StreamFacts, preferred_languages: &[String]) -> i64 {
    let index = preferred_languages
        .iter()
        .position(|l| lang_eq(l, stream.language.as_deref()));
    let mut score: i64 = match index {
        Some(i) => 101 - i as i64,
        None => 1,
    };
    score = score * 10 + if stream.is_forced { 2 } else { 1 };
    score = score * 10 + if stream.is_default { 2 } else { 1 };
    score = score * 10
        + if stream.supports_external_stream {
            2
        } else {
            1
        };
    score = score * 10 + if stream.is_text_subtitle_stream { 2 } else { 1 };
    score = score * 10 + if stream.is_external { 2 } else { 1 };
    score
}

/// Streams of one kind, best first. Ties keep their original relative order so
/// the container's own track order decides between equals.
fn sorted_by_score(streams: &[StreamFacts], preferred_languages: &[String]) -> Vec<StreamFacts> {
    let mut out = streams.to_vec();
    out.sort_by_key(|s| std::cmp::Reverse(stream_score(s, preferred_languages)));
    out
}

/// The audio track to start on, or `None` when there is no audio at all.
pub fn default_audio_stream_index(
    audio: &[StreamFacts],
    preferred_languages: &[String],
    prefer_default_track: bool,
) -> Option<u32> {
    let sorted = sorted_by_score(audio, preferred_languages);
    if prefer_default_track {
        if let Some(s) = sorted.iter().find(|s| s.is_default) {
            return Some(s.index);
        }
    }
    sorted.first().map(|s| s.index)
}

/// The subtitle track to start on under `mode`, given the language of the
/// audio track that was chosen.
pub fn default_subtitle_stream_index(
    subtitles: &[StreamFacts],
    preferred_languages: &[String],
    mode: SubtitleMode,
    audio_language: Option<&str>,
) -> Option<u32> {
    if mode == SubtitleMode::None {
        return None;
    }
    // Upstream sorts subtitles by flags rather than by score here: external >
    // default > preferred-and-not-forced > preferred-and-forced >
    // forced-undefined > forced.
    let mut sorted: Vec<StreamFacts> = subtitles.to_vec();
    sorted.sort_by_key(|s| {
        let lang_pref = matches_preferred(s.language.as_deref(), preferred_languages);
        std::cmp::Reverse((
            s.is_external,
            s.is_default,
            !s.is_forced && lang_pref,
            s.is_forced && lang_pref,
            s.is_forced && is_language_undefined(s.language.as_deref()),
            s.is_forced,
        ))
    });

    let chosen = match mode {
        SubtitleMode::None => None,
        SubtitleMode::Default => sorted
            .iter()
            .find(|s| s.is_external || s.is_default || s.is_forced),
        SubtitleMode::Smart => {
            // Subtitles only when the audio is NOT already in a language the
            // user reads; when it is, behave like OnlyForced.
            if !preferred_languages
                .iter()
                .any(|l| lang_eq(l, audio_language))
            {
                sorted
                    .iter()
                    .find(|s| matches_preferred(s.language.as_deref(), preferred_languages))
            } else {
                behavior_only_forced(&sorted, preferred_languages)
                    .first()
                    .copied()
            }
        }
        SubtitleMode::Always => sorted
            .iter()
            .find(|s| !s.is_forced && matches_preferred(s.language.as_deref(), preferred_languages))
            .or_else(|| {
                behavior_only_forced(&sorted, preferred_languages)
                    .first()
                    .copied()
            }),
        SubtitleMode::OnlyForced => behavior_only_forced(&sorted, preferred_languages)
            .first()
            .copied(),
    };
    chosen.map(|s| s.index)
}

/// Forced tracks the user can read — their preferred languages, or a track
/// whose language the container never stated.
fn behavior_only_forced<'a>(
    sorted: &'a [StreamFacts],
    preferred_languages: &[String],
) -> Vec<&'a StreamFacts> {
    let mut out: Vec<&StreamFacts> = sorted
        .iter()
        .filter(|s| {
            s.is_forced
                && (matches_preferred(s.language.as_deref(), preferred_languages)
                    || is_language_undefined(s.language.as_deref()))
        })
        .collect();
    out.sort_by_key(|s| {
        std::cmp::Reverse((
            matches_preferred(s.language.as_deref(), preferred_languages),
            is_language_undefined(s.language.as_deref()),
        ))
    });
    out
}

/// An EMPTY preference list means "any language", not "no language" — a user
/// who has set no preference must not have every track filtered away.
fn matches_preferred(language: Option<&str>, preferred_languages: &[String]) -> bool {
    preferred_languages.is_empty() || preferred_languages.iter().any(|l| lang_eq(l, language))
}

fn is_language_undefined(language: Option<&str>) -> bool {
    match language {
        None => true,
        Some(l) => {
            let l = l.trim();
            l.is_empty()
                || ["und", "unknown", "undetermined", "mul", "zxx"]
                    .iter()
                    .any(|p| l.eq_ignore_ascii_case(p))
        }
    }
}

fn lang_eq(pref: &str, language: Option<&str>) -> bool {
    language.is_some_and(|l| l.eq_ignore_ascii_case(pref))
}

/// Expand a configured language into the tags a container might carry.
///
/// jellyfin-web posts an ISO 639-2 code (`eng`), but files are tagged with
/// whatever the muxer felt like — `en`, `eng`, sometimes `en-US`. Upstream
/// resolves this through its localization tables; the common two- and
/// three-letter forms are covered here, and anything unrecognised passes
/// through unchanged so an exact tag match still works.
pub fn normalize_language(pref: &str) -> Vec<String> {
    let p = pref.trim().to_ascii_lowercase();
    if p.is_empty() {
        return Vec::new();
    }
    // (639-1, 639-2/T, 639-2/B) for the languages a library realistically
    // carries; the table is a convenience, not an authority — an unlisted code
    // still matches itself.
    const TABLE: &[(&str, &str, &str)] = &[
        ("en", "eng", "eng"),
        ("ja", "jpn", "jpn"),
        ("de", "deu", "ger"),
        ("fr", "fra", "fre"),
        ("es", "spa", "spa"),
        ("it", "ita", "ita"),
        ("pt", "por", "por"),
        ("ru", "rus", "rus"),
        ("uk", "ukr", "ukr"),
        ("pl", "pol", "pol"),
        ("nl", "nld", "dut"),
        ("sv", "swe", "swe"),
        ("da", "dan", "dan"),
        ("no", "nor", "nor"),
        ("fi", "fin", "fin"),
        ("cs", "ces", "cze"),
        ("hu", "hun", "hun"),
        ("tr", "tur", "tur"),
        ("ar", "ara", "ara"),
        ("he", "heb", "heb"),
        ("hi", "hin", "hin"),
        ("ko", "kor", "kor"),
        ("zh", "zho", "chi"),
    ];
    // A region-qualified tag (`en-US`) keeps its base for matching.
    let base = p.split(['-', '_']).next().unwrap_or(&p).to_string();
    for (one, two_t, two_b) in TABLE {
        if base == *one || base == *two_t || base == *two_b {
            let mut out = vec![(*one).to_string(), (*two_t).to_string()];
            if two_b != two_t {
                out.push((*two_b).to_string());
            }
            return out;
        }
    }
    if base == p {
        vec![p]
    } else {
        vec![p, base]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn langs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    fn audio(index: u32, language: &str, is_default: bool) -> StreamFacts {
        StreamFacts {
            index,
            language: Some(language.into()),
            is_default,
            ..Default::default()
        }
    }

    /// Upstream's `GetStreamScore_MediaStream_CorrectScore`, values copied from
    /// its test data. The scores are observable (`MediaStream.Score`), so a
    /// divergence here is a divergence a client can see.
    #[test]
    fn stream_scores_match_upstreams_table() {
        let prefs = langs(&["eng", "fre"]);
        let base = StreamFacts::default();
        assert_eq!(stream_score(&base, &prefs), 111_111);
        assert_eq!(
            stream_score(
                &StreamFacts {
                    language: Some("eng".into()),
                    ..Default::default()
                },
                &prefs
            ),
            10_111_111
        );
        assert_eq!(
            stream_score(
                &StreamFacts {
                    language: Some("fre".into()),
                    ..Default::default()
                },
                &prefs
            ),
            10_011_111
        );
        assert_eq!(
            stream_score(
                &StreamFacts {
                    is_forced: true,
                    ..Default::default()
                },
                &prefs
            ),
            121_111
        );
        assert_eq!(
            stream_score(
                &StreamFacts {
                    is_default: true,
                    ..Default::default()
                },
                &prefs
            ),
            112_111
        );
        assert_eq!(
            stream_score(
                &StreamFacts {
                    supports_external_stream: true,
                    ..Default::default()
                },
                &prefs
            ),
            111_211
        );
        assert_eq!(
            stream_score(
                &StreamFacts {
                    is_external: true,
                    ..Default::default()
                },
                &prefs
            ),
            111_112
        );
        assert_eq!(
            stream_score(
                &StreamFacts {
                    language: Some("eng".into()),
                    is_forced: true,
                    is_default: true,
                    supports_external_stream: true,
                    is_external: true,
                    ..Default::default()
                },
                &prefs
            ),
            10_122_212
        );
    }

    /// Upstream's `GetDefaultAudioStreamIndex_PreferredLanguage_SelectsCorrect`,
    /// same fixture (a French DEFAULT track and an English non-default one) and
    /// same expectations — including the two rows where `prefer_default_track`
    /// overrides the language preference.
    #[test]
    fn audio_selection_matches_upstreams_table() {
        let streams = vec![audio(1, "fre", true), audio(2, "eng", false)];
        let cases: &[(&[&str], bool, u32)] = &[
            (&[], false, 1),
            (&[], true, 1),
            (&["eng"], false, 2),
            (&["eng"], true, 1),
            (&["eng", "fre"], false, 2),
            (&["fre", "eng"], false, 1),
            (&["eng", "fre"], true, 1),
        ];
        for (prefs, prefer_default, want) in cases {
            let got = default_audio_stream_index(&streams, &langs(prefs), *prefer_default);
            assert_eq!(
                got,
                Some(*want),
                "prefs {prefs:?} prefer_default {prefer_default}"
            );
        }
    }

    #[test]
    fn audio_selection_of_nothing_is_none() {
        for prefer_default in [true, false] {
            assert_eq!(
                default_audio_stream_index(&[], &langs(&["eng"]), prefer_default),
                None
            );
        }
    }

    /// The Aliens case, which is why this exists: the container's only
    /// default-flagged audio is a language the user does not speak. The
    /// preference alone does NOT rescue it — the user must also turn off
    /// "play default audio track regardless of language".
    #[test]
    fn a_default_flagged_dub_beats_the_language_preference_until_that_setting_is_off() {
        let streams = vec![
            audio(1, "ukr", true),
            audio(2, "ukr", false),
            audio(3, "ukr", false),
            audio(4, "eng", false),
        ];
        let prefs = langs(&["eng"]);
        assert_eq!(
            default_audio_stream_index(&streams, &prefs, true),
            Some(1),
            "prefer_default_track on: the Ukrainian default wins, as upstream"
        );
        assert_eq!(
            default_audio_stream_index(&streams, &prefs, false),
            Some(4),
            "prefer_default_track off: the English track wins"
        );
    }

    #[test]
    fn subtitle_mode_none_selects_nothing() {
        let subs = vec![StreamFacts {
            index: 5,
            language: Some("eng".into()),
            is_default: true,
            ..Default::default()
        }];
        assert_eq!(
            default_subtitle_stream_index(&subs, &langs(&["eng"]), SubtitleMode::None, Some("jpn")),
            None
        );
    }

    /// Smart is the mode that gives "Japanese audio with English subtitles" for
    /// anime and no subtitles for an English film, from ONE setting.
    #[test]
    fn smart_subtitles_appear_only_when_the_audio_is_foreign() {
        let subs = vec![StreamFacts {
            index: 5,
            language: Some("eng".into()),
            ..Default::default()
        }];
        let prefs = langs(&["eng"]);
        assert_eq!(
            default_subtitle_stream_index(&subs, &prefs, SubtitleMode::Smart, Some("jpn")),
            Some(5),
            "foreign audio → the readable subtitle track"
        );
        assert_eq!(
            default_subtitle_stream_index(&subs, &prefs, SubtitleMode::Smart, Some("eng")),
            None,
            "audio already readable → nothing but forced tracks"
        );
    }

    /// With readable audio, Smart still surfaces a FORCED track — the
    /// signs-and-songs case.
    #[test]
    fn smart_still_shows_forced_tracks_under_readable_audio() {
        let subs = vec![
            StreamFacts {
                index: 5,
                language: Some("eng".into()),
                ..Default::default()
            },
            StreamFacts {
                index: 6,
                language: Some("eng".into()),
                is_forced: true,
                ..Default::default()
            },
        ];
        assert_eq!(
            default_subtitle_stream_index(
                &subs,
                &langs(&["eng"]),
                SubtitleMode::Smart,
                Some("eng")
            ),
            Some(6)
        );
    }

    #[test]
    fn only_forced_accepts_an_undefined_language_tag() {
        for tag in [None, Some("und"), Some("mul"), Some("zxx"), Some("")] {
            let subs = vec![StreamFacts {
                index: 7,
                language: tag.map(str::to_string),
                is_forced: true,
                ..Default::default()
            }];
            assert_eq!(
                default_subtitle_stream_index(
                    &subs,
                    &langs(&["eng"]),
                    SubtitleMode::OnlyForced,
                    Some("jpn")
                ),
                Some(7),
                "tag {tag:?}"
            );
        }
    }

    #[test]
    fn always_prefers_a_full_track_over_a_forced_one() {
        let subs = vec![
            StreamFacts {
                index: 6,
                language: Some("eng".into()),
                is_forced: true,
                ..Default::default()
            },
            StreamFacts {
                index: 7,
                language: Some("eng".into()),
                ..Default::default()
            },
        ];
        assert_eq!(
            default_subtitle_stream_index(
                &subs,
                &langs(&["eng"]),
                SubtitleMode::Always,
                Some("jpn")
            ),
            Some(7)
        );
    }

    #[test]
    fn language_normalization_covers_the_tags_files_actually_carry() {
        assert_eq!(normalize_language("eng"), vec!["en", "eng"]);
        assert_eq!(normalize_language("en"), vec!["en", "eng"]);
        assert_eq!(normalize_language("en-US"), vec!["en", "eng"]);
        assert_eq!(normalize_language("ja"), vec!["ja", "jpn"]);
        // 639-2/B and /T differ for these, and files use both.
        assert_eq!(normalize_language("fre"), vec!["fr", "fra", "fre"]);
        assert_eq!(normalize_language("ger"), vec!["de", "deu", "ger"]);
        // Unknown codes still match themselves rather than vanishing.
        assert_eq!(normalize_language("qya"), vec!["qya"]);
        assert!(normalize_language("  ").is_empty());
    }

    #[test]
    fn an_unset_preference_matches_any_language() {
        let streams = vec![audio(1, "ukr", false), audio(2, "eng", false)];
        assert_eq!(default_audio_stream_index(&streams, &[], false), Some(1));
    }

    #[test]
    fn subtitle_mode_parses_the_strings_jellyfin_web_posts() {
        assert_eq!(SubtitleMode::from_config("None"), SubtitleMode::None);
        assert_eq!(SubtitleMode::from_config("Always"), SubtitleMode::Always);
        assert_eq!(
            SubtitleMode::from_config("OnlyForced"),
            SubtitleMode::OnlyForced
        );
        assert_eq!(SubtitleMode::from_config("Smart"), SubtitleMode::Smart);
        assert_eq!(SubtitleMode::from_config("Default"), SubtitleMode::Default);
        assert_eq!(SubtitleMode::from_config("nonsense"), SubtitleMode::Default);
    }
}
