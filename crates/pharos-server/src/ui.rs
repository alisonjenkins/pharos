//! pharos-ui served under `/ui/` (spec 009 contracts/serving.md, V204, V210).
//!
//! The bundle's files are indexed once at startup and only indexed files are
//! ever opened, so no request path reaches the filesystem. Keys are
//! lowercase because [`LowercasePath`](crate::middleware::LowercasePath)
//! folds every path before routing.
//!
//! [`configure`] mounts a `/ui` scope and nothing else; every other path
//! falls through to the server's own routes and its 404. A browser
//! navigating to a UI route gets `index.html`; an API client, a non-GET, or
//! a path that names a file gets 404.
//!
//! [`router::configure_app`](crate::router::configure_app) does the ordering;
//! `main` and the tests share it.

use actix_files::NamedFile;
use actix_web::http::header::{self, CacheControl, CacheDirective, HeaderValue};
use actix_web::{web, HttpRequest, HttpResponse};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The path prefix the bundle is mounted under.
const MOUNT: &str = "/ui";

/// A year: the longest `max-age` caches honour. Only content-hashed assets
/// get it (see [`is_hashed_asset`]).
const IMMUTABLE_MAX_AGE: u32 = 31_536_000;

#[derive(Debug, thiserror::Error)]
pub enum UiBundleError {
    #[error("ui bundle {dir}: no index.html")]
    NoIndex { dir: PathBuf },
    #[error("ui bundle {dir}: {source}")]
    Read {
        dir: PathBuf,
        source: std::io::Error,
    },
    #[error("ui bundle: {a} and {b} differ only in case, and paths are matched lowercased")]
    CaseCollision { a: PathBuf, b: PathBuf },
    #[error("ui bundle: index.html yields an invalid Content-Security-Policy header")]
    Csp,
}

/// A built pharos-ui bundle (`dx build`'s `public/`).
#[derive(Debug)]
pub struct UiBundle {
    index: PathBuf,
    /// The Content-Security-Policy every UI response carries.
    csp: HeaderValue,
    /// Lowercased path relative to the bundle root, `/`-separated → file.
    files: HashMap<String, PathBuf>,
}

impl UiBundle {
    /// Index every regular file under `dir`. Symlinks are skipped, so the
    /// index never points outside the bundle.
    pub fn load(dir: &Path) -> Result<Self, UiBundleError> {
        let mut files = HashMap::new();
        walk(dir, dir, &mut files)?;
        let index = files
            .get("index.html")
            .cloned()
            .ok_or_else(|| UiBundleError::NoIndex {
                dir: dir.to_path_buf(),
            })?;
        let html = std::fs::read_to_string(&index).map_err(|source| UiBundleError::Read {
            dir: dir.to_path_buf(),
            source,
        })?;
        let csp = HeaderValue::from_str(&content_security_policy(&html))
            .map_err(|_| UiBundleError::Csp)?;
        Ok(Self { index, csp, files })
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }
}

fn walk(
    root: &Path,
    dir: &Path,
    files: &mut HashMap<String, PathBuf>,
) -> Result<(), UiBundleError> {
    let read_err = |source| UiBundleError::Read {
        dir: dir.to_path_buf(),
        source,
    };
    for entry in std::fs::read_dir(dir).map_err(read_err)? {
        let entry = entry.map_err(read_err)?;
        let path = entry.path();
        let kind = entry.file_type().map_err(read_err)?;
        if kind.is_dir() {
            walk(root, &path, files)?;
        } else if kind.is_file() {
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let key = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
                .collect::<Vec<_>>()
                .join("/");
            if let Some(prev) = files.insert(key, path.clone()) {
                return Err(UiBundleError::CaseCollision { a: prev, b: path });
            }
        }
    }
    Ok(())
}

/// The policy for the bundle's pages. Script runs only from this origin,
/// plus wasm compilation and `index.html`'s own inline scripts by hash; the
/// page connects only to this origin (the native API's WebSocket included),
/// plays media from it or from `blob:` (MSE), and is never framed.
fn content_security_policy(index_html: &str) -> String {
    use base64::Engine as _;
    use sha2::Digest as _;
    let hashes: String = inline_scripts(index_html)
        .map(|body| {
            let digest = sha2::Sha256::digest(body.as_bytes());
            format!(
                " 'sha256-{}'",
                base64::engine::general_purpose::STANDARD.encode(digest)
            )
        })
        .collect();
    format!(
        "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'{hashes}; \
         style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; \
         media-src 'self' blob:; worker-src 'self' blob:; connect-src 'self'; \
         font-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; \
         frame-ancestors 'none'"
    )
}

/// The bodies of `<script>` elements with no `src`, as the browser hashes them.
fn inline_scripts(html: &str) -> impl Iterator<Item = &str> {
    html.match_indices("<script").filter_map(move |(at, _)| {
        let rest = &html[at..];
        let open_end = rest.find('>')?;
        let tag = &rest[..open_end];
        if tag.contains(" src=") || tag.contains(" src ") {
            return None;
        }
        let body = &rest[open_end + 1..];
        Some(&body[..body.find("</script>")?])
    })
}

#[derive(Clone, Copy)]
enum Outcome {
    Index,
    Asset,
    File,
    Redirect,
    NotFound,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Outcome::Index => "index",
            Outcome::Asset => "asset",
            Outcome::File => "file",
            Outcome::Redirect => "redirect",
            Outcome::NotFound => "not_found",
        }
    }
}

fn record(outcome: Outcome) {
    metrics::counter!("pharos_ui_requests_total", "outcome" => outcome.label()).increment(1);
}

/// Mount the UI under `/ui`.
pub fn configure(bundle: Arc<UiBundle>) -> impl FnOnce(&mut web::ServiceConfig) {
    move |sc| {
        sc.app_data(web::Data::from(bundle))
            .service(web::scope(MOUNT).default_service(web::to(serve)));
    }
}

async fn index(req: &HttpRequest, bundle: &UiBundle) -> HttpResponse {
    respond(req, bundle, &bundle.index, Outcome::Index).await
}

async fn respond(
    req: &HttpRequest,
    bundle: &UiBundle,
    path: &Path,
    outcome: Outcome,
) -> HttpResponse {
    let owned = path.to_path_buf();
    let file = match web::block(move || NamedFile::open(owned)).await {
        Ok(Ok(file)) => file,
        Ok(Err(e)) => {
            tracing::error!(path = %path.display(), error = %e, "ui: bundle file unreadable");
            return secured(HttpResponse::InternalServerError().finish(), &bundle.csp);
        }
        Err(e) => {
            tracing::error!(path = %path.display(), error = %e, "ui: bundle file unreadable");
            return secured(HttpResponse::InternalServerError().finish(), &bundle.csp);
        }
    };
    let cache = match outcome {
        Outcome::Asset => CacheControl(vec![
            CacheDirective::Public,
            CacheDirective::MaxAge(IMMUTABLE_MAX_AGE),
            CacheDirective::Extension("immutable".into(), None),
        ]),
        _ => CacheControl(vec![CacheDirective::NoCache]),
    };
    record(outcome);
    let mut resp = file.into_response(req);
    let headers = resp.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&cache.to_string()) {
        headers.insert(header::CACHE_CONTROL, value);
    }
    secured(resp, &bundle.csp)
}

/// Stamps the security headers on a UI response of any status, so no reply
/// from the UI, a redirect or an error included, goes out without them.
fn secured(mut resp: HttpResponse, csp: &HeaderValue) -> HttpResponse {
    let headers = resp.headers_mut();
    headers.insert(header::CONTENT_SECURITY_POLICY, csp.clone());
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    resp
}

/// `/ui` → `308 /ui/`, keeping the query.
fn redirect(req: &HttpRequest, bundle: &UiBundle) -> HttpResponse {
    let location = match req.query_string() {
        "" => format!("{MOUNT}/"),
        q => format!("{MOUNT}/?{q}"),
    };
    record(Outcome::Redirect);
    secured(
        HttpResponse::PermanentRedirect()
            .insert_header((header::LOCATION, location))
            .finish(),
        &bundle.csp,
    )
}

/// A dx-built asset, whose name carries its content hash (`-dxh<hex>`), so
/// a changed file has a new URL. Unhashed files (`pharos.css`) revalidate.
fn is_hashed_asset(key: &str) -> bool {
    key.strip_prefix("assets/")
        .is_some_and(|name| name.contains("-dxh"))
}

fn wants_html(req: &HttpRequest) -> bool {
    req.headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/html"))
}

async fn serve(req: HttpRequest, bundle: web::Data<UiBundle>) -> HttpResponse {
    let is_read = matches!(
        *req.method(),
        actix_web::http::Method::GET | actix_web::http::Method::HEAD
    );
    let rest = req.path().strip_prefix(MOUNT).unwrap_or("");
    if is_read {
        if rest.is_empty() {
            return redirect(&req, &bundle);
        }
        let key = rest.trim_start_matches('/');
        if key.is_empty() {
            return index(&req, &bundle).await;
        }
        if let Some(path) = bundle.files.get(key) {
            let outcome = if is_hashed_asset(key) {
                Outcome::Asset
            } else {
                Outcome::File
            };
            return respond(&req, &bundle, path, outcome).await;
        }
        let last = key.rsplit('/').next().unwrap_or("");
        if wants_html(&req) && !last.contains('.') {
            return index(&req, &bundle).await;
        }
    }
    tracing::debug!(method = %req.method(), path = req.path(), "ui: no route or file");
    record(Outcome::NotFound);
    secured(HttpResponse::NotFound().finish(), &bundle.csp)
}
