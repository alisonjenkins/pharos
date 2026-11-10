//! The playback track settings of the native Settings page, kept in the
//! user's Jellyfin configuration (`AudioLanguagePreference`,
//! `SubtitleLanguagePreference`, `SubtitleMode`) so jellyfin-web and the
//! native UI read and write the same fields and track selection
//! (`user_prefs::TrackPreference`) reads them unchanged.

use pharos_core::{PreferenceStore, UserId};

use crate::metadata_language::{normalize, ORIGINAL};
use crate::services::user_config::{get_string_field, set_string_field, SetFieldError};

/// One configuration field the native settings page edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackSetting {
    AudioLanguage,
    SubtitleLanguage,
    SubtitleMode,
}

/// The `SubtitleMode` values `stream_select::SubtitleMode::from_config`
/// knows, in the casing jellyfin-web posts.
const SUBTITLE_MODES: [&str; 5] = ["Default", "Always", "OnlyForced", "None", "Smart"];

impl TrackSetting {
    /// The setting a pref key names, if it names one.
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "playback.audio_language" => Some(Self::AudioLanguage),
            "playback.subtitle_language" => Some(Self::SubtitleLanguage),
            "playback.subtitle_mode" => Some(Self::SubtitleMode),
            _ => None,
        }
    }

    fn field(self) -> &'static str {
        match self {
            Self::AudioLanguage => "AudioLanguagePreference",
            Self::SubtitleLanguage => "SubtitleLanguagePreference",
            Self::SubtitleMode => "SubtitleMode",
        }
    }

    /// `raw` in the form stored, or `None` when this setting does not accept
    /// it. Languages are the three-letter form jellyfin-web uses and empty
    /// means no preference (English is a real choice here, unlike for
    /// descriptions).
    fn canonical(self, raw: &str) -> Option<String> {
        let raw = raw.trim();
        match self {
            Self::AudioLanguage | Self::SubtitleLanguage => {
                if raw.is_empty() {
                    return Some(String::new());
                }
                if self == Self::AudioLanguage && raw.eq_ignore_ascii_case(ORIGINAL) {
                    return Some(ORIGINAL.to_string());
                }
                normalize(raw)
            }
            Self::SubtitleMode => SUBTITLE_MODES
                .iter()
                .find(|m| m.eq_ignore_ascii_case(raw))
                .map(|m| (*m).to_string()),
        }
    }
}

/// The stored value of `setting`, in the form the settings page offers;
/// empty when unset. A value written unchecked by a Jellyfin client that
/// names nothing valid is returned as stored.
pub async fn get_setting<S: PreferenceStore>(
    store: &S,
    user: UserId,
    setting: TrackSetting,
) -> Result<String, String> {
    let stored = get_string_field(store, user, setting.field()).await?;
    Ok(setting.canonical(&stored).unwrap_or(stored))
}

/// Store `value` as `setting` of `user`, keeping the rest of their
/// configuration.
pub async fn set_setting<S: PreferenceStore>(
    store: &S,
    user: UserId,
    setting: TrackSetting,
    value: &str,
) -> Result<(), SetFieldError> {
    let value = setting.canonical(value).ok_or(SetFieldError::Invalid)?;
    set_string_field(store, user, setting.field(), &value).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_are_stored_in_the_three_letter_form() {
        let a = TrackSetting::AudioLanguage;
        assert_eq!(a.canonical("fr").as_deref(), Some("fra"));
        assert_eq!(a.canonical("eng").as_deref(), Some("eng"));
        assert_eq!(a.canonical("").as_deref(), Some(""));
        assert_eq!(
            a.canonical("originallanguage").as_deref(),
            Some("OriginalLanguage")
        );
        assert_eq!(a.canonical("not a language"), None);
    }

    #[test]
    fn only_the_audio_language_may_be_the_titles_original() {
        assert_eq!(
            TrackSetting::SubtitleLanguage.canonical("OriginalLanguage"),
            None
        );
    }

    #[test]
    fn subtitle_mode_takes_only_the_modes_selection_knows() {
        let m = TrackSetting::SubtitleMode;
        for mode in SUBTITLE_MODES {
            assert_eq!(m.canonical(mode).as_deref(), Some(mode));
            assert_eq!(
                crate::api::jellyfin::stream_select::SubtitleMode::from_config(mode)
                    == crate::api::jellyfin::stream_select::SubtitleMode::Default,
                mode == "Default",
                "{mode} must be a mode selection really parses"
            );
        }
        assert_eq!(m.canonical("onlyforced").as_deref(), Some("OnlyForced"));
        assert_eq!(m.canonical(""), None);
        assert_eq!(m.canonical("Sometimes"), None);
    }

    #[test]
    fn keys_map_to_their_settings() {
        assert_eq!(
            TrackSetting::from_key("playback.subtitle_mode"),
            Some(TrackSetting::SubtitleMode)
        );
        assert_eq!(TrackSetting::from_key("playback.other"), None);
    }
}
