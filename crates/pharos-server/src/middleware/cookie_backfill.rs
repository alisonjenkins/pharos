//! Give a browser that signed in before the `JellyfinAuth` cookie existed (or
//! lost it) the cookie on its next authenticated request.
//!
//! The cookie is issued at sign-in only (ADR-0031), but jellyfin-web keeps its
//! token in localStorage and calls the JSON API with a header. Without the
//! cookie its `<img>` and `<track>` fetches carry no credential and 401 until
//! the user signs out and back in.
//!
//! The auth extractors leave a [`HeaderCredential`] in the request extensions
//! once the token store accepted a header or `api_key` token. A 2xx response
//! to such a request that arrives with no `JellyfinAuth` cookie, or one
//! holding another token, gets the cookie for the validated token. A cookie
//! alone never produces one: only a credential the store has just accepted is
//! echoed.

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

/// Why the cookie was sent; the `reason` label of
/// `pharos_auth_cookie_backfilled_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackfillReason {
    /// The request carried no `JellyfinAuth` cookie.
    Absent,
    /// The request's cookie held a token other than the one it authenticated with.
    Different,
}

impl BackfillReason {
    pub const ALL: [Self; 2] = [Self::Absent, Self::Different];

    pub fn label(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Different => "different",
        }
    }
}

/// `None` when the request's cookie already holds `token`.
fn reason(cookie: Option<&str>, token: &str) -> Option<BackfillReason> {
    match cookie.map(str::trim) {
        None | Some("") => Some(BackfillReason::Absent),
        Some(held) if held == token => None,
        Some(_) => Some(BackfillReason::Different),
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
            let Some(why) = reason(held.as_ref().map(|c| c.value()), &token) else {
                return Ok(res);
            };
            match res
                .response_mut()
                .add_cookie(&auth_cookie(&request, &token))
            {
                Ok(()) => {
                    metrics::counter!(
                        "pharos_auth_cookie_backfilled_total",
                        "reason" => why.label()
                    )
                    .increment(1);
                    tracing::debug!(
                        reason = why.label(),
                        path = request.path(),
                        "JellyfinAuth cookie backfilled for a header-authenticated request"
                    );
                }
                Err(e) => tracing::warn!(
                    error = %e,
                    reason = why.label(),
                    "JellyfinAuth cookie backfill failed: token is not a valid cookie value"
                ),
            }
            Ok(res)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn reason_distinguishes_absent_different_and_matching() {
        assert_eq!(reason(None, "t"), Some(BackfillReason::Absent));
        assert_eq!(reason(Some(""), "t"), Some(BackfillReason::Absent));
        assert_eq!(reason(Some("old"), "t"), Some(BackfillReason::Different));
        assert_eq!(reason(Some("t"), "t"), None);
    }

    #[test]
    fn labels_are_distinct() {
        let labels: std::collections::HashSet<_> =
            BackfillReason::ALL.iter().map(|r| r.label()).collect();
        assert_eq!(labels.len(), BackfillReason::ALL.len());
    }
}
