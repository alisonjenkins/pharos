//! Sign the `ImageTag`s in every JSON response on its way out (ADR-0073).
//!
//! The DTO builders mint unkeyed tags in some seventy places. Rather than
//! thread the server key through each, this layer rewrites the finished
//! body: [`pharos_jellyfin_api::image_tag_sig::sign_image_tags`] pairs each
//! tag field with the id field a client builds the image URL from, and
//! appends a MAC only this server can produce. An anonymous image request
//! then proves it came from a listing (V712).
//!
//! Only `application/json` bodies that mention `ImageTag` are touched, so
//! media, images, subtitles and the native API's protobuf pass straight
//! through without being buffered.

use actix_web::{
    body::{BoxBody, MessageBody},
    dev::{ServiceRequest, ServiceResponse},
    http::header,
    middleware::Next,
    web, Error,
};

use crate::state::AppState;

/// Upper bound on a body this layer will buffer to sign. A larger JSON body
/// is passed through unsigned and counted, rather than held in memory.
const MAX_SIGNED_BODY: usize = 64 * 1024 * 1024;

/// What happened to a JSON response; the `outcome` label of
/// `pharos_image_tag_sign_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignOutcome {
    /// At least one tag was signed.
    Signed,
    /// The body mentions `ImageTag` but held no tag with its id field.
    NoTags,
    /// The body was not valid JSON; passed through unchanged.
    Unparsed,
    /// The body exceeded [`MAX_SIGNED_BODY`]; passed through unsigned.
    TooLarge,
    /// No `AppState` (and so no key) on the app: passed through unsigned.
    /// Under enforcement every tag in such a body would 401.
    NoKey,
}

impl SignOutcome {
    pub const ALL: [Self; 5] = [
        Self::Signed,
        Self::NoTags,
        Self::Unparsed,
        Self::TooLarge,
        Self::NoKey,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Signed => "signed",
            Self::NoTags => "no_tags",
            Self::Unparsed => "unparsed",
            Self::TooLarge => "too_large",
            Self::NoKey => "no_key",
        }
    }
}

fn is_json(res: &ServiceResponse<impl MessageBody>) -> bool {
    res.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/json"))
}

fn count(outcome: SignOutcome) {
    metrics::counter!("pharos_image_tag_sign_total", "outcome" => outcome.label()).increment(1);
}

/// `actix_web::middleware::from_fn(sign_image_tags)`.
pub async fn sign_image_tags(
    req: ServiceRequest,
    next: Next<impl MessageBody + 'static>,
) -> Result<ServiceResponse<BoxBody>, Error> {
    // Listings are GETs; everything else passes through unbuffered.
    let is_get = req.method() == actix_web::http::Method::GET;
    let key = req
        .app_data::<web::Data<AppState>>()
        .map(|s| s.image_tag_key.clone());
    let res = next.call(req).await?;
    if !is_get || !res.status().is_success() || !is_json(&res) {
        return Ok(res.map_into_boxed_body());
    }
    let Some(key) = key else {
        count(SignOutcome::NoKey);
        tracing::warn!(
            path = res.request().path(),
            "no AppState on the app, JSON image tags go out unsigned"
        );
        return Ok(res.map_into_boxed_body());
    };
    // A body known to be over the limit is passed through rather than
    // buffered. Only a streamed body of unknown size can still hit the limit
    // below, and no JSON route streams.
    if let actix_web::body::BodySize::Sized(n) = res.response().body().size() {
        if n > MAX_SIGNED_BODY as u64 {
            count(SignOutcome::TooLarge);
            tracing::warn!(
                bytes = n,
                limit = MAX_SIGNED_BODY,
                path = res.request().path(),
                "JSON body too large to sign, image tags go out unsigned"
            );
            return Ok(res.map_into_boxed_body());
        }
    }
    let (req, res) = res.into_parts();
    let (mut head, body) = res.into_parts();
    let bytes = match actix_web::body::to_bytes_limited(body, MAX_SIGNED_BODY).await {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            let e: Box<dyn std::error::Error> = e.into();
            return Err(actix_web::error::ErrorInternalServerError(e.to_string()));
        }
        Err(_) => {
            // A streamed body ran past the limit and is consumed; there is
            // nothing left to pass through.
            count(SignOutcome::TooLarge);
            tracing::warn!(
                limit = MAX_SIGNED_BODY,
                path = req.path(),
                "streamed JSON body too large to sign"
            );
            return Err(actix_web::error::ErrorInternalServerError(
                "response too large",
            ));
        }
    };
    if memchr_tag(&bytes) {
        match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(mut v) => {
                let n = pharos_jellyfin_api::image_tag_sig::sign_image_tags(&key, &mut v);
                if n > 0 {
                    count(SignOutcome::Signed);
                    if let Ok(out) = serde_json::to_vec(&v) {
                        head.headers_mut().remove(header::CONTENT_LENGTH);
                        let res = head.set_body(BoxBody::new(out));
                        return Ok(ServiceResponse::new(req, res));
                    }
                } else {
                    count(SignOutcome::NoTags);
                }
            }
            Err(_) => count(SignOutcome::Unparsed),
        }
    }
    let res = head.set_body(BoxBody::new(bytes));
    Ok(ServiceResponse::new(req, res))
}

fn memchr_tag(bytes: &[u8]) -> bool {
    bytes.windows(8).any(|w| w == b"ImageTag")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    use actix_web::{test as at, App, HttpResponse};

    async fn listing() -> HttpResponse {
        HttpResponse::Ok().json(serde_json::json!({"Id": "a", "ImageTags": {"Primary": "p"}}))
    }

    async fn tag_of(
        app_state: Option<web::Data<AppState>>,
        method: actix_web::http::Method,
    ) -> String {
        let mut app = App::new();
        if let Some(st) = app_state {
            app = app.app_data(st);
        }
        let app = at::init_service(
            app.wrap(actix_web::middleware::from_fn(sign_image_tags))
                .route("/l", web::route().to(listing)),
        )
        .await;
        let req = at::TestRequest::default()
            .method(method)
            .uri("/l")
            .to_request();
        let body: serde_json::Value = at::call_and_read_body_json(&app, req).await;
        body["ImageTags"]["Primary"].as_str().unwrap().to_owned()
    }

    #[actix_web::test]
    async fn a_get_listing_is_signed_and_other_methods_pass_through() {
        let stores = crate::state::Stores::connect("sqlite::memory:")
            .await
            .unwrap();
        let state = web::Data::new(AppState::new(stores, "t".into()));
        let key = state.image_tag_key.clone();
        let signed = tag_of(Some(state.clone()), actix_web::http::Method::GET).await;
        assert_eq!(
            key.verify("a", "primary", Some(&signed)),
            pharos_jellyfin_api::image_tag_sig::TagVerdict::Valid
        );
        assert_eq!(
            tag_of(Some(state), actix_web::http::Method::POST).await,
            "p"
        );
    }

    #[actix_web::test]
    async fn an_app_without_state_passes_tags_through_and_says_so() {
        let _ = crate::obs::init("info", None);
        assert_eq!(tag_of(None, actix_web::http::Method::GET).await, "p");
        let rendered = crate::obs::render();
        assert!(
            rendered
                .lines()
                .any(|l| l.starts_with("pharos_image_tag_sign_total{")
                    && l.contains(r#"outcome="no_key""#)),
            "{rendered}"
        );
    }

    #[test]
    fn outcome_labels_are_distinct() {
        let mut labels: Vec<_> = SignOutcome::ALL.iter().map(|o| o.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), SignOutcome::ALL.len());
    }
}
