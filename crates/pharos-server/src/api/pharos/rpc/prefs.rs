//! `Prefs`: a signed-in user's own key/value preferences. Only keys on
//! [`ALLOWED_KEYS`] exist, and a value is at most [`MAX_PREF_VALUE_BYTES`], so
//! a client cannot grow a user's row set or a row without bound. A refused
//! call is logged with the key and the size, never the value.

use std::rc::Rc;

use pharos_api::pharos_capnp::prefs as wire;
use pharos_api::{redact, server, Denial};
use pharos_core::UserPrefStore;

use super::session::Session;
use crate::services::metadata_text::set_language_setting;
use crate::services::track_settings::{get_setting, set_setting, TrackSetting};
use crate::services::user_config::SetFieldError;

/// The pref that says whether this user's clients may send diagnostics.
/// Value bytes `true` or `false`; absent means off.
pub(super) const DIAGNOSTICS_OPT_IN: &str = "diagnostics.opt_in";

/// The language show and episode descriptions are shown in (ADR-0062): a
/// three-letter code, `OriginalLanguage`, or empty for the default. Not a row
/// of its own: it reads and writes `DescriptionLanguagePreference` in the
/// user's Jellyfin configuration, so the native UI and jellyfin-web set the
/// same thing.
pub(super) const DESCRIPTION_LANGUAGE: &str = "description.language";

/// The playback track settings (spec 038 FR-702), like [`DESCRIPTION_LANGUAGE`]
/// fields of the user's Jellyfin configuration: see
/// [`crate::services::track_settings`].
const PLAYBACK_AUDIO_LANGUAGE: &str = "playback.audio_language";
const PLAYBACK_SUBTITLE_LANGUAGE: &str = "playback.subtitle_language";
const PLAYBACK_SUBTITLE_MODE: &str = "playback.subtitle_mode";

/// The newest "what's new" entry this user has been shown (spec 027,
/// ADR-0065): a decimal number of at most [`MAX_WHATS_NEW_DIGITS`] digits.
/// Absent means the user has seen none.
pub(super) const WHATS_NEW_SEEN: &str = "whats_new.seen";

/// The user's saved mixes (spec 024, B902): one capnp `SavedMixes` message
/// the client writes whole. Must match `pharos_client::mix::SAVED_KEY`.
pub(super) const MIX_SAVED: &str = "mix.saved";

/// Digits a [`WHATS_NEW_SEEN`] value may hold: room for a u32 counter.
const MAX_WHATS_NEW_DIGITS: usize = 9;

/// Keys a client may read and write; any other key is refused.
const ALLOWED_KEYS: &[&str] = &[
    DIAGNOSTICS_OPT_IN,
    DESCRIPTION_LANGUAGE,
    PLAYBACK_AUDIO_LANGUAGE,
    PLAYBACK_SUBTITLE_LANGUAGE,
    PLAYBACK_SUBTITLE_MODE,
    WHATS_NEW_SEEN,
    MIX_SAVED,
];

/// Largest value one pref may hold: a toggle or a small settings document,
/// never a blob, and a bound on what one user can make the database keep.
pub const MAX_PREF_VALUE_BYTES: usize = 4096;

/// Longest rejected key written to a log line.
const MAX_LOGGED_KEY_BYTES: usize = 128;

pub(super) struct Prefs {
    session: Rc<Session>,
}

impl Prefs {
    pub(super) fn new(session: Rc<Session>) -> Self {
        Self { session }
    }
}

/// The key as an allowed `&'static str`, or why it is refused (logged).
fn allowed_key(
    method: &'static str,
    key: capnp::text::Reader<'_>,
    size: usize,
) -> Result<&'static str, Denial> {
    let allowed = key
        .to_str()
        .ok()
        .and_then(|k| ALLOWED_KEYS.iter().find(|a| **a == k).copied());
    allowed.ok_or_else(|| {
        tracing::warn!(
            method,
            key = %redact::bounded_text(key, MAX_LOGGED_KEY_BYTES),
            size,
            "pref refused: unknown key"
        );
        Denial::NotPermitted {
            what: "unknown pref key",
        }
    })
}

fn store_denial(method: &'static str, key: &'static str, error: &dyn std::fmt::Display) -> Denial {
    tracing::warn!(method, key, error = %error, "pref store unavailable");
    Denial::Unavailable {
        dependency: "pref store",
    }
}

impl server::prefs::Handler for Prefs {
    async fn get(
        self: Rc<Self>,
        params: wire::GetParams,
        results: &mut wire::GetResults,
    ) -> Result<(), capnp::Error> {
        let key = params.get()?.get_key()?;
        let mut out = results.get().init_result();
        match self.read(key).await {
            Ok(Some(value)) => out.init_value(value.len() as u32).copy_from_slice(&value),
            Ok(None) => out.set_not_set(()),
            Err(denial) => denial.write(out.init_error()),
        }
        Ok(())
    }

    async fn set(
        self: Rc<Self>,
        params: wire::SetParams,
        results: &mut wire::SetResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let (key, value) = (params.get_key()?, params.get_value()?);
        let mut out = results.get().init_result();
        match self.write(key, value).await {
            Ok(()) => out.set_ok(()),
            Err(denial) => denial.write(out.init_error()),
        }
        Ok(())
    }
}

impl Prefs {
    async fn read(&self, key: capnp::text::Reader<'_>) -> Result<Option<Vec<u8>>, Denial> {
        let user = self.session.current_user().await?;
        let key = allowed_key("get", key, 0)?;
        if key == DESCRIPTION_LANGUAGE {
            let stores = &self.session.state.stores;
            let value = crate::services::metadata_text::language_setting(stores, user.id)
                .await
                .map_err(|e| store_denial("get", key, &e))?;
            return Ok((!value.is_empty()).then(|| value.into_bytes()));
        }
        if let Some(setting) = TrackSetting::from_key(key) {
            let value = get_setting(&self.session.state.stores, user.id, setting)
                .await
                .map_err(|e| store_denial("get", key, &e))?;
            return Ok((!value.is_empty()).then(|| value.into_bytes()));
        }
        self.session
            .state
            .stores
            .get_user_pref(user.id, key)
            .await
            .map_err(|e| store_denial("get", key, &e))
    }

    async fn write(&self, key: capnp::text::Reader<'_>, value: &[u8]) -> Result<(), Denial> {
        let user = self.session.current_user().await?;
        let key = allowed_key("set", key, value.len())?;
        if value.len() > MAX_PREF_VALUE_BYTES {
            tracing::warn!(
                key,
                size = value.len(),
                max = MAX_PREF_VALUE_BYTES,
                "pref refused: value too large"
            );
            return Err(Denial::Limit {
                name: "pref_value_bytes",
                value: value.len() as u64,
                max: MAX_PREF_VALUE_BYTES as u64,
            });
        }
        let setting = TrackSetting::from_key(key);
        if key == DESCRIPTION_LANGUAGE || setting.is_some() {
            let text =
                std::str::from_utf8(value).map_err(|_| Denial::Invalid { field: "value" })?;
            let stores = &self.session.state.stores;
            let stored = match setting {
                Some(setting) => set_setting(stores, user.id, setting, text).await,
                None => set_language_setting(stores, user.id, text).await,
            };
            return stored.map_err(|e| match e {
                SetFieldError::Invalid => {
                    tracing::warn!(
                        key,
                        size = value.len(),
                        "pref refused: not an accepted value"
                    );
                    Denial::Invalid { field: "value" }
                }
                SetFieldError::Unreadable => {
                    store_denial("set", key, &"stored configuration is not a JSON object")
                }
                SetFieldError::Store(e) => store_denial("set", key, &e),
            });
        }
        if key == WHATS_NEW_SEEN && !is_whats_new_entry(value) {
            tracing::warn!(key, size = value.len(), "pref refused: not an entry number");
            return Err(Denial::Invalid { field: "value" });
        }
        self.session
            .state
            .stores
            .set_user_pref(user.id, key, value)
            .await
            .map_err(|e| store_denial("set", key, &e))?;
        if key == DIAGNOSTICS_OPT_IN {
            self.session.forget_opt_in();
        }
        Ok(())
    }
}

/// Whether `value` is a [`WHATS_NEW_SEEN`] number: ASCII digits only.
fn is_whats_new_entry(value: &[u8]) -> bool {
    (1..=MAX_WHATS_NEW_DIGITS).contains(&value.len()) && value.iter().all(u8::is_ascii_digit)
}
