//! Jellyfin `/Playlists` controller (T70).
//!
//! A playlist is a user-owned, ordered, duplicate-allowing container —
//! distinct from a collection / box set (a name-hashed set). jellyfin-web's
//! "Add to playlist" flow and the Playlists view drive these endpoints:
//! create, fetch the header item, list members (each tagged with its
//! `PlaylistItemId` so the client can remove / reorder one slot), append,
//! remove-by-entry, move, and delete.
//!
//! Persistence lives in [`pharos_core::PlaylistStore`]; the member listing
//! reuses [`items::build_items_page`] so playlist rows carry the same
//! `BaseItemDto` shape (user-data, trickplay, parent ids) as every other
//! `/Items` surface, then overlays the per-entry `PlaylistItemId`.

use crate::{
    api::jellyfin::{auth_extractor::AuthUser, items},
    state::AppState,
};
use actix_web::{error, web, HttpResponse, Responder};
use pharos_core::{MediaStore, PlaylistStore};
use serde::Deserialize;

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31 lowercase routes; the `LowercasePath` middleware folds the
    // PascalCase requests jellyfin-web sends onto these. Path params here
    // (wire id, entry id, index) are already lowercase-safe: wire/entry ids
    // are lowercase-hex UUIDs and the index is digits.
    cfg.route("/playlists", web::post().to(create_playlist))
        .route("/playlists/{id}", web::get().to(get_playlist))
        .route("/playlists/{id}", web::delete().to(delete_playlist))
        .route("/playlists/{id}/items", web::get().to(playlist_items))
        .route("/playlists/{id}/items", web::post().to(add_items))
        .route("/playlists/{id}/items", web::delete().to(remove_items))
        .route(
            "/playlists/{id}/items/{entry_id}/move/{new_index}",
            web::post().to(move_item),
        );
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct CreatePlaylistQuery {
    name: Option<String>,
    /// Comma-separated media ids to seed the playlist with, in order.
    ids: Option<String>,
    /// `Audio` for a music queue, else `Video`. Jellyfin sends this.
    media_type: Option<String>,
}

/// `POST /Playlists?Name=&Ids=&MediaType=` — create a playlist owned by the
/// bearer, seeded with `Ids`. Returns Jellyfin's `PlaylistCreationResult`
/// (`{ "Id": <wire id> }`).
async fn create_playlist(
    state: web::Data<AppState>,
    user: AuthUser,
    q: web::Query<CreatePlaylistQuery>,
) -> Result<impl Responder, actix_web::Error> {
    let name = q
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("New Playlist");
    let media_type = q
        .media_type
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Video");
    let ids = items::parse_id_csv(q.ids.as_deref());
    let owner = user.0.id.0.simple().to_string();
    let playlist = state
        .stores
        .create_playlist(name, Some(&owner), media_type, &ids)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::Ok().json(serde_json::json!({ "Id": playlist.wire_id })))
}

/// `GET /Playlists/{id}` — the playlist header as a `Playlist` folder item.
async fn get_playlist(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    let wire_id = path.into_inner();
    let playlist = state
        .stores
        .playlist_by_wire_id(&wire_id)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?
        .ok_or_else(|| error::ErrorNotFound("playlist not found"))?;
    let count = state
        .stores
        .playlist_entries(&wire_id)
        .await
        .map(|e| e.len() as u32)
        .unwrap_or(0);
    Ok(HttpResponse::Ok().json(playlist_dto(&state, &playlist, count)))
}

/// `GET /Playlists/{id}/Items` — the members in curated order, each tagged
/// with its `PlaylistItemId` (the per-entry id the client removes / moves).
async fn playlist_items(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    let wire_id = path.into_inner();
    // 404 when the playlist itself is absent (distinct from an empty one).
    if state
        .stores
        .playlist_by_wire_id(&wire_id)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?
        .is_none()
    {
        return Err(error::ErrorNotFound("playlist not found"));
    }
    let entries = state
        .stores
        .playlist_entries(&wire_id)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    // Resolve each entry to its media item, preserving order + duplicates.
    // A dangling entry (media row deleted) is skipped but keeps its slot out
    // of the response — its entry_id simply doesn't appear.
    let mut resolved: Vec<(String, pharos_core::MediaItem)> = Vec::with_capacity(entries.len());
    for e in entries {
        if let Ok(item) = state.stores.get(e.item_id).await {
            resolved.push((e.entry_id, item));
        }
    }
    let media: Vec<pharos_core::MediaItem> = resolved.iter().map(|(_, m)| m.clone()).collect();
    let total = media.len() as u32;
    let page = items::build_items_page(&state, user.0.id, &media, total, 0).await?;
    // Overlay PlaylistItemId per entry — build_items_page yields the generic
    // BaseItemDto; the client needs the per-entry id to target a slot.
    let mut value =
        serde_json::to_value(&page).map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    if let Some(arr) = value.get_mut("Items").and_then(|v| v.as_array_mut()) {
        for (dto, (entry_id, _)) in arr.iter_mut().zip(resolved.iter()) {
            if let Some(obj) = dto.as_object_mut() {
                obj.insert(
                    "PlaylistItemId".to_string(),
                    serde_json::Value::String(entry_id.clone()),
                );
            }
        }
    }
    Ok(HttpResponse::Ok().json(value))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct AddItemsQuery {
    ids: Option<String>,
}

/// `POST /Playlists/{id}/Items?Ids=` — append media ids (each a new entry).
async fn add_items(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
    q: web::Query<AddItemsQuery>,
) -> Result<impl Responder, actix_web::Error> {
    let wire_id = path.into_inner();
    let ids = items::parse_id_csv(q.ids.as_deref());
    let added = state
        .stores
        .add_playlist_items(&wire_id, &ids)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    if added.is_none() {
        return Err(error::ErrorNotFound("playlist not found"));
    }
    Ok(HttpResponse::NoContent().finish())
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct RemoveItemsQuery {
    /// Comma-separated per-entry ids (Jellyfin's `EntryIds`).
    entry_ids: Option<String>,
}

/// `DELETE /Playlists/{id}/Items?EntryIds=` — remove specific entries.
async fn remove_items(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
    q: web::Query<RemoveItemsQuery>,
) -> Result<impl Responder, actix_web::Error> {
    let wire_id = path.into_inner();
    let entry_ids = parse_str_csv(q.entry_ids.as_deref());
    let removed = state
        .stores
        .remove_playlist_entries(&wire_id, &entry_ids)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    if removed.is_none() {
        return Err(error::ErrorNotFound("playlist not found"));
    }
    Ok(HttpResponse::NoContent().finish())
}

/// `POST /Playlists/{id}/Items/{entryId}/Move/{newIndex}` — reorder an entry.
async fn move_item(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<(String, String, usize)>,
) -> Result<impl Responder, actix_web::Error> {
    let (wire_id, entry_id, new_index) = path.into_inner();
    let outcome = state
        .stores
        .move_playlist_entry(&wire_id, &entry_id, new_index)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    match outcome {
        None => Err(error::ErrorNotFound("playlist not found")),
        Some(false) => Err(error::ErrorNotFound("playlist entry not found")),
        Some(true) => Ok(HttpResponse::NoContent().finish()),
    }
}

/// `DELETE /Playlists/{id}` — delete the playlist and its entries.
async fn delete_playlist(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    let wire_id = path.into_inner();
    let deleted = state
        .stores
        .delete_playlist(&wire_id)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    if deleted.is_none() {
        return Err(error::ErrorNotFound("playlist not found"));
    }
    Ok(HttpResponse::NoContent().finish())
}

/// The playlist header `BaseItemDto`. A Jellyfin playlist is an `IsFolder`
/// `Playlist` item — opening it lists members via `/Playlists/{id}/Items`
/// (and the `/Items?ParentId=<wire id>` pivot). `Id` is the wire id so it
/// round-trips byte-identically.
fn playlist_dto(
    state: &AppState,
    playlist: &pharos_core::Playlist,
    child_count: u32,
) -> serde_json::Value {
    serde_json::json!({
        "Id": playlist.wire_id,
        "Name": playlist.name,
        "ServerId": state.server_id,
        "Type": "Playlist",
        "MediaType": playlist.media_type,
        "IsFolder": true,
        "ChildCount": child_count,
        "CanDelete": true,
        "ImageTags": {},
        "BackdropImageTags": [],
        "Genres": [], "Tags": [],
    })
}

/// Split a comma-separated id list, trimming blanks. Used for `EntryIds`
/// (opaque string ids — unlike the numeric [`items::parse_id_csv`]).
fn parse_str_csv(raw: Option<&str>) -> Vec<String> {
    raw.map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default()
}
