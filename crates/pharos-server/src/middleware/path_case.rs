//! Lowercase the request URI **path** before the actix router matches.
//!
//! Background: jellyfin-web mixes PascalCase and lowercase variants of
//! every endpoint (`/Items`, `/items`, `/Users/{id}/Items`, …) and our
//! handlers used to register both forms. Maintenance cost grew with
//! every endpoint, hence T31.
//!
//! Strategy: a single transform mutates the URI on the way in so each
//! handler only needs to register the lowercase canonical path. The
//! query string is preserved verbatim — only the path component is
//! touched.

use actix_web::{
    body::MessageBody,
    dev::{Service, ServiceRequest, ServiceResponse, Transform, Url},
    http::uri::{PathAndQuery, Uri},
    Error,
};
use std::{
    future::{ready, Future, Ready},
    pin::Pin,
    rc::Rc,
    str::FromStr,
    task::{Context, Poll},
};

pub struct LowercasePath;

impl<S, B> Transform<S, ServiceRequest> for LowercasePath
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Transform = LowercasePathMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(LowercasePathMiddleware {
            inner: Rc::new(service),
        }))
    }
}

pub struct LowercasePathMiddleware<S> {
    inner: Rc<S>,
}

impl<S, B> Service<ServiceRequest> for LowercasePathMiddleware<S>
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

    fn call(&self, mut req: ServiceRequest) -> Self::Future {
        if let Some(new_uri) = lowercased(req.uri()) {
            // Both `match_info` (router state) and `head.uri` (the
            // request HEAD the handlers + extractors read) must be
            // updated — mirrors actix-web's own `NormalizePath`. Skip
            // either and the router still sees the original case and
            // returns 404.
            req.match_info_mut().set(Url::new(new_uri.clone()));
            req.head_mut().uri = new_uri;
        }
        let inner = self.inner.clone();
        Box::pin(async move { inner.call(req).await })
    }
}

/// Build a new `Uri` with the path lowercased; returns `None` if the
/// path was already entirely lowercase (skip the allocation).
fn lowercased(uri: &Uri) -> Option<Uri> {
    let path = uri.path();
    if !path.bytes().any(|b| b.is_ascii_uppercase()) {
        return None;
    }
    let mut lowered = String::with_capacity(path.len());
    for c in path.chars() {
        lowered.push(c.to_ascii_lowercase());
    }
    let pq = match uri.query() {
        Some(q) => format!("{lowered}?{q}"),
        None => lowered,
    };
    let new_pq = PathAndQuery::from_str(&pq).ok()?;
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(new_pq);
    Uri::from_parts(parts).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    // `actix_web::test` is also a proc-macro attribute; bringing the
    // module into scope shadows the built-in `#[test]` attribute and
    // makes rustc demand `async fn`. Import the helpers under an alias
    // so the sync unit tests below keep using stdlib `#[test]`.
    use super::*;
    use actix_web::{test as at, web, App, HttpResponse};

    #[test]
    fn lowercased_skips_no_op() {
        let u: Uri = "/already/lower?Q=KEEP".parse().unwrap();
        assert!(lowercased(&u).is_none());
    }

    #[test]
    fn lowercased_path_only_preserves_query() {
        let u: Uri = "/Items/100?SearchTerm=BLADE".parse().unwrap();
        let new = lowercased(&u).unwrap();
        assert_eq!(new.path(), "/items/100");
        assert_eq!(new.query(), Some("SearchTerm=BLADE"));
    }

    #[test]
    fn lowercased_preserves_path_params() {
        let u: Uri = "/Users/DEADBEEF/Items".parse().unwrap();
        let new = lowercased(&u).unwrap();
        // UUIDs survive lowercasing — pharos always serialises as
        // lowercase hex anyway, so a Jellyfin client that uppercased
        // its id will round-trip onto our canonical form.
        assert_eq!(new.path(), "/users/deadbeef/items");
    }

    #[actix_web::test]
    async fn middleware_routes_pascalcase_to_lowercase_handler() {
        let app = at::init_service(
            App::new().wrap(LowercasePath).route(
                "/items/{id}",
                web::get().to(|p: web::Path<String>| {
                    let id = p.into_inner();
                    async move { HttpResponse::Ok().body(id) }
                }),
            ),
        )
        .await;

        // PascalCase request — handler is only registered as lowercase.
        let req = at::TestRequest::get().uri("/Items/42").to_request();
        let body = at::call_and_read_body(&app, req).await;
        assert_eq!(&body[..], b"42");
    }

    #[actix_web::test]
    async fn middleware_passes_through_already_lowercase() {
        let app = at::init_service(App::new().wrap(LowercasePath).route(
            "/items",
            web::get().to(|| async { HttpResponse::Ok().body("ok") }),
        ))
        .await;
        let req = at::TestRequest::get().uri("/items").to_request();
        let body = at::call_and_read_body(&app, req).await;
        assert_eq!(&body[..], b"ok");
    }

    #[actix_web::test]
    async fn middleware_keeps_query_string_case() {
        let app = at::init_service(App::new().wrap(LowercasePath).route(
            "/items",
            web::get().to(|q: web::Query<std::collections::HashMap<String, String>>| {
                let map = q.into_inner();
                async move { HttpResponse::Ok().body(map.get("SearchTerm").cloned().unwrap_or_default()) }
            }),
        ))
        .await;
        let req = at::TestRequest::get()
            .uri("/Items?SearchTerm=BLADE")
            .to_request();
        let body = at::call_and_read_body(&app, req).await;
        assert_eq!(&body[..], b"BLADE");
    }
}
