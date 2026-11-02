//! pharos-ui served at the root (spec 009 contracts/serving.md, V204).
//!
//! The bundle's files are indexed once at startup and only indexed files are
//! ever opened, so no request path reaches the filesystem. Keys are
//! lowercase because [`LowercasePath`](crate::middleware::LowercasePath)
//! folds every path before routing.
//!
//! [`configure`] goes before every other route: its `/` replaces the plain
//! `"pharos"` answer, and its default service only sees paths no API route
//! claimed. A browser navigating to a UI route gets `index.html`; an API
//! client, a non-GET, or a path that names a file gets 404.

use actix_files::NamedFile;
use actix_web::http::header::{self, CacheControl, CacheDirective, HeaderValue};
use actix_web::{web, HttpRequest, HttpResponse};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The first path segment of every route the UI owns (spec 009 data-model
/// §Route). No API route may claim one; `it_wire::ui_serving` checks each.
pub const UI_ROUTES: &[&str] = &[
    "browse", "title", "watch", "find", "together", "settings", "manage", "signin",
];

/// A year: the longest `max-age` caches honour. Assets are content-hashed,
/// so a changed file has a new name.
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
}

/// A built pharos-ui bundle (`dx build`'s `public/`).
#[derive(Debug)]
pub struct UiBundle {
    index: PathBuf,
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
        Ok(Self { index, files })
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

/// Mount the UI. Register it before every other route.
pub fn configure(bundle: Arc<UiBundle>) -> impl FnOnce(&mut web::ServiceConfig) {
    move |sc| {
        sc.app_data(web::Data::from(bundle))
            .route("/", web::get().to(root))
            .route("/ui", web::get().to(redirect))
            .route("/ui/{tail:.*}", web::get().to(redirect))
            .default_service(web::to(fallback));
    }
}

async fn root(req: HttpRequest, bundle: web::Data<UiBundle>) -> HttpResponse {
    index(&req, &bundle).await
}

async fn index(req: &HttpRequest, bundle: &UiBundle) -> HttpResponse {
    respond(req, &bundle.index, Outcome::Index).await
}

async fn respond(req: &HttpRequest, path: &Path, outcome: Outcome) -> HttpResponse {
    let file = match NamedFile::open(path) {
        Ok(file) => file,
        Err(e) => {
            tracing::error!(path = %path.display(), error = %e, "ui: bundle file unreadable");
            return HttpResponse::InternalServerError().finish();
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
    if let Ok(value) = HeaderValue::from_str(&cache.to_string()) {
        resp.headers_mut().insert(header::CACHE_CONTROL, value);
    }
    resp
}

/// `/ui/<rest>` → `308 /<rest>`, keeping the query.
async fn redirect(req: HttpRequest) -> HttpResponse {
    let rest = req
        .path()
        .strip_prefix("/ui")
        .unwrap_or("")
        .trim_start_matches('/');
    let location = match req.query_string() {
        "" => format!("/{rest}"),
        q => format!("/{rest}?{q}"),
    };
    record(Outcome::Redirect);
    HttpResponse::PermanentRedirect()
        .insert_header((header::LOCATION, location))
        .finish()
}

fn wants_html(req: &HttpRequest) -> bool {
    req.headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/html"))
}

async fn fallback(req: HttpRequest, bundle: web::Data<UiBundle>) -> HttpResponse {
    let is_read = matches!(
        *req.method(),
        actix_web::http::Method::GET | actix_web::http::Method::HEAD
    );
    let key = req.path().trim_start_matches('/');
    if is_read {
        if let Some(path) = bundle.files.get(key) {
            let outcome = if key.starts_with("assets/") {
                Outcome::Asset
            } else {
                Outcome::File
            };
            return respond(&req, path, outcome).await;
        }
        let last = key.rsplit('/').next().unwrap_or("");
        if wants_html(&req) && !last.contains('.') {
            return index(&req, &bundle).await;
        }
    }
    tracing::debug!(method = %req.method(), path = req.path(), "ui: no route or file");
    record(Outcome::NotFound);
    HttpResponse::NotFound().finish()
}
