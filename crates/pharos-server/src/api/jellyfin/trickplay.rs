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

use crate::{
    api::jellyfin::auth_extractor::AuthUser,
    state::AppState,
    trickplay_cache::{Layout, TrickplayCacheError},
};
use actix_web::{error, web, HttpResponse};
use pharos_core::MediaStore;

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

/// Compose a Layout for the requested width when probe has the data we
/// need. Returns None when duration_ms / dimensions are missing — the
/// route then 404s rather than 500.
pub fn build_layout(
    probe: &pharos_core::MediaProbe,
    width: u32,
    interval_ms: u32,
) -> Option<Layout> {
    let duration_ms = probe.duration_ms?;
    let src_w = probe.width?;
    let src_h = probe.height?;
    Layout::compute(duration_ms, src_w, src_h, width, interval_ms)
}

/// Render `BaseItemDto.Trickplay` for a video item. Returns the
/// `{ width_str → TileInfo }` map; empty when no width yields a valid
/// layout (no probe data, audio-only item, or widths unconfigured).
///
/// Wire shape per width:
/// ```json
/// "320": {
///   "Width": 320, "Height": 180,
///   "TileWidth": 10, "TileHeight": 10,
///   "ThumbnailCount": 89, "Interval": 10000, "Bandwidth": 0
/// }
/// ```
pub fn build_dto_layout_map(
    probe: &pharos_core::MediaProbe,
    widths: &[u32],
    interval_ms: u32,
) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    for &w in widths {
        if let Some(layout) = build_layout(probe, w, interval_ms) {
            out.insert(
                w.to_string(),
                serde_json::json!({
                    "Width": layout.width,
                    "Height": layout.height,
                    "TileWidth": crate::trickplay_cache::TILE_GRID,
                    "TileHeight": crate::trickplay_cache::TILE_GRID,
                    "ThumbnailCount": layout.thumb_count,
                    "Interval": layout.interval_ms,
                    "Bandwidth": 0u64,
                }),
            );
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use pharos_core::MediaProbe;

    fn probe_1080p_10min() -> MediaProbe {
        MediaProbe {
            duration_ms: Some(10 * 60 * 1000),
            width: Some(1920),
            height: Some(1080),
            ..Default::default()
        }
    }

    #[test]
    fn dto_layout_map_emits_one_entry_per_configured_width() {
        let probe = probe_1080p_10min();
        let map = build_dto_layout_map(&probe, &[320, 640], 10_000);
        assert_eq!(map.len(), 2);
        let v320 = map.get("320").unwrap();
        assert_eq!(v320.get("Width").unwrap().as_u64().unwrap(), 320);
        assert_eq!(v320.get("Height").unwrap().as_u64().unwrap(), 180);
        assert_eq!(v320.get("Interval").unwrap().as_u64().unwrap(), 10_000);
        assert_eq!(v320.get("TileWidth").unwrap().as_u64().unwrap(), 10);
        // 10 min @ 10s = 60 thumbs.
        assert_eq!(v320.get("ThumbnailCount").unwrap().as_u64().unwrap(), 60);
    }

    #[test]
    fn dto_layout_map_empty_when_probe_lacks_dimensions() {
        let probe = MediaProbe {
            duration_ms: Some(60_000),
            width: None,
            height: None,
            ..Default::default()
        };
        let map = build_dto_layout_map(&probe, &[320], 10_000);
        assert!(map.is_empty());
    }

    #[test]
    fn dto_layout_map_empty_when_no_widths_configured() {
        let probe = probe_1080p_10min();
        let map = build_dto_layout_map(&probe, &[], 10_000);
        assert!(map.is_empty());
    }
}
