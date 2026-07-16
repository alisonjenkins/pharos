//! SIMD-accelerated JSON wire helpers.
//!
//! Response bodies and request bodies for the Jellyfin API go through
//! `sonic-rs` (SIMD JSON) instead of actix's default `serde_json` path. Two
//! entry points:
//!
//! - [`json`] — serialize any `Serialize` DTO to a `200 application/json`
//!   response with sonic-rs. Replaces `HttpResponse::Ok().json(v)`.
//! - [`SimdJson`] — a `FromRequest` extractor that deserializes the request
//!   body with sonic-rs. Replaces `web::Json<T>`.
//!
//! Pairing typed DTOs with these helpers is deliberate: a hand-built
//! `json!({...})` response can silently drop a field a strict client (the
//! jellyfin-sdk-kotlin Android/TV apps) requires — the recurring "Unable to
//! connect / resolve playback info" class (B13/B63/B64). A `#[derive(Serialize)]`
//! struct makes every field compile-visible and auditable against the SDK model.

use actix_web::{
    dev::Payload, error::ErrorBadRequest, http::header, web::BytesMut, FromRequest, HttpRequest,
    HttpResponse,
};
use futures_util::StreamExt;
use serde::{de::DeserializeOwned, Serialize};
use std::future::Future;
use std::pin::Pin;

/// Serialize `v` with sonic-rs (SIMD) into a `200 application/json` response.
/// The DTO must round-trip through serde; sonic-rs uses the same derive.
pub fn json<T: Serialize>(v: &T) -> HttpResponse {
    match sonic_rs::to_vec(v) {
        Ok(bytes) => HttpResponse::Ok()
            .content_type("application/json")
            .body(bytes),
        // Serialization of an owned DTO effectively never fails; surface the
        // cause rather than a bare 500 if it somehow does.
        Err(e) => {
            HttpResponse::InternalServerError().body(format!("response serialization failed: {e}"))
        }
    }
}

/// Serialize a paged-list response as a Jellyfin `BaseItemDtoQueryResult`
/// envelope (`Items` / `TotalRecordCount` / `StartIndex`). Wraps the typed
/// [`ItemsResultDto`] so no browse/list/search handler hand-builds the
/// envelope as a `json!` literal that could drop one of the three
/// kotlin-required fields (B78/V38). `T` is inferred from `items` —
/// `BaseItemDto`, `SynthItemDto`, or already-serialized `serde_json::Value`.
pub fn query_result<T: Serialize>(
    items: Vec<T>,
    total_record_count: u32,
    start_index: u32,
) -> HttpResponse {
    json(&pharos_jellyfin_api::dto::ItemsResultDto {
        items,
        total_record_count,
        start_index,
    })
}

/// Cap on a JSON request body (defensive; Jellyfin bodies are small — the
/// largest is a DeviceProfile at a few KB).
const MAX_BODY: usize = 2 * 1024 * 1024;

/// `FromRequest` extractor that deserializes the body with sonic-rs (SIMD).
/// Drop-in for `web::Json<T>` on the request side. `T::default()` is NOT used —
/// a malformed/oversized body is a 400, matching actix's `Json` behaviour.
pub struct SimdJson<T>(pub T);

impl<T> SimdJson<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T: DeserializeOwned + 'static> FromRequest for SimdJson<T> {
    type Error = actix_web::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self, Self::Error>>>>;

    fn from_request(_req: &HttpRequest, payload: &mut Payload) -> Self::Future {
        let mut payload = payload.take();
        Box::pin(async move {
            let mut buf = BytesMut::new();
            while let Some(chunk) = payload.next().await {
                let chunk = chunk.map_err(ErrorBadRequest)?;
                if buf.len() + chunk.len() > MAX_BODY {
                    return Err(ErrorBadRequest("request body too large"));
                }
                buf.extend_from_slice(&chunk);
            }
            // An empty body deserializes to `T` only when `T` accepts it
            // (e.g. `#[serde(default)]`); sonic-rs errors otherwise → 400.
            let value = sonic_rs::from_slice::<T>(&buf)
                .map_err(|e| ErrorBadRequest(format!("invalid JSON body: {e}")))?;
            Ok(SimdJson(value))
        })
    }
}

/// Whether a header set names `application/json`. (Kept for parity with actix's
/// content-type gate; callers that must be lenient — Jellyfin clients sometimes
/// omit the header on a POST — can skip it.)
pub fn is_json_content_type(req: &HttpRequest) -> bool {
    req.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("application/json"))
        .unwrap_or(false)
}
