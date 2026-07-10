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
    dev::Payload, error::ErrorInternalServerError, error::ErrorUnauthorized, web, FromRequest,
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
            let state = state.ok_or_else(|| ErrorInternalServerError("AppState not configured"))?;
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

/// Like [`AuthUser`] but also yields the client's `deviceId` — the session key
/// the SyncPlay [`SessionHub`](pharos_sync::SessionHub) uses to route an HTTP
/// `/SyncPlay/*` command to the caller's `/socket`. The device id comes from the
/// Emby-Authorization header (`DeviceId="…"`), falling back to the `deviceId`
/// query param or the `X-Emby-Device-Id` header (all carry the same value a
/// jellyfin-web client also puts on its `/socket` URL).
pub struct AuthSession {
    pub user: User,
    pub device_id: Option<String>,
}

impl FromRequest for AuthSession {
    type Error = actix_web::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self, actix_web::Error>>>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        let token = extract_token(req);
        let device_id = device_id_from_request(req);
        let state = req.app_data::<web::Data<AppState>>().cloned();
        Box::pin(async move {
            let token = token.ok_or_else(|| ErrorUnauthorized("missing token"))?;
            let state = state.ok_or_else(|| ErrorInternalServerError("AppState not configured"))?;
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
            Ok(AuthSession {
                user: record.into_user(),
                device_id,
            })
        })
    }
}

/// Extract the client's device id from any place Jellyfin clients carry it:
/// the Emby-Authorization header, the `deviceId` query param, or the
/// `X-Emby-Device-Id` header.
pub fn device_id_from_request(req: &HttpRequest) -> Option<String> {
    if let Some(id) = auth_header_from_request(req).device_id {
        return Some(id);
    }
    for (k, v) in req
        .query_string()
        .split('&')
        .filter_map(|kv| kv.split_once('='))
    {
        if k.eq_ignore_ascii_case("deviceid") && !v.is_empty() {
            return Some(v.to_string());
        }
    }
    req.headers()
        .get("X-Emby-Device-Id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
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
    if let Some(v) = h.get("X-MediaBrowser-Token").and_then(|v| v.to_str().ok()) {
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
    for (k, v) in req
        .query_string()
        .split('&')
        .filter_map(|kv| kv.split_once('='))
    {
        if (k.eq_ignore_ascii_case("api_key") || k.eq_ignore_ascii_case("ApiKey")) && !v.is_empty()
        {
            return Some(v.to_string());
        }
    }
    // P24 — last-resort cookie fallback. Stream endpoints set a
    // `JellyfinAuth` cookie after the first authenticated request so
    // `<video src=…>` (which can't inject headers, and on Safari /
    // tvOS can't trust query strings under strict CSP) keeps working.
    if let Some(cookie_hdr) = h
        .get(actix_web::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
    {
        for entry in cookie_hdr.split(';') {
            let entry = entry.trim();
            if let Some((name, value)) = entry.split_once('=') {
                if name.trim().eq_ignore_ascii_case("JellyfinAuth") {
                    let v = value.trim();
                    if !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
        }
    }
    None
}

/// P24 — Set-Cookie value emitted when a media-element-style URL
/// successfully authenticated via `?api_key=`. Caller uses it on the
/// stream / HLS response so subsequent fetches don't have to repeat
/// the query string.
pub fn auth_cookie_header(token: &str) -> String {
    format!("JellyfinAuth={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age=86400")
}

/// Parse `MediaBrowser Token="abc", Client="x", …` style headers.
fn parse_mediabrowser_token(value: &str) -> Option<String> {
    parse_auth_header(value).and_then(|p| p.token)
}

/// All four fields jellyfin-web / mobile / TV clients drop into the
/// Emby-Authorization header. Used by `/Users/AuthenticateByName` to
/// label the resulting `SessionInfo`, by /Sessions to enrich the live
/// session list, and by token issuance to bind tokens to a device.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AuthHeader {
    pub token: Option<String>,
    pub device_id: Option<String>,
    pub device: Option<String>,
    pub client: Option<String>,
    pub version: Option<String>,
}

impl AuthHeader {
    /// What clients call themselves — falls back to "Unknown" only at
    /// the wire boundary. Callers can still introspect `Option::None`.
    pub fn device_label(&self) -> String {
        self.device.clone().unwrap_or_else(|| "Unknown".into())
    }
    pub fn client_label(&self) -> String {
        self.client.clone().unwrap_or_else(|| "Unknown".into())
    }
    pub fn version_label(&self) -> String {
        self.version.clone().unwrap_or_else(|| "0".into())
    }
}

/// Walk every recognised header and merge them. Header keys checked
/// (in order): `X-Emby-Authorization`, `Authorization`.
pub fn auth_header_from_request(req: &HttpRequest) -> AuthHeader {
    let mut out = AuthHeader::default();
    for name in ["X-Emby-Authorization", "Authorization"] {
        if let Some(v) = req.headers().get(name).and_then(|v| v.to_str().ok()) {
            if let Some(parsed) = parse_auth_header(v) {
                out = merge(out, parsed);
            }
        }
    }
    out
}

fn merge(mut a: AuthHeader, b: AuthHeader) -> AuthHeader {
    if a.token.is_none() {
        a.token = b.token;
    }
    if a.device_id.is_none() {
        a.device_id = b.device_id;
    }
    if a.device.is_none() {
        a.device = b.device;
    }
    if a.client.is_none() {
        a.client = b.client;
    }
    if a.version.is_none() {
        a.version = b.version;
    }
    a
}

/// `MediaBrowser Client="x", Device="iPhone", DeviceId="abc",
/// Version="1.0", Token="…"` — parse every k=v pair the schemes
/// recognised by Emby/Jellyfin use. Unknown keys are ignored.
pub fn parse_auth_header(value: &str) -> Option<AuthHeader> {
    let after = value
        .strip_prefix("MediaBrowser")
        .or_else(|| value.strip_prefix("Emby"))?;
    let mut out = AuthHeader::default();
    for part in after.split(',') {
        let part = part.trim();
        let Some((k, raw)) = part.split_once('=') else {
            continue;
        };
        let v = raw.trim().trim_matches('"').trim();
        if v.is_empty() {
            continue;
        }
        match k.trim().to_ascii_lowercase().as_str() {
            "token" => out.token = Some(v.to_string()),
            "deviceid" => out.device_id = Some(v.to_string()),
            "device" => out.device = Some(v.to_string()),
            "client" => out.client = Some(v.to_string()),
            "version" => out.version = Some(v.to_string()),
            _ => {}
        }
    }
    Some(out)
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
    fn parse_auth_header_extracts_all_four_fields() {
        let p = parse_auth_header(
            r#"MediaBrowser Client="Jellyfin Web", Device="Firefox", DeviceId="abc-123", Version="10.11.0", Token="xyz""#,
        )
        .unwrap();
        assert_eq!(p.client.as_deref(), Some("Jellyfin Web"));
        assert_eq!(p.device.as_deref(), Some("Firefox"));
        assert_eq!(p.device_id.as_deref(), Some("abc-123"));
        assert_eq!(p.version.as_deref(), Some("10.11.0"));
        assert_eq!(p.token.as_deref(), Some("xyz"));
    }

    #[test]
    fn auth_header_from_request_merges_multiple_headers() {
        // Some clients split Authorization vs X-Emby-Authorization;
        // missing fields should fall through from one to the other.
        let req = TestRequest::default()
            .insert_header(("X-Emby-Authorization", r#"MediaBrowser DeviceId="a""#))
            .insert_header(("Authorization", r#"MediaBrowser Client="c", Version="2""#))
            .to_http_request();
        let h = auth_header_from_request(&req);
        assert_eq!(h.device_id.as_deref(), Some("a"));
        assert_eq!(h.client.as_deref(), Some("c"));
        assert_eq!(h.version.as_deref(), Some("2"));
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
