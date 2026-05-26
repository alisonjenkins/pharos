//! Auth extractor — pulls a token from any of the headers Jellyfin clients
//! use, resolves it via the configured `TokenStore`, and yields a `User`.
//!
//! Recognized headers (matches Jellyfin server behaviour):
//! - `X-Emby-Token: <token>`
//! - `X-MediaBrowser-Token: <token>`
//! - `X-Emby-Authorization: MediaBrowser Token="<token>", …`
//! - `Authorization: MediaBrowser Token="<token>", …`

use crate::state::AppState;
use actix_web::{
    dev::Payload, error::ErrorUnauthorized, error::ErrorInternalServerError, web, FromRequest,
    HttpRequest,
};
use pharos_core::{TokenStore, User, UserStore};
use std::{future::Future, pin::Pin};

pub struct AuthUser(pub User);

impl FromRequest for AuthUser {
    type Error = actix_web::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self, actix_web::Error>>>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        let token = extract_token(req);
        let state = req.app_data::<web::Data<AppState>>().cloned();
        Box::pin(async move {
            let token = token.ok_or_else(|| ErrorUnauthorized("missing token"))?;
            let state = state
                .ok_or_else(|| ErrorInternalServerError("AppState not configured"))?;
            let uid = state
                .stores
                .resolve(&token)
                .await
                .map_err(|_| ErrorUnauthorized("invalid token"))?;
            let record = state
                .stores
                .get(uid)
                .await
                .map_err(|_| ErrorUnauthorized("user revoked"))?;
            Ok(AuthUser(record.into_user()))
        })
    }
}

/// Public for tests + handler-side use that need the raw token string.
///
/// Lookup order: `X-Emby-Token` → `X-MediaBrowser-Token` →
/// MediaBrowser/Emby `Authorization` parse → `api_key` query param (used
/// by media-element `src=` playback where headers can't be injected).
pub fn extract_token(req: &HttpRequest) -> Option<String> {
    let h = req.headers();

    if let Some(v) = h.get("X-Emby-Token").and_then(|v| v.to_str().ok()) {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    if let Some(v) = h
        .get("X-MediaBrowser-Token")
        .and_then(|v| v.to_str().ok())
    {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    for name in ["X-Emby-Authorization", "Authorization"] {
        if let Some(v) = h.get(name).and_then(|v| v.to_str().ok()) {
            if let Some(tok) = parse_mediabrowser_token(v) {
                return Some(tok);
            }
        }
    }
    for (k, v) in req.query_string().split('&').filter_map(|kv| kv.split_once('=')) {
        if (k.eq_ignore_ascii_case("api_key") || k.eq_ignore_ascii_case("ApiKey"))
            && !v.is_empty()
        {
            return Some(v.to_string());
        }
    }
    None
}

/// Parse `MediaBrowser Token="abc", Client="x", …` style headers.
fn parse_mediabrowser_token(value: &str) -> Option<String> {
    let after = value.strip_prefix("MediaBrowser").or_else(|| value.strip_prefix("Emby"))?;
    for part in after.split(',') {
        let part = part.trim();
        let Some((k, raw)) = part.split_once('=') else {
            continue;
        };
        if k.trim().eq_ignore_ascii_case("Token") {
            let v = raw.trim().trim_matches('"').trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use actix_web::test::TestRequest;

    #[test]
    fn parses_x_emby_token_header() {
        let req = TestRequest::default()
            .insert_header(("X-Emby-Token", "raw-token-value"))
            .to_http_request();
        assert_eq!(extract_token(&req).as_deref(), Some("raw-token-value"));
    }

    #[test]
    fn parses_mediabrowser_authorization_header() {
        let req = TestRequest::default()
            .insert_header((
                "Authorization",
                r#"MediaBrowser Client="Finamp", Device="iPhone", Token="xyz123", Version="1.0""#,
            ))
            .to_http_request();
        assert_eq!(extract_token(&req).as_deref(), Some("xyz123"));
    }

    #[test]
    fn parses_emby_legacy_authorization_header() {
        let req = TestRequest::default()
            .insert_header(("X-Emby-Authorization", r#"Emby Token="abcd""#))
            .to_http_request();
        assert_eq!(extract_token(&req).as_deref(), Some("abcd"));
    }

    #[test]
    fn returns_none_when_no_recognized_header() {
        let req = TestRequest::default().to_http_request();
        assert!(extract_token(&req).is_none());
    }

    #[test]
    fn returns_none_for_mediabrowser_without_token() {
        let req = TestRequest::default()
            .insert_header(("Authorization", r#"MediaBrowser Client="x""#))
            .to_http_request();
        assert!(extract_token(&req).is_none());
    }

    #[test]
    fn parses_api_key_query_param() {
        let req = TestRequest::default()
            .uri("/Videos/123/stream?static=true&api_key=abc123&MediaSourceId=xyz")
            .to_http_request();
        assert_eq!(extract_token(&req).as_deref(), Some("abc123"));
    }

    #[test]
    fn header_token_wins_over_query_api_key() {
        let req = TestRequest::default()
            .uri("/Videos/123/stream?api_key=query-token")
            .insert_header(("X-Emby-Token", "header-token"))
            .to_http_request();
        assert_eq!(extract_token(&req).as_deref(), Some("header-token"));
    }
}
