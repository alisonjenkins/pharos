//! Give a browser that signed in before the `JellyfinAuth` cookie existed (or
//! lost it) the cookie on its next authenticated request.
//!
//! The cookie is issued at sign-in only (ADR-0031), but jellyfin-web keeps its
//! token in localStorage and calls the JSON API with a header. Without the
//! cookie its `<img>` and `<track>` fetches carry no credential and 401 until
//! the user signs out and back in.
//!
//! The auth extractors leave a [`HeaderCredential`] in the request extensions
//! once the token store accepted a header token. A 2xx response to such a
//! request gets the cookie for that token only when the browser says the
//! request came from pharos's own page (`Sec-Fetch-Site: same-origin`) and it
//! holds no `JellyfinAuth` cookie yet (B253, V193). An `api_key` in the URL
//! never counts: any page on a sibling host can put one in an `<img>`, and
//! echoing it would plant the attacker's session in the victim's browser.
//! For the same reason a cookie holding another token is never overwritten;
//! signing in is what replaces it.

use actix_web::{
    body::MessageBody,
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    Error, HttpMessage,
};
use std::{
    future::{ready, Future, Ready},
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

use crate::api::jellyfin::auth_extractor::{auth_cookie, HeaderCredential, AUTH_COOKIE};

/// Whether a header-authenticated 2xx response gets the cookie; the
/// `outcome` label of `pharos_auth_cookie_backfill_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackfillOutcome {
    /// Same-origin request with no `JellyfinAuth` cookie: the cookie is set.
    Set,
    /// The request already holds the cookie for this token.
    Held,
    /// `Sec-Fetch-Site` is absent or not `same-origin`: a native client, or a
    /// request another site's page made.
    NotSameOrigin,
    /// The request holds a cookie for another token; never overwritten.
    OtherCookie,
    /// The cookie was due but the token is not a valid cookie value.
    Unencodable,
}

impl BackfillOutcome {
    pub const ALL: [Self; 5] = [
        Self::Set,
        Self::Held,
        Self::NotSameOrigin,
        Self::OtherCookie,
        Self::Unencodable,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Set => "set",
            Self::Held => "held",
            Self::NotSameOrigin => "not_same_origin",
            Self::OtherCookie => "other_cookie",
            Self::Unencodable => "unencodable",
        }
    }
}

fn outcome(fetch_site: Option<&str>, cookie: Option<&str>, token: &str) -> BackfillOutcome {
    if fetch_site.map(str::trim) != Some("same-origin") {
        return BackfillOutcome::NotSameOrigin;
    }
    match cookie.map(str::trim) {
        None | Some("") => BackfillOutcome::Set,
        Some(held) if held == token => BackfillOutcome::Held,
        Some(_) => BackfillOutcome::OtherCookie,
    }
}

pub struct CookieBackfill;

impl<S, B> Transform<S, ServiceRequest> for CookieBackfill
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Transform = CookieBackfillMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(CookieBackfillMiddleware {
            inner: Rc::new(service),
        }))
    }
}

pub struct CookieBackfillMiddleware<S> {
    inner: Rc<S>,
}

impl<S, B> Service<ServiceRequest> for CookieBackfillMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let inner = self.inner.clone();
        Box::pin(async move {
            let mut res = inner.call(req).await?;
            if !res.status().is_success() {
                return Ok(res);
            }
            let request = res.request().clone();
            let Some(token) = request
                .extensions()
                .get::<HeaderCredential>()
                .map(|c| c.0.clone())
            else {
                return Ok(res);
            };
            // Sign-in and sign-out set or clear the cookie themselves.
            if res.response().cookies().any(|c| c.name() == AUTH_COOKIE) {
                return Ok(res);
            }
            let held = request.cookie(AUTH_COOKIE);
            let fetch_site = request
                .headers()
                .get("Sec-Fetch-Site")
                .and_then(|v| v.to_str().ok());
            let verdict = outcome(fetch_site, held.as_ref().map(|c| c.value()), &token);
            let verdict = if verdict == BackfillOutcome::Set {
                match res
                    .response_mut()
                    .add_cookie(&auth_cookie(&request, &token))
                {
                    Ok(()) => verdict,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "JellyfinAuth cookie backfill failed: token is not a valid cookie value"
                        );
                        BackfillOutcome::Unencodable
                    }
                }
            } else {
                verdict
            };
            metrics::counter!(
                "pharos_auth_cookie_backfill_total",
                "outcome" => verdict.label()
            )
            .increment(1);
            tracing::debug!(
                outcome = verdict.label(),
                fetch_site,
                path = request.path(),
                "JellyfinAuth cookie backfill decided for a header-authenticated request"
            );
            Ok(res)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn only_a_same_origin_request_without_a_cookie_gets_one() {
        use BackfillOutcome::*;
        let same = Some("same-origin");
        assert_eq!(outcome(same, None, "t"), Set);
        assert_eq!(outcome(same, Some(""), "t"), Set);
        assert_eq!(outcome(same, Some("t"), "t"), Held);
        assert_eq!(outcome(same, Some("old"), "t"), OtherCookie);
        for site in [None, Some("same-site"), Some("cross-site"), Some("none")] {
            assert_eq!(outcome(site, None, "t"), NotSameOrigin, "{site:?}");
        }
    }

    #[test]
    fn labels_are_distinct() {
        let labels: std::collections::HashSet<_> =
            BackfillOutcome::ALL.iter().map(|r| r.label()).collect();
        assert_eq!(labels.len(), BackfillOutcome::ALL.len());
    }
}
