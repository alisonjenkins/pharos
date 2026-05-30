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

    let layout = build_layout(&item.probe, width, state.trickplay_interval_ms)
        .ok_or_else(|| error::ErrorNotFound("no trickplay layout (missing probe data)"))?;

    let bytes = cache
        .tile_bytes(id_num, layout, tile_index, &item.path)
        .await
        .map_err(map_cache_err)?;
    Ok(HttpResponse::Ok().content_type("image/jpeg").body(bytes))
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
