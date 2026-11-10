//! A user's own devices: which devices they are signed in on, and the
//! sound formats each is sent converted (T130, ADR-0095). Both the Jellyfin
//! `/Devices` endpoints and the native `Session.myDevices` /
//! `Session.setDeviceConvertedAudio` decide here, so an owner may do the same
//! over either API.

use pharos_core::{TokenStore as _, UserId};

use crate::services::playback::{
    denied_audio_codecs, denied_audio_codecs_key, normalise_denied_audio_codecs, DeniedCodecsError,
};
use crate::state::AppState;

/// A device a user is signed in on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedDevice {
    pub id: String,
    /// The name an admin or the owner gave it, else its id.
    pub name: String,
    /// The audio codecs it is sent converted, normalised and sorted.
    pub denied_audio_codecs: Vec<String>,
}

/// Why a device's sound formats could not be set.
#[derive(Debug, thiserror::Error)]
pub enum SetConvertedAudioError {
    /// The caller is not signed in on the device.
    #[error("not signed in on device {0}")]
    NotSignedIn(String),
    #[error(transparent)]
    Refused(#[from] DeniedCodecsError),
    #[error("store unavailable: {0}")]
    Store(String),
}

/// The name shown for `device_id`: the stored custom name, else the id.
pub async fn display_name(state: &AppState, device_id: &str) -> String {
    use pharos_store_sqlx::ServerConfigStore as _;
    state
        .stores
        .load_named_config(&format!("devname:{device_id}"))
        .await
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| device_id.to_string())
}

/// Whether `user` holds a token issued to `device_id`. A media or manage
/// token is a child of a session, not a device of its own, so it never
/// counts.
pub async fn is_signed_in_on(
    state: &AppState,
    user: UserId,
    device_id: &str,
) -> Result<bool, String> {
    if crate::api::pharos::rpc::is_child_token_device(device_id) {
        return Ok(false);
    }
    let tokens = state
        .stores
        .tokens_for(user)
        .await
        .map_err(|e| e.to_string())?;
    Ok(tokens.iter().any(|t| t.device_id == device_id))
}

/// The devices `user` is signed in on, once each, in the order first seen.
pub async fn devices_of(state: &AppState, user: UserId) -> Result<Vec<OwnedDevice>, String> {
    let tokens = state
        .stores
        .tokens_for(user)
        .await
        .map_err(|e| e.to_string())?;
    let mut out: Vec<OwnedDevice> = Vec::new();
    for t in tokens {
        if crate::api::pharos::rpc::is_child_token_device(&t.device_id)
            || out.iter().any(|d| d.id == t.device_id)
        {
            continue;
        }
        out.push(OwnedDevice {
            name: display_name(state, &t.device_id).await,
            denied_audio_codecs: denied_audio_codecs(state, &t.device_id).await,
            id: t.device_id,
        });
    }
    Ok(out)
}

/// Sets the sound formats `device_id` is sent converted, for `user`, who must
/// be signed in on it. Answers the list as stored.
pub async fn set_own_converted_audio(
    state: &AppState,
    user: UserId,
    user_name: &str,
    device_id: &str,
    codecs: &[String],
) -> Result<Vec<String>, SetConvertedAudioError> {
    use pharos_store_sqlx::ServerConfigStore as _;
    if !is_signed_in_on(state, user, device_id)
        .await
        .map_err(SetConvertedAudioError::Store)?
    {
        tracing::info!(
            device.id = device_id,
            by.user = user_name,
            "device options refused: not an admin and not signed in on the device"
        );
        return Err(SetConvertedAudioError::NotSignedIn(device_id.to_string()));
    }
    let denied = normalise_denied_audio_codecs(codecs).inspect_err(|e| {
        tracing::info!(device.id = device_id, error = %e, "device audio deny list refused");
    })?;
    let json =
        serde_json::to_string(&denied).map_err(|e| SetConvertedAudioError::Store(e.to_string()))?;
    state
        .stores
        .set_named_config(&denied_audio_codecs_key(device_id), &json)
        .await
        .map_err(|e| SetConvertedAudioError::Store(e.to_string()))?;
    tracing::info!(
        device.id = device_id,
        by.user = user_name,
        by.role = "owner",
        denied = ?denied,
        "device audio deny list set"
    );
    Ok(denied)
}
