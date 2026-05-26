//! Direct-play streaming endpoints. Hands off to `actix_files::NamedFile`,
//! which provides byte ranges, content-type sniffing, ETags, and 206
//! Partial Content for free. Transcoded streaming (HLS) lands in T9.
//!
//! V9: the stored `MediaItem.path` is treated as authoritative — its
//! provenance is the scanner-walked media roots (T3). Anything reaching
//! the `MediaStore` from elsewhere must validate root-prefix at the
//! call site; tracked in §B if violated.

use crate::{api::jellyfin::auth_extractor::AuthUser, state::AppState};
use actix_files::NamedFile;
use actix_web::{error, web, HttpRequest};
use pharos_core::MediaStore;

pub fn register(cfg: &mut web::ServiceConfig) {
    cfg.route("/Videos/{id}/stream", web::get().to(stream_video))
        .route("/Videos/{id}/stream.{ext}", web::get().to(stream_video))
        .route("/Audio/{id}/stream", web::get().to(stream_audio))
        .route("/Audio/{id}/universal", web::get().to(stream_audio));
}

async fn stream_video(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
    path: web::Path<StreamPath>,
) -> Result<NamedFile, actix_web::Error> {
    open_item(&state, path.id_str()).await.map(|f| f.into_response_file(&req))
}

async fn stream_audio(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
    path: web::Path<StreamPath>,
) -> Result<NamedFile, actix_web::Error> {
    open_item(&state, path.id_str()).await.map(|f| f.into_response_file(&req))
}

#[derive(serde::Deserialize)]
struct StreamPath {
    id: String,
    #[serde(default)]
    #[allow(dead_code)]
    ext: Option<String>,
}

impl StreamPath {
    fn id_str(&self) -> &str {
        &self.id
    }
}

struct OpenItem(NamedFile);

impl OpenItem {
    fn into_response_file(self, _req: &HttpRequest) -> NamedFile {
        // Use the byte-range support actix-files gives us for free.
        self.0.use_etag(true).use_last_modified(true)
    }
}

async fn open_item(state: &AppState, id_str: &str) -> Result<OpenItem, actix_web::Error> {
    let id: u64 = id_str
        .parse()
        .map_err(|_| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    let file = NamedFile::open_async(&item.path)
        .await
        .map_err(|e| error::ErrorNotFound(e.to_string()))?;
    Ok(OpenItem(file))
}
