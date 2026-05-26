//! /Items and /Library item-browsing routes.
//!
//! Phase-1 scope: list, get-by-id, per-user list, virtual-folders summary.
//! Filters/search land in later T6 follow-ups.

use crate::{
    api::jellyfin::{
        auth_extractor::AuthUser,
        dto::{BaseItemDto, ItemsResultDto, VirtualFolderInfoDto, VirtualFolderOptionsDto},
    },
    state::AppState,
};
use actix_web::{error, web, HttpResponse, Responder};
use pharos_core::MediaStore;
use serde::Deserialize;

pub fn register(cfg: &mut web::ServiceConfig) {
    cfg.route("/Items", web::get().to(list_items))
        .route("/Items/{id}", web::get().to(get_item))
        .route("/Users/{user_id}/Items", web::get().to(list_user_items))
        .route("/Library/VirtualFolders", web::get().to(virtual_folders));
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ListQuery {
    #[serde(default)]
    start_index: u32,
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_limit() -> u32 {
    100
}

async fn list_items(
    state: web::Data<AppState>,
    _user: AuthUser,
    q: web::Query<ListQuery>,
) -> Result<impl Responder, actix_web::Error> {
    let all = state
        .stores
        .list()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::Ok().json(paginate(all, &state.server_id, q.start_index, q.limit)))
}

async fn list_user_items(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
    q: web::Query<ListQuery>,
) -> Result<impl Responder, actix_web::Error> {
    // V9 spirit: the path user must match the bearer. Reject mismatched.
    let user_path = path.into_inner();
    let bearer_id = user.0.id.0.simple().to_string();
    if user_path != bearer_id {
        return Err(error::ErrorForbidden("user mismatch"));
    }
    let all = state
        .stores
        .list()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::Ok().json(paginate(all, &state.server_id, q.start_index, q.limit)))
}

async fn get_item(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    let id_str = path.into_inner();
    let id: u64 = id_str
        .parse()
        .map_err(|_| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    Ok(HttpResponse::Ok().json(BaseItemDto::from_domain(&item, &state.server_id)))
}

async fn virtual_folders(
    state: web::Data<AppState>,
    _user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    // Phase 1: report a single synthesized "All Media" library covering the
    // entire store. Real per-root libraries land with media-roots wiring.
    let folder = VirtualFolderInfoDto {
        name: "All Media".into(),
        locations: vec![],
        collection_type: "mixed",
        item_id: "00000000000000000000000000000000".into(),
        library_options: VirtualFolderOptionsDto::default(),
    };
    let _ = &state.stores;
    Ok(HttpResponse::Ok().json(vec![folder]))
}

fn paginate(
    all: Vec<pharos_core::MediaItem>,
    server_id: &str,
    start_index: u32,
    limit: u32,
) -> ItemsResultDto {
    let total = all.len() as u32;
    let start = start_index as usize;
    let end = (start + limit as usize).min(all.len());
    let slice = if start >= all.len() {
        &[][..]
    } else {
        &all[start..end]
    };
    let items: Vec<BaseItemDto> = slice
        .iter()
        .map(|i| BaseItemDto::from_domain(i, server_id))
        .collect();
    ItemsResultDto {
        items,
        total_record_count: total,
        start_index,
    }
}
