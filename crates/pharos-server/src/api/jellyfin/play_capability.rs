//! A play session's id as the credential for that playback's own URLs.
//!
//! `PlaybackInfo` (and the native grant) registers every negotiated playback
//! under a random PlaySessionId bound to one item and to the token that asked
//! (B75, B256). That id already authorized a tokenless `/stream?tag=`; here it
//! authorizes the transcode routes too (HLS playlists and segments, the
//! universal audio route, and the subtitle and font files a playback fetches),
//! so none of those URLs has to carry the caller's long-lived token as
//! `api_key` (017 T064, B1090, V1050).
//!
//! A capability is item-scoped, ends with the playback's stop, with its
//! holder's token, and six hours after negotiation, and each use re-checks that
//! the holder may still see the item.

use crate::api::jellyfin::auth_extractor::{self, MediaUser};
use crate::state::AppState;
use actix_web::{dev::Payload, error, web, FromRequest, HttpMessage, HttpRequest};
use pharos_core::{MediaId, PlaySessionId, User};
use std::future::Future;
use std::pin::Pin;

/// The query keys a capability arrives under: the native SDKs forward the
/// MediaSource `ETag` as `tag`; every transcode URL carries `PlaySessionId`.
const KEYS: [&str; 2] = ["tag", "PlaySessionId"];

/// Set on a request whose PlaySessionId is a live capability for its item and
/// user. URLs a playlist builds from such a request carry the PlaySessionId
/// and no `api_key`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CapabilityBound;

/// Whether [`CapabilityBound`] is set on `req`.
pub(crate) fn is_bound(req: &HttpRequest) -> bool {
    req.extensions().get::<CapabilityBound>().is_some()
}

/// What a capability check concluded. The label is a dashboard contract
/// (`pharos_play_capability_total{outcome}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Granted,
    /// No live session under that id (never minted, stopped, or expired).
    NoSession,
    /// A live session for another item.
    WrongItem,
    /// The holder's token was revoked, or their account disabled.
    HolderEnded,
    /// The session or token store could not answer.
    Unavailable,
}

impl Outcome {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Granted => "granted",
            Self::NoSession => "no_session",
            Self::WrongItem => "wrong_item",
            Self::HolderEnded => "holder_ended",
            Self::Unavailable => "unavailable",
        }
    }
}

fn count(outcome: Outcome) {
    metrics::counter!("pharos_play_capability_total", "outcome" => outcome.label()).increment(1);
}

/// The PlaySessionId values `req` names, `tag` first.
fn presented(req: &HttpRequest) -> impl Iterator<Item = String> + '_ {
    let qs = req.query_string();
    KEYS.into_iter().filter_map(move |key| {
        super::ci_query::values(qs, key)
            .find(|v| !v.is_empty())
            .map(str::to_owned)
    })
}

/// The user a capability in `req` speaks for on `media_id`: `Ok(None)` when
/// the request presents none that is live for this item, a 401 when the
/// holder's token has ended, a 503 when a store could not say. The caller
/// still checks the item against the user's library policy.
pub(crate) async fn capability_user(
    state: &AppState,
    req: &HttpRequest,
    media_id: MediaId,
) -> Result<Option<User>, actix_web::Error> {
    use crate::services::auth::{user_for_token_id, TokenRefusal};
    let mut outcome = None;
    for cap in presented(req) {
        let psid = PlaySessionId::from(cap.as_str());
        // The stored row, not this replica's cache: a stop on any replica
        // deletes it, and its age limit holds however often it is used (B256).
        let session = match state.transcode_sessions.get_stored(&psid).await {
            Ok(Some(session)) => session,
            Ok(None) => {
                outcome.get_or_insert(Outcome::NoSession);
                continue;
            }
            Err(e) => {
                count(Outcome::Unavailable);
                tracing::warn!(media.id = media_id, error = %e, "play capability: session store unavailable");
                return Err(error::ErrorServiceUnavailable("session store unavailable"));
            }
        };
        if session.media_id != media_id {
            outcome = Some(Outcome::WrongItem);
            tracing::info!(
                media.id = media_id,
                session.media_id = session.media_id,
                psid = %pharos_core::secret_prefix(psid.as_str()),
                "play capability: session is for another item"
            );
            continue;
        }
        return match user_for_token_id(&state.stores, &session.holder.token_id).await {
            Ok(user) if user.id == session.holder.user_id => {
                count(Outcome::Granted);
                Ok(Some(user))
            }
            Ok(user) => {
                count(Outcome::HolderEnded);
                tracing::info!(
                    media.id = media_id,
                    holder = %session.holder.user_id,
                    token_user = %user.id,
                    "play capability: holder's token now names another user"
                );
                Err(error::ErrorUnauthorized("session holder mismatch"))
            }
            Err(TokenRefusal::Unavailable(detail)) => {
                count(Outcome::Unavailable);
                tracing::warn!(media.id = media_id, error = %detail, "play capability: token store unavailable");
                Err(error::ErrorServiceUnavailable("token store unavailable"))
            }
            Err(refusal) => {
                count(Outcome::HolderEnded);
                tracing::info!(
                    media.id = media_id,
                    reason = refusal.label(),
                    "play capability: holder's token no longer live"
                );
                Err(error::ErrorUnauthorized("session ended"))
            }
        };
    }
    if let Some(outcome) = outcome {
        count(outcome);
        tracing::debug!(
            media.id = media_id,
            outcome = outcome.label(),
            "play capability refused"
        );
    }
    Ok(None)
}

/// How a playback request authenticated. The label is a dashboard contract
/// (`pharos_playback_credential_total{credential}`): `query` is a token in
/// the URL, which this module exists to retire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Credential {
    Header,
    Query,
    Cookie,
    Capability,
}

impl Credential {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Header => "header",
            Self::Query => "query",
            Self::Cookie => "cookie",
            Self::Capability => "capability",
        }
    }

    fn of_token(req: &HttpRequest) -> Self {
        if auth_extractor::extract_header_token(req).is_some() {
            Self::Header
        } else if auth_extractor::extract_token(req).is_some() {
            Self::Query
        } else {
            Self::Cookie
        }
    }
}

fn count_credential(credential: Credential) {
    metrics::counter!("pharos_playback_credential_total", "credential" => credential.label())
        .increment(1);
}

/// The item the route's `{id}` names.
fn path_media_id(req: &HttpRequest) -> Option<MediaId> {
    req.match_info()
        .get("id")
        .and_then(pharos_jellyfin_api::dto::parse_item_id)
}

/// Marks a token-authenticated playlist request whose PlaySessionId is a
/// session this same token negotiated for the item, so the URLs it embeds
/// drop the token. Reads the in-memory registry only: the token itself
/// authorized the request. A session another token holds (one since revoked,
/// say) stays unbound, so the playlist keeps the live token it was given.
async fn bind_if_live(state: &AppState, req: &HttpRequest, media_id: MediaId, user: &User) {
    let Some(cap) = super::ci_query::values(req.query_string(), "PlaySessionId")
        .find(|v| !v.is_empty())
        .map(str::to_owned)
    else {
        return;
    };
    let Some(token) = auth_extractor::media_token(Some(state), req).await else {
        return;
    };
    let token_id = pharos_core::TokenStore::token_id(&state.stores, &token);
    match state
        .transcode_sessions
        .get(PlaySessionId::from(cap.as_str()))
        .await
    {
        Ok(Some(s))
            if s.media_id == media_id
                && s.holder.user_id == user.id
                && s.holder.token_id == token_id =>
        {
            req.extensions_mut().insert(CapabilityBound);
        }
        Ok(_) => {}
        Err(e) => tracing::debug!(
            media.id = media_id,
            error = %e,
            "play capability: registry unavailable; the playlist keeps the token"
        ),
    }
}

/// [`MediaUser`] for a transcode route: a token from a header, `api_key` or a
/// cookie, else the PlaySessionId of a live playback of the route's item.
/// Each request counts under `pharos_playback_credential_total`.
pub struct PlaybackUser(pub User);

impl FromRequest for PlaybackUser {
    type Error = actix_web::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self, actix_web::Error>>>>;

    fn from_request(req: &HttpRequest, payload: &mut Payload) -> Self::Future {
        let state = req.app_data::<web::Data<AppState>>().cloned();
        let has_token = auth_extractor::extract_token(req).is_some()
            || req.cookie(auth_extractor::AUTH_COOKIE).is_some()
            || req.cookie(auth_extractor::MEDIA_COOKIE).is_some();
        let by_token = MediaUser::from_request(req, payload);
        let req = req.clone();
        Box::pin(async move {
            let media_id = path_media_id(&req);
            let mut by_token = Some(by_token);
            let mut refusal = None;
            if let Some(fut) = by_token.take_if(|_| has_token) {
                match fut.await {
                    Ok(MediaUser(user)) => {
                        count_credential(Credential::of_token(&req));
                        let is_playlist = req.match_pattern().is_some_and(|p| p.ends_with(".m3u8"));
                        if let (true, Some(state), Some(id)) = (is_playlist, &state, media_id) {
                            bind_if_live(state, &req, id, &user).await;
                        }
                        return Ok(PlaybackUser(user));
                    }
                    // A dead token may still ride with a live capability.
                    Err(e)
                        if e.as_response_error().status_code()
                            != actix_web::http::StatusCode::UNAUTHORIZED =>
                    {
                        return Err(e);
                    }
                    Err(e) => refusal = Some(e),
                }
            }
            if let (Some(state), Some(id)) = (&state, media_id) {
                if let Some(user) = capability_user(state, &req, id).await? {
                    req.extensions_mut().insert(CapabilityBound);
                    count_credential(Credential::Capability);
                    return Ok(PlaybackUser(user));
                }
            }
            match (refusal, by_token) {
                (Some(e), _) => Err(e),
                // No credential at all: the token path's own 401 and rejection count.
                (None, Some(fut)) => fut.await.map(|MediaUser(user)| PlaybackUser(user)),
                (None, None) => Err(error::ErrorUnauthorized("missing token")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_labels_are_distinct() {
        let all = [
            Outcome::Granted,
            Outcome::NoSession,
            Outcome::WrongItem,
            Outcome::HolderEnded,
            Outcome::Unavailable,
        ];
        let labels: std::collections::BTreeSet<_> = all.iter().map(|o| o.label()).collect();
        assert_eq!(labels.len(), all.len());
    }

    #[test]
    fn credential_labels_are_distinct() {
        let all = [
            Credential::Header,
            Credential::Query,
            Credential::Cookie,
            Credential::Capability,
        ];
        let labels: std::collections::BTreeSet<_> = all.iter().map(|c| c.label()).collect();
        assert_eq!(labels.len(), all.len());
    }
}
