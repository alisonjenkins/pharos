//! A user's profile picture on Jellyfin's routes (V362): `/UserImage` and
//! the older `/Users/{id}/Images/{type}` spellings, so Jellyfin clients see
//! and set the same avatar the native UI does. Storage and checks are
//! [`crate::avatar`]'s.
//!
//! As in Jellyfin, a GET is anonymous (an `<img>` sends no credential) and
//! names the user by id; setting or removing one needs the user themselves
//! or an admin. The upload body is the picture base64-encoded, as Jellyfin
//! reads it; raw picture bytes are accepted too.

use std::borrow::Cow;

use actix_web::{error, http::header, web, HttpRequest, HttpResponse};
use base64::Engine as _;
use pharos_core::{UserId, UserStore};
use uuid::Uuid;

use crate::api::jellyfin::auth_extractor::AuthUser;
use crate::api::jellyfin::ci_query::CiQuery;
use crate::avatar::{self, AvatarError};
use crate::state::AppState;

/// Largest upload body: a [`avatar::MAX_UPLOAD_BYTES`] picture once base64
/// grows it by a third, with room for a data-URL prefix and line breaks.
const MAX_BODY_BYTES: usize = avatar::MAX_UPLOAD_BYTES / 3 * 4 + 64 * 1024;

/// How long a client may cache a picture asked for by its tag; the tag
/// changes with the picture, so the URL does too.
const TAGGED_MAX_AGE_SECS: u32 = 365 * 24 * 60 * 60;

pub fn register(cfg: &mut web::ServiceConfig) {
    // Lowercase only: `LowercasePath` normalises incoming paths (T31).
    for path in [
        "/userimage",
        "/users/{user_id}/images/{image_type}",
        "/users/{user_id}/images/{image_type}/{image_index}",
    ] {
        cfg.service(
            web::resource(path)
                .app_data(web::PayloadConfig::new(MAX_BODY_BYTES))
                .route(web::get().to(get_image))
                .route(web::head().to(get_image))
                .route(web::post().to(post_image))
                .route(web::delete().to(delete_image)),
        );
    }
}

#[derive(Debug, Default, serde::Deserialize)]
struct ImageQuery {
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    tag: Option<String>,
}

fn parse_user_id(raw: &str) -> Result<UserId, actix_web::Error> {
    Uuid::parse_str(raw)
        .map(UserId)
        .map_err(|_| error::ErrorBadRequest("invalid user id"))
}

/// The user a request names: the path's `{user_id}`, else `userId`.
fn named_user(req: &HttpRequest, query: &ImageQuery) -> Option<Result<UserId, actix_web::Error>> {
    req.match_info()
        .get("user_id")
        .or(query.user_id.as_deref())
        .map(parse_user_id)
}

/// Only a user's Primary image exists; any other type is not found.
fn check_type(req: &HttpRequest) -> Result<(), actix_web::Error> {
    match req.match_info().get("image_type") {
        None => Ok(()),
        Some(t) if t.eq_ignore_ascii_case("primary") => Ok(()),
        Some(_) => Err(error::ErrorNotFound("no such user image")),
    }
}

/// The user a write is for, when `caller` may change them: themselves, or
/// anyone when they are an admin.
async fn writable_target(
    state: &AppState,
    req: &HttpRequest,
    query: &ImageQuery,
    caller: &AuthUser,
) -> Result<UserId, actix_web::Error> {
    check_type(req)?;
    let target = match named_user(req, query) {
        Some(id) => id?,
        None => caller.0.id,
    };
    if target == caller.0.id {
        return Ok(target);
    }
    if !caller.0.policy.admin {
        tracing::warn!(user = %caller.0.name, target = %target.0.simple(), "refused another user's avatar change");
        return Err(error::ErrorForbidden(
            "user is not allowed to change this image",
        ));
    }
    match state.stores.get(target).await {
        Ok(_) => Ok(target),
        Err(pharos_core::AuthError::UserNotFound) => Err(error::ErrorNotFound("user not found")),
        Err(e) => {
            tracing::warn!(target = %target.0.simple(), error = %e, "avatar: user lookup failed");
            Err(error::ErrorServiceUnavailable("user store unavailable"))
        }
    }
}

async fn get_image(
    state: web::Data<AppState>,
    req: HttpRequest,
    q: CiQuery<ImageQuery>,
) -> Result<HttpResponse, actix_web::Error> {
    check_type(&req)?;
    let Some(user) = named_user(&req, &q.0) else {
        return Err(error::ErrorBadRequest("UserId is required"));
    };
    let user = user?;
    let found = avatar::load(&state.stores, user).await.map_err(|e| {
        tracing::warn!(user.id = %user.0.simple(), error = %e, "avatar: read failed");
        error::ErrorServiceUnavailable("user store unavailable")
    })?;
    let Some(found) = found else {
        return Err(error::ErrorNotFound("no user image"));
    };
    let etag = format!("\"{}\"", found.tag);
    let not_modified = req
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|inm| inm.split(',').map(str::trim).any(|t| t == etag || t == "*"));
    let cache = if q.0.tag.as_deref() == Some(found.tag.as_str()) {
        format!("public, max-age={TAGGED_MAX_AGE_SECS}, immutable")
    } else {
        "no-cache".to_owned()
    };
    let mut resp = if not_modified {
        HttpResponse::NotModified()
    } else {
        HttpResponse::Ok()
    };
    resp.insert_header((header::ETAG, etag))
        .insert_header((header::CACHE_CONTROL, cache))
        .insert_header((header::X_CONTENT_TYPE_OPTIONS, "nosniff"));
    if not_modified {
        return Ok(resp.finish());
    }
    resp.content_type(found.content_type);
    if req.method() == actix_web::http::Method::HEAD {
        return Ok(resp.finish());
    }
    Ok(resp.body(found.bytes))
}

/// The picture in an upload body: base64 as Jellyfin sends it (with or
/// without a `data:` URL prefix), or the picture's own bytes. Runs in the
/// decode slot ([`avatar::Unwrap`]), not on the actix worker.
fn upload_bytes(body: &[u8]) -> Result<Cow<'_, [u8]>, AvatarError> {
    if image::guess_format(body).is_ok() {
        return Ok(Cow::Borrowed(body));
    }
    let text: Vec<u8> = body
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    let encoded = match text.iter().position(|&b| b == b',') {
        Some(comma) if text.starts_with(b"data:") => {
            text.get(comma.saturating_add(1)..).unwrap_or_default()
        }
        _ => text.as_slice(),
    };
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map(Cow::Owned)
        .map_err(|_| AvatarError::Unsupported)
}

fn refusal(e: &AvatarError) -> actix_web::Error {
    match e {
        AvatarError::Empty | AvatarError::Unsupported | AvatarError::Decode(_) => {
            error::ErrorBadRequest(e.to_string())
        }
        AvatarError::TooLarge { .. } => error::ErrorPayloadTooLarge(e.to_string()),
        AvatarError::Busy => error::ErrorServiceUnavailable(e.to_string()),
        AvatarError::Encode(_) | AvatarError::Store(_) => {
            error::ErrorInternalServerError("could not store the image")
        }
    }
}

async fn post_image(
    state: web::Data<AppState>,
    req: HttpRequest,
    caller: AuthUser,
    q: CiQuery<ImageQuery>,
    body: web::Bytes,
) -> Result<HttpResponse, actix_web::Error> {
    let target = writable_target(&state, &req, &q.0, &caller).await?;
    avatar::set(&state.stores, target, body, upload_bytes)
        .await
        .map_err(|e| refusal(&e))?;
    Ok(HttpResponse::NoContent().finish())
}

async fn delete_image(
    state: web::Data<AppState>,
    req: HttpRequest,
    caller: AuthUser,
    q: CiQuery<ImageQuery>,
) -> Result<HttpResponse, actix_web::Error> {
    let target = writable_target(&state, &req, &q.0, &caller).await?;
    avatar::remove(&state.stores, target).await.map_err(|e| {
        tracing::warn!(user.id = %target.0.simple(), error = %e, "avatar: remove failed");
        error::ErrorServiceUnavailable("user store unavailable")
    })?;
    Ok(HttpResponse::NoContent().finish())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

    #[test]
    fn an_upload_is_base64_with_or_without_a_data_url_or_raw_bytes() {
        let raw = [PNG_MAGIC, b"rest"].concat();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
        assert_eq!(upload_bytes(&raw).unwrap(), raw);
        assert_eq!(upload_bytes(b64.as_bytes()).unwrap(), raw);
        let wrapped = format!("data:image/png;base64,{}\n{}\r\n", &b64[..4], &b64[4..]);
        assert_eq!(upload_bytes(wrapped.as_bytes()).unwrap(), raw);
        assert!(matches!(
            upload_bytes(b"not base64!"),
            Err(AvatarError::Unsupported)
        ));
    }

    #[test]
    fn the_body_limit_admits_a_full_size_picture_in_base64() {
        assert!(MAX_BODY_BYTES >= avatar::MAX_UPLOAD_BYTES.div_ceil(3) * 4);
    }
}
