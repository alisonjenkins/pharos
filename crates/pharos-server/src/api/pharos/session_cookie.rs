//! `/pharos/session`: browser sign-in and sign-out for the native API
//! (research R6, FR-003a, ADR-0029).
//!
//! A browser cannot set a cookie from a WebSocket frame, so it signs in over
//! plain HTTP and the `/pharos/rpc` upgrade then carries the cookie to
//! `Bootstrap.resume("")`. The cookie is `HttpOnly`, so script never sees the
//! token. Native apps use `Bootstrap.login` and keep the token themselves.
//!
//! Both methods are browser-only. Each needs an `Origin` in
//! `[server.native_api].allowed_origins`, which a page on another site cannot
//! forge (login CSRF, forced logout). `POST` also needs a JSON body, which a
//! cross-site form cannot send without a CORS preflight that pharos never
//! answers.

use actix_web::cookie::{time::Duration as CookieDuration, Cookie, SameSite};
use actix_web::http::header::{CACHE_CONTROL, ORIGIN};
use actix_web::{web, HttpRequest, HttpResponse};
use pharos_core::{SecretString, TokenStore};
use serde::Deserialize;

use super::rpc::{origin, SESSION_COOKIE};
use crate::auth::LoginRefusal;
use crate::config::NativeApiConfig;
use crate::services::auth::{
    password_login, record_login, revoke_token, LoginOutcome, LoginTransport, PasswordLoginError,
    RevokeCause,
};
use crate::state::AppState;

/// Largest sign-in body; a name and a password fit in far less.
pub const MAX_BODY: usize = 4 * 1024;

/// How long the browser keeps the cookie: the longest browsers allow
/// (400 days). The token behind it lives until it is revoked, and a browser
/// that dropped its cookie could no longer sign out, so the cookie should not
/// expire before the token; until tokens expire server-side, an unused one is
/// revoked from the user's devices list.
const COOKIE_MAX_AGE: CookieDuration = CookieDuration::days(400);

#[derive(Deserialize)]
pub struct SignIn {
    name: String,
    /// Wrapped in a [`SecretString`] as soon as it is read.
    password: String,
}

/// `pharos_session` with the attributes every response sets it with: a
/// clearing cookie only clears when name, path and domain match.
fn cookie(value: String, max_age: CookieDuration) -> Cookie<'static> {
    Cookie::build(SESSION_COOKIE, value)
        .path("/pharos")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Strict)
        .max_age(max_age)
        .finish()
}

/// `None` when the request's `Origin` is in `allowed_origins`; otherwise the
/// 403 to answer with, logged naming the origin. A missing `Origin` is
/// refused here, unlike on the upgrade: only browsers use this endpoint.
fn refuse_origin(req: &HttpRequest, cfg: &NativeApiConfig, action: &str) -> Option<HttpResponse> {
    let origin = req
        .headers()
        .get(ORIGIN)
        .map(|v| v.to_str().unwrap_or("<non-ascii>"));
    let verdict = match origin {
        None => Err("origin_missing"),
        Some(o) => origin::check(Some(o), &cfg.allowed_origins).map_err(|r| r.label()),
    };
    let reason = verdict.err()?;
    tracing::warn!(
        action,
        reason,
        origin = origin.unwrap_or(""),
        "native api session request refused"
    );
    Some(HttpResponse::Forbidden().body(reason))
}

/// `POST /pharos/session`: check the password and set `pharos_session`.
pub async fn sign_in(
    req: HttpRequest,
    // Extracted as a `Result` so the origin is judged first: a cross-site
    // request is refused (and counted) as such whatever its body.
    body: Result<web::Json<SignIn>, actix_web::Error>,
    state: web::Data<AppState>,
    cfg: web::Data<NativeApiConfig>,
) -> Result<HttpResponse, actix_web::Error> {
    if let Some(refused) = refuse_origin(&req, &cfg, "sign_in") {
        record_login(
            LoginTransport::Cookie,
            "password",
            LoginOutcome::OriginRefused,
        );
        return Ok(refused);
    }
    let SignIn { name, password } = body?.into_inner();
    let password = SecretString::new(password);
    let client_ip = crate::client_ip::client_ip(&req, &state.trusted_proxies);
    let device_id = format!("pharos-web-{}", uuid::Uuid::new_v4().simple());
    Ok(
        match password_login(
            &state,
            LoginTransport::Cookie,
            &name,
            &password,
            client_ip,
            &device_id,
            // The cookie opens no connection; the upgrade's resume is capped.
            |_| Ok(()),
        )
        .await
        {
            Ok((_user, token)) => HttpResponse::NoContent()
                .insert_header((CACHE_CONTROL, "no-store"))
                .cookie(cookie(token.0.expose().to_owned(), COOKIE_MAX_AGE))
                .finish(),
            Err(PasswordLoginError::Refused(LoginRefusal::Throttled(retry))) => {
                HttpResponse::TooManyRequests()
                    .insert_header((actix_web::http::header::RETRY_AFTER, retry.secs.to_string()))
                    .body("too many failed login attempts")
            }
            Err(PasswordLoginError::Refused(LoginRefusal::Invalid)) => {
                HttpResponse::Unauthorized().body("invalid credentials")
            }
            // The cause is in the server log (`native login refused`).
            Err(_) => HttpResponse::ServiceUnavailable().body("sign-in unavailable"),
        },
    )
}

/// How a sign-out ended. The label is the `outcome` of
/// `pharos_native_logout_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogoutOutcome {
    /// The cookie's token was revoked.
    Revoked,
    /// No cookie, or its token no longer resolved: only the cookie is cleared.
    NoSession,
    /// The token store failed; the cookie is kept so the browser can retry.
    Unavailable,
    OriginRefused,
}

impl LogoutOutcome {
    pub fn label(self) -> &'static str {
        match self {
            LogoutOutcome::Revoked => "revoked",
            LogoutOutcome::NoSession => "no_session",
            LogoutOutcome::Unavailable => "unavailable",
            LogoutOutcome::OriginRefused => "origin_refused",
        }
    }
}

fn record_logout(outcome: LogoutOutcome) {
    metrics::counter!("pharos_native_logout_total", "outcome" => outcome.label()).increment(1);
}

/// `DELETE /pharos/session`: revoke the cookie's token, which closes the
/// connections it opened (T025), and clear the cookie. A missing or unknown
/// cookie is still cleared. When the store fails the cookie is kept and the
/// answer is 503: clearing it would leave a live token the browser can no
/// longer revoke.
pub async fn sign_out(
    req: HttpRequest,
    state: web::Data<AppState>,
    cfg: web::Data<NativeApiConfig>,
) -> HttpResponse {
    if let Some(refused) = refuse_origin(&req, &cfg, "sign_out") {
        record_logout(LogoutOutcome::OriginRefused);
        return refused;
    }
    let outcome = match req.cookie(SESSION_COOKIE).map(|c| c.value().to_owned()) {
        None => LogoutOutcome::NoSession,
        Some(token) => match state.stores.resolve(&token).await {
            Ok(user) => match revoke_token(&state, user, &token, RevokeCause::Logout).await {
                Ok(()) => LogoutOutcome::Revoked,
                Err(e) => {
                    tracing::warn!(error = %e, "native api sign-out: revoke failed");
                    LogoutOutcome::Unavailable
                }
            },
            Err(pharos_core::AuthError::Backend(e)) => {
                tracing::warn!(error = %e, "native api sign-out: token store unavailable");
                LogoutOutcome::Unavailable
            }
            // Already revoked or never valid: nothing to revoke.
            Err(_) => LogoutOutcome::NoSession,
        },
    };
    record_logout(outcome);
    tracing::info!(outcome = outcome.label(), "native api sign-out");
    if outcome == LogoutOutcome::Unavailable {
        return HttpResponse::ServiceUnavailable().body("sign-out unavailable");
    }
    HttpResponse::NoContent()
        .insert_header((CACHE_CONTROL, "no-store"))
        .cookie(cookie(String::new(), CookieDuration::ZERO))
        .finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logout_outcome_labels_are_distinct() {
        let outcomes = [
            LogoutOutcome::Revoked,
            LogoutOutcome::NoSession,
            LogoutOutcome::Unavailable,
            LogoutOutcome::OriginRefused,
        ];
        let unique: std::collections::HashSet<_> = outcomes.iter().map(|o| o.label()).collect();
        assert_eq!(unique.len(), outcomes.len());
    }
}
