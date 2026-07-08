//! Jellyfin Trickplay routes.
//!
//! Wire shape:
//! - `GET /videos/{id}/trickplay/{width}/{tile}.jpg` — one sprite-grid
//!   tile JPEG. `tile` is 0-based. Auth via the standard
//!   `AuthUser` extractor (Emby headers or `api_key` query param).
//!
//! The per-item sprite layout is advertised inline in
//! `BaseItemDto.Trickplay`; clients discover widths + thumbnail count +
//! interval from there. No separate manifest endpoint.

use crate::{api::jellyfin::auth_extractor::AuthUser, state::AppState};
use actix_web::{error, web, HttpResponse};
use pharos_cache::trickplay_cache::TrickplayCacheError;
use pharos_core::MediaStore;
use pharos_jellyfin_api::dto::build_layout;

pub fn register(cfg: &mut web::ServiceConfig) {
    cfg.route(
        "/videos/{id}/trickplay/{width}/{tile}.jpg",
        web::get().to(tile),
    );
}

async fn tile(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<(String, u32, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, width, tile_index) = path.into_inner();

    if !state.trickplay_widths.contains(&width) {
        return Err(error::ErrorNotFound("unknown trickplay width"));
    }
    let cache = state
        .trickplay
        .as_ref()
        .ok_or_else(|| error::ErrorNotFound("trickplay disabled"))?;

    let id_num: u64 = id
        .parse()
        .map_err(|_| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id_num).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;

    // Validate the tile index against the layout (cheap; probe-derived).
    let layout = build_layout(&item.probe, width, state.trickplay_interval_ms)
        .ok_or_else(|| error::ErrorNotFound("no trickplay layout (missing probe data)"))?;
    if tile_index >= layout.tile_count {
        return Err(error::ErrorNotFound("tile index out of range"));
    }

    // Serve only what the background pre-generator has already produced —
    // never generate on the request path. Whole-video sprite generation is
    // slow (tens of seconds) + CPU-heavy, so doing it inline blocked the
    // request past the ingress timeout (504) and stole CPU from playback. A
    // 404 here just means "no preview yet"; jellyfin-web degrades gracefully.
    match cache
        .tile_bytes_cached(id_num, width, tile_index)
        .await
        .map_err(map_cache_err)?
    {
        Some(bytes) => Ok(HttpResponse::Ok().content_type("image/jpeg").body(bytes)),
        None => Err(error::ErrorNotFound("trickplay not generated yet")),
    }
}

fn map_cache_err(e: TrickplayCacheError) -> actix_web::Error {
    match e {
        TrickplayCacheError::TileOutOfRange(_, _) => {
            error::ErrorNotFound("tile index out of range")
        }
        TrickplayCacheError::UnknownDuration => error::ErrorNotFound("source duration unknown"),
        other => error::ErrorInternalServerError(format!("trickplay: {other}")),
    }
}
