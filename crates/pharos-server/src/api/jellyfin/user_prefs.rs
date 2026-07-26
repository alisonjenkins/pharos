//! The track preferences a user configured, in the form selection needs them.
//!
//! jellyfin-web writes these to `/Users/{id}/Configuration` and pharos has
//! stored them since that endpoint existed — as an opaque JSON blob nothing
//! ever read. This turns that blob into the arguments
//! [`crate::api::jellyfin::stream_select`] takes, which is the whole of the
//! gap between "the setting is saved" and "the setting does something".
//!
//! A user who has never opened those settings gets Jellyfin's defaults: no
//! language preference (so the container decides), `PlayDefaultAudioTrack` on,
//! and `SubtitleMode` Default.

use crate::state::AppState;
use pharos_core::{PreferenceStore, UserId};
use pharos_jellyfin_api::dto::UserConfigurationDto;

use super::stream_select::{normalize_language, SubtitleMode};

/// Everything selection needs about one user, resolved once per request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackPreference {
    /// Preferred audio languages, expanded to the tags containers really
    /// carry (`eng` also matches `en`). Empty means "no preference".
    pub audio_languages: Vec<String>,
    /// Preferred subtitle languages, same expansion.
    pub subtitle_languages: Vec<String>,
    pub subtitle_mode: SubtitleMode,
    /// When set, a container's default-flagged audio track wins over the
    /// language preference. Jellyfin's default is ON, and it is why a
    /// preference alone does not move a title whose dub is flagged default.
    pub prefer_default_audio_track: bool,
    /// Whether a track the user picked before should be re-selected.
    pub remember_audio: bool,
    pub remember_subtitle: bool,
    /// `AudioLanguagePreference` was the literal `OriginalLanguage`, meaning
    /// "whatever language this title was made in" rather than a fixed one.
    pub audio_original_language: bool,
}

impl Default for TrackPreference {
    fn default() -> Self {
        Self {
            audio_languages: Vec::new(),
            subtitle_languages: Vec::new(),
            subtitle_mode: SubtitleMode::Default,
            prefer_default_audio_track: true,
            remember_audio: true,
            remember_subtitle: true,
            audio_original_language: false,
        }
    }
}

impl TrackPreference {
    pub fn from_configuration(cfg: &UserConfigurationDto) -> Self {
        let original = cfg
            .audio_language_preference
            .eq_ignore_ascii_case("originallanguage");
        Self {
            audio_languages: if original {
                // Resolved per item from the title's own original language;
                // with none recorded this degrades to "no preference", which
                // is the container's own order rather than a wrong language.
                Vec::new()
            } else {
                normalize_language(&cfg.audio_language_preference)
            },
            subtitle_languages: normalize_language(&cfg.subtitle_language_preference),
            subtitle_mode: SubtitleMode::from_config(&cfg.subtitle_mode),
            prefer_default_audio_track: cfg.play_default_audio_track,
            remember_audio: cfg.remember_audio_selections,
            remember_subtitle: cfg.remember_subtitle_selections,
            audio_original_language: original,
        }
    }

    /// The audio languages to rank by for one item, given the original
    /// language recorded for it (`None` when unknown or not requested).
    pub fn audio_languages_for(&self, original_language: Option<&str>) -> Vec<String> {
        match (self.audio_original_language, original_language) {
            (true, Some(l)) => normalize_language(l),
            _ => self.audio_languages.clone(),
        }
    }
}

/// Load one user's track preferences. A missing or unparseable configuration
/// yields the defaults rather than an error — a corrupt blob must not stop
/// playback, and the defaults are what an unconfigured user gets anyway.
pub async fn track_preference(state: &AppState, user: UserId) -> TrackPreference {
    let raw = match state.stores.get_user_configuration(user).await {
        Ok(Some(raw)) => raw,
        Ok(None) => return TrackPreference::default(),
        Err(e) => {
            tracing::warn!(
                user.id = %user,
                error = %e,
                "could not read user configuration — track selection falls back to \
                 the container's own order"
            );
            return TrackPreference::default();
        }
    };
    match serde_json::from_str::<UserConfigurationDto>(&raw) {
        Ok(cfg) => TrackPreference::from_configuration(&cfg),
        Err(e) => {
            tracing::warn!(
                user.id = %user,
                error = %e,
                "stored user configuration did not parse — track selection falls back \
                 to the container's own order"
            );
            TrackPreference::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unconfigured_user_gets_jellyfins_defaults() {
        let p = TrackPreference::from_configuration(&UserConfigurationDto::default());
        assert!(p.audio_languages.is_empty());
        assert!(p.prefer_default_audio_track, "Jellyfin defaults this ON");
        assert_eq!(p.subtitle_mode, SubtitleMode::Default);
        assert!(!p.audio_original_language);
    }

    #[test]
    fn a_configured_language_expands_to_the_tags_files_carry() {
        let cfg = UserConfigurationDto {
            audio_language_preference: "eng".into(),
            subtitle_language_preference: "jpn".into(),
            subtitle_mode: "Smart".into(),
            play_default_audio_track: false,
            ..Default::default()
        };
        let p = TrackPreference::from_configuration(&cfg);
        assert_eq!(p.audio_languages, vec!["en", "eng"]);
        assert_eq!(p.subtitle_languages, vec!["ja", "jpn"]);
        assert_eq!(p.subtitle_mode, SubtitleMode::Smart);
        assert!(!p.prefer_default_audio_track);
    }

    /// `OriginalLanguage` is not a language — it defers to the item, so it
    /// must NOT be normalized into a bogus tag list.
    #[test]
    fn original_language_defers_to_the_item() {
        let cfg = UserConfigurationDto {
            audio_language_preference: "OriginalLanguage".into(),
            ..Default::default()
        };
        let p = TrackPreference::from_configuration(&cfg);
        assert!(p.audio_original_language);
        assert!(p.audio_languages.is_empty());
        assert_eq!(p.audio_languages_for(Some("jpn")), vec!["ja", "jpn"]);
        // Nothing recorded for the item → no preference, not a wrong one.
        assert!(p.audio_languages_for(None).is_empty());
    }

    #[test]
    fn a_fixed_preference_ignores_the_items_original_language() {
        let cfg = UserConfigurationDto {
            audio_language_preference: "eng".into(),
            ..Default::default()
        };
        let p = TrackPreference::from_configuration(&cfg);
        assert_eq!(p.audio_languages_for(Some("jpn")), vec!["en", "eng"]);
    }
}
