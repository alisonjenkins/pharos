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

/// Give a newly created user the server's default track preferences.
///
/// Jellyfin has no server-wide default — every account starts at the stock
/// values and each person re-picks them by hand, which on a shared server
/// means everyone hits the same wrong-language playback until they do. A
/// stock `[user_defaults]` writes nothing at all, so the row only exists when
/// the operator actually chose something; the user can still change any of it
/// afterwards, and their own write simply replaces this one.
///
/// Best-effort: a failure here must not fail user creation. The account is
/// already created at this point, and a missing preferences row is exactly
/// what every account had before this existed.
pub async fn seed_default_configuration(
    stores: &crate::state::Stores,
    defaults: &crate::config::UserDefaultsConfig,
    user: UserId,
) {
    if defaults.is_stock() {
        return;
    }
    let cfg = defaults.to_configuration();
    let json = match serde_json::to_string(&cfg) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!(user.id = %user, error = %e, "could not encode default user configuration");
            return;
        }
    };
    match stores.set_user_configuration(user, &json).await {
        Ok(()) => tracing::info!(
            user.id = %user,
            audio_language_preference = %defaults.audio_language_preference,
            subtitle_mode = %defaults.subtitle_mode,
            play_default_audio_track = defaults.play_default_audio_track,
            "seeded a new user with the server's default track preferences"
        ),
        Err(e) => tracing::warn!(
            user.id = %user,
            error = %e,
            "could not store the default track preferences for a new user — \
             they start at Jellyfin's defaults"
        ),
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
#[allow(clippy::unwrap_used, clippy::expect_used)]
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

    /// A stock `[user_defaults]` must write nothing: an operator who has not
    /// configured anything should leave no trace on new accounts, and an
    /// absent row is what every account had before this existed.
    #[tokio::test]
    async fn stock_defaults_write_no_configuration_at_all() {
        use pharos_core::{PreferenceStore, SecretString, UserPolicy, UserRecord, UserStore};
        let stores = crate::state::Stores::connect("sqlite::memory:")
            .await
            .expect("store");
        let uid = UserId::new();
        stores
            .create(UserRecord {
                id: uid,
                name: "u".into(),
                password_hash: SecretString::new("h"),
                policy: UserPolicy::default(),
            })
            .await
            .expect("create");

        seed_default_configuration(&stores, &crate::config::UserDefaultsConfig::default(), uid)
            .await;

        assert_eq!(
            stores.get_user_configuration(uid).await.expect("read"),
            None
        );
    }

    /// A configured default lands as a real stored configuration, in the
    /// shape jellyfin-web reads back.
    #[tokio::test]
    async fn a_configured_default_is_stored_for_a_new_user() {
        use pharos_core::{PreferenceStore, SecretString, UserPolicy, UserRecord, UserStore};
        let stores = crate::state::Stores::connect("sqlite::memory:")
            .await
            .expect("store");
        let uid = UserId::new();
        stores
            .create(UserRecord {
                id: uid,
                name: "u".into(),
                password_hash: SecretString::new("h"),
                policy: UserPolicy::default(),
            })
            .await
            .expect("create");

        let defaults = crate::config::UserDefaultsConfig {
            audio_language_preference: "OriginalLanguage".into(),
            subtitle_language_preference: "eng".into(),
            subtitle_mode: "Smart".into(),
            play_default_audio_track: false,
        };
        seed_default_configuration(&stores, &defaults, uid).await;

        let raw = stores
            .get_user_configuration(uid)
            .await
            .expect("read")
            .expect("a configuration was written");
        let cfg: UserConfigurationDto = serde_json::from_str(&raw).expect("parses");
        assert_eq!(cfg.audio_language_preference, "OriginalLanguage");
        assert_eq!(cfg.subtitle_mode, "Smart");
        assert!(!cfg.play_default_audio_track);

        // And it resolves into the preferences selection actually uses.
        let prefs = TrackPreference::from_configuration(&cfg);
        assert!(prefs.audio_original_language);
        assert_eq!(prefs.subtitle_mode, SubtitleMode::Smart);
        assert!(!prefs.prefer_default_audio_track);
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
