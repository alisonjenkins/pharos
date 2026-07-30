//! `GET /Items/{itemId}/Download` — serve an item's file bytes (004-books, FR-004).
//!
//! This is the URL every jellyfin-web reader constructs. `getItemDownloadUrl`
//! builds `Items/{id}/Download?api_key=<token>` with **no `Authorization`
//! header**, so query auth is not optional here — the existing extractor
//! already accepts `api_key`, which is why this handler takes a plain
//! [`AuthUser`].
//!
//! Not restricted to books. Real Jellyfin's `/Download` serves any item and
//! `CanDownload` is advertised generally; refusing anything but `Type=Book`
//! would be a gratuitous divergence from the client's expectations.
//!
//! # Why `NamedFile`
//!
//! Range support, `Accept-Ranges`, `ETag`, `Last-Modified` and conditional
//! requests all come for free — but the load-bearing reason is
//! `Content-Length`. actix's h1 encoder derives that header from the BODY's
//! declared `BodySize` and **discards** a hand-set one, so a handler that
//! answers a HEAD with `.finish()` advertises `Content-Length: 0` no matter
//! what it inserted. That is B166 on the image path and B101 on the video path;
//! V113 exists because it has now been written twice. `NamedFile`'s body is a
//! `SizedStream` carrying `BodySize::Sized(file_len)`, so the correct length is
//! structural rather than remembered, and its reader is never polled on a HEAD.

use actix_files::NamedFile;
use actix_web::{error, http::header, web, HttpRequest, HttpResponse, Responder};

use pharos_core::MediaStore;

use super::auth_extractor::AuthUser;
use crate::state::AppState;

pub fn register(cfg: &mut web::ServiceConfig) {
    cfg.route("/items/{id}/download", web::get().to(download))
        .route("/items/{id}/download", web::head().to(download));
}

/// Extension → `Content-Type` for the formats a client actually opens.
///
/// The book MIME types matter: `comicsPlayer` and `bookPlayer` fetch these
/// bytes as an `ArrayBuffer` and sniff content themselves, but a browser that
/// sees `application/octet-stream` on a same-origin navigation may offer a
/// download dialog instead. Everything unrecognised stays
/// `application/octet-stream` rather than guessing.
fn content_type_for(path: &std::path::Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "epub" => "application/epub+zip",
        "pdf" => "application/pdf",
        "cbz" => "application/vnd.comicbook+zip",
        "cbr" => "application/vnd.comicbook-rar",
        "cbt" => "application/x-cbt",
        "cb7" => "application/x-cb7",
        "mobi" => "application/x-mobipocket-ebook",
        "azw3" => "application/vnd.amazon.ebook",
        _ => "application/octet-stream",
    }
}

/// `Content-Disposition: attachment; filename="…"`.
///
/// The filename is the stored basename. It is quoted and any `"` or `\` is
/// stripped rather than escaped — a header value cannot carry a raw quote
/// without breaking parsing, and a filename containing one is vanishingly rare
/// next to the cost of getting the quoting subtly wrong. Non-ASCII names are
/// left as-is: actix rejects invalid header bytes, and the fallback below keeps
/// the response valid if that happens.
///
/// 005-kindle-conversion — `served` is what is actually going over the wire,
/// which for a converted book is a cache file named after the item id. The
/// NAME comes from the source (the reader saving it wants "The Prince", not
/// "42") while the EXTENSION comes from what is being sent, so the saved file
/// is not an `.azw` full of EPUB bytes that no desktop reader will open.
fn disposition_for(source: &std::path::Path, served: &std::path::Path) -> String {
    let stem = source
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("download");
    let name = match served.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{stem}.{ext}"),
        None => stem.to_string(),
    };
    let name: String = name.chars().filter(|c| *c != '"' && *c != '\\').collect();
    format!("attachment; filename=\"{name}\"")
}

async fn download(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    let id_str = path.into_inner();
    // V9 — the request carries an ID, never a path. The path comes out of the
    // store, so there is no traversal to block: nothing user-supplied reaches
    // the filesystem.
    let id = pharos_jellyfin_api::dto::parse_item_id(&id_str)
        .ok_or_else(|| error::ErrorNotFound("unknown item"))?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("unknown item"),
        // Expose the cause: a store failure here is not a missing item, and
        // reporting it as 404 would send a reader hunting for the wrong thing.
        other => error::ErrorInternalServerError(other.to_string()),
    })?;

    // 005-kindle-conversion — a Kindle book was stored as `Epub` because it
    // converts, so this route must hand over the EPUB and not the `.azw3` the
    // item still points at. Converting here rather than at scan keeps the
    // read-only media share untouched and makes the cache self-healing.
    //
    // A failure falls back to the ORIGINAL file rather than erroring: the
    // 004-books behaviour (an unopenable but downloadable book) is strictly
    // better than a 500, and the cause is already on the counter and in the
    // log by the time this returns.
    let serve_path = match (&state.books, &item.book) {
        (Some(cache), Some(book))
            if pharos_scanner::book::kindle::is_converted_kindle(&item.path, book.format) =>
        {
            match cache.get_or_convert(id, &item.path).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        media.id = id,
                        path = %item.path.display(),
                        error = %e,
                        "download: kindle conversion unavailable; serving the original file"
                    );
                    item.path.clone()
                }
            }
        }
        _ => item.path.clone(),
    };

    let file = NamedFile::open_async(&serve_path).await.map_err(|e| {
        // Carry the offending path AND the underlying io error — "not found"
        // alone cannot distinguish a deleted file from a dead NFS mount, which
        // is exactly the distinction the mergerfs outage turned on.
        tracing::warn!(
            media.id = id,
            path = %serve_path.display(),
            error = %e,
            "download: source unreadable"
        );
        error::ErrorNotFound("source unreadable")
    })?;

    let mut resp = file
        .use_etag(true)
        .use_last_modified(true)
        .into_response(&req);

    // Both derived from what is actually being SENT, not from the item's
    // stored path: a converted book must arrive as `application/epub+zip`,
    // because a browser handed `application/vnd.amazon.ebook` on a same-origin
    // fetch may offer a download dialog instead of letting the reader have it.
    let ct = content_type_for(&serve_path);
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, header::HeaderValue::from_static(ct));
    if let Ok(v) = header::HeaderValue::from_str(&disposition_for(&item.path, &serve_path)) {
        resp.headers_mut().insert(header::CONTENT_DISPOSITION, v);
    }
    Ok::<HttpResponse, actix_web::Error>(resp)
}
