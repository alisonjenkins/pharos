//! `POST /Pharos/Remote/Items` — catalogue a video by its URL (008).
//!
//! Deliberately NOT a Jellyfin route. Jellyfin has no concept of adding a
//! library item by URL, so there is no wire shape to be compatible with, and
//! inventing one under `/Items` would put a pharos-only body on a path clients
//! already have expectations about.
//!
//! # Why not `/Library/VirtualFolders`
//!
//! That endpoint spawns a scan for any path outside an existing root, so
//! reaching the library through it would kick a filesystem walk of `ytdlp://`.
//! The library row and the item are therefore written directly.
//!
//! # Why the library root is `ytdlp:`
//!
//! `Path::starts_with` is component-wise, and a synthetic path's first
//! component is exactly `ytdlp:` — so the store's existing path-prefix
//! machinery (`backfill_library_ids`) puts every remote item in this library
//! with no special-casing, and `sweep_unseen` never walks it because scan roots
//! come from `[media]` config, not from library rows (V136).

use crate::api::jellyfin::auth_extractor::AuthUser;
use crate::remote::ResolveError;
use crate::state::AppState;
use actix_web::{error, web, HttpResponse};
use pharos_core::{LibraryKind, LibraryStore, MediaItem, MediaKind, MediaStore};
use serde::{Deserialize, Serialize};

/// The library every URL-backed item lands in.
const REMOTE_LIBRARY_NAME: &str = "Web Videos";
/// Root for that library. See the module docs — this is a path PREFIX that no
/// filesystem walk will ever visit, not a directory.
const REMOTE_LIBRARY_ROOT: &str = "ytdlp:";

#[derive(Debug, Deserialize)]
pub struct AddRemoteBody {
    /// The page URL, as a person would paste it.
    pub url: String,
}

#[derive(Debug, Serialize)]
pub struct AddRemoteResponse {
    /// The wire id, ready to put straight into a SyncPlay queue.
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "RunTimeTicks")]
    pub run_time_ticks: Option<u64>,
    /// True when this URL was already in the library. The endpoint is
    /// idempotent — re-adding refreshes the metadata and returns the same id —
    /// so a caller that retries on a timeout cannot create a duplicate.
    #[serde(rename = "AlreadyPresent")]
    pub already_present: bool,
}

pub fn routes(cfg: &mut web::ServiceConfig) {
    cfg.route("/pharos/remote/items", web::post().to(add_remote_item));
}

/// Resolve a URL's metadata and write it as a library item.
///
/// Metadata only — nothing is downloaded here, and the media locator is not
/// stored. The row holds a stable `ytdlp://<extractor>/<id>`, and the real
/// locator is resolved fresh at playback because it is signed and rotates.
async fn add_remote_item(
    state: web::Data<AppState>,
    _user: AuthUser,
    body: web::Json<AddRemoteBody>,
) -> Result<HttpResponse, actix_web::Error> {
    let Some(resolver) = state.remote.as_ref() else {
        return Err(error::ErrorServiceUnavailable(
            ResolveError::Disabled.to_string(),
        ));
    };
    let url = body.url.trim();
    if url.is_empty() {
        return Err(error::ErrorBadRequest("url is required"));
    }

    let resolved = resolver.describe(url).await.map_err(|e| {
        tracing::warn!(url, error = %e, "could not catalogue a URL");
        // The resolver's message carries the site's own reason; a bare class
        // would leave a person guessing why their link was refused.
        error::ErrorBadGateway(format!("could not read {url}: {e}"))
    })?;

    let path = resolved.reference.to_synthetic_path();
    let id = pharos_scanner::fs::stable_id(&path);
    let already_present = state.stores.get(id).await.is_ok();

    // The library row, upserted rather than assumed: the first URL anyone adds
    // creates it, and every one after finds it.
    let wire_id =
        crate::api::jellyfin::items::library_id_for_root(std::path::Path::new(REMOTE_LIBRARY_ROOT));
    state
        .stores
        .upsert_library(
            REMOTE_LIBRARY_NAME,
            REMOTE_LIBRARY_ROOT,
            // Mixed rather than Movies: what lands here is whatever someone
            // pasted, and claiming a collection type the contents do not honour
            // makes jellyfin-web render the wrong grid.
            LibraryKind::Mixed,
            &wire_id,
        )
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("library: {e}")))?;

    state
        .stores
        .put(MediaItem {
            id,
            path,
            title: resolved.title.clone(),
            // Movie is the kind that renders as a single playable title. The
            // alternative would be a new MediaKind, which every DTO projection
            // and every client would then have to learn.
            kind: MediaKind::Movie,
            probe: resolved.probe.clone(),
            created_at: None,
            ..Default::default()
        })
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("store: {e}")))?;

    // Stamp library_id by the same path-prefix rule every other item uses,
    // rather than writing it here — one mechanism, so a change to how libraries
    // are assigned cannot leave this path behind.
    if let Err(e) = state.stores.backfill_library_ids().await {
        tracing::warn!(error = %e, "could not assign the new item to its library");
    }

    tracing::info!(
        media.id = id,
        extractor = resolved.reference.extractor(),
        title = %resolved.title,
        already_present,
        "catalogued a URL-backed item",
    );
    metrics::counter!(
        "pharos_remote_ingest_total",
        "extractor" => resolved.reference.extractor().to_string(),
        "outcome" => if already_present { "refreshed" } else { "added" },
    )
    .increment(1);

    Ok(HttpResponse::Ok().json(AddRemoteResponse {
        id: pharos_jellyfin_api::dto::wire_item_id(id),
        name: resolved.title,
        run_time_ticks: resolved.probe.duration_ms.map(|ms| ms * 10_000),
        already_present,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The library root must be a prefix of every synthetic path and of no
    /// filesystem path — the property that lets the existing path-prefix
    /// machinery file these items with no special-casing, while keeping them
    /// out of reach of any scan (V136).
    #[test]
    fn the_remote_library_root_captures_synthetic_paths_and_nothing_else() {
        let synthetic = pharos_core::RemoteRef::new("youtube", "dQw4w9WgXcQ")
            .expect("valid ref")
            .to_synthetic_path();
        assert!(
            synthetic.starts_with(REMOTE_LIBRARY_ROOT),
            "the library root must capture a synthetic path: {}",
            synthetic.display()
        );
        // A different site still lands in the same library.
        let other = pharos_core::RemoteRef::new("vimeo", "76979871")
            .expect("valid ref")
            .to_synthetic_path();
        assert!(other.starts_with(REMOTE_LIBRARY_ROOT));

        // And it captures nothing real. If it did, a scan of that root would
        // walk it, find none of these rows, and sweep them all — below B98's
        // blast-radius guard.
        for real in [
            "/media",
            "/media/Movies/Arrival.mkv",
            "/var/lib/pharos/media",
        ] {
            assert!(
                !std::path::Path::new(real).starts_with(REMOTE_LIBRARY_ROOT),
                "{real} must not fall in the remote library"
            );
        }
    }

    /// Two adds of the same URL produce the same id, so a retry cannot
    /// duplicate a title.
    #[test]
    fn the_same_url_always_yields_the_same_item_id() {
        let a = pharos_core::RemoteRef::new("youtube", "dQw4w9WgXcQ")
            .expect("valid ref")
            .to_synthetic_path();
        let b = pharos_core::RemoteRef::new("youtube", "dQw4w9WgXcQ")
            .expect("valid ref")
            .to_synthetic_path();
        assert_eq!(
            pharos_scanner::fs::stable_id(&a),
            pharos_scanner::fs::stable_id(&b)
        );
        // A different video is a different item.
        let c = pharos_core::RemoteRef::new("youtube", "aqz-KE-bpKQ")
            .expect("valid ref")
            .to_synthetic_path();
        assert_ne!(
            pharos_scanner::fs::stable_id(&a),
            pharos_scanner::fs::stable_id(&c)
        );
    }
}
