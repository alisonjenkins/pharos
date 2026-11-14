//! `/opds/` — an OPDS 1.2 catalogue of the signed-in user's books, for
//! e-readers (045, ADR-0128): KOReader on a Kobo, and any other OPDS client.
//!
//! Every route needs HTTP Basic auth with a Pharos username and one of that
//! user's e-reader passwords ([`reader_password::authenticate`]); the account
//! password is refused, and an e-reader password opens nothing outside this
//! scope. The catalogue is read-only: feeds, covers and file downloads.
//!
//! Which books are listed, and which a link may fetch, is decided by the
//! same policy code the library and the download route use
//! ([`access::visible_items`], [`access::check_item`]), so a library the user
//! may not open, or a rating above their parental cap, stays out (V1395).
//!
//! Each request counts one `pharos_opds_requests_total{route,outcome}`; each
//! file sent one `pharos_opds_downloads_total{format}`.

pub mod catalog;
pub mod feed;

use std::sync::Arc;

use actix_web::{http::header, http::StatusCode, web, HttpRequest, HttpResponse};
use pharos_core::{LibraryKind, LibraryStore, MediaId, MediaItem, MediaStore, User};

use crate::services::{access, reader_password};
use crate::state::AppState;
use catalog::Format;
use feed::{Head, Link, NavEntry, Paging, ACQUISITION_TYPE, NAVIGATION_TYPE, OPENSEARCH_TYPE};

/// Where the catalogue lives; what a reader is told to type.
pub const PREFIX: &str = "/opds";
/// Realm of the Basic challenge.
const REALM: &str = "Basic realm=\"Pharos\", charset=\"UTF-8\"";
/// Cover and thumbnail widths, in pixels. The cover is the image route's
/// Primary cap.
const COVER_WIDTH: u32 = 480;
const THUMBNAIL_WIDTH: u32 = 160;
/// Highest page number read; past it a request is refused.
const MAX_PAGE: usize = 100_000;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope(PREFIX)
            .route("", web::get().to(root))
            .route("/", web::get().to(root))
            .route("/books", web::get().to(all_books))
            .route("/recent", web::get().to(recent))
            .route("/authors", web::get().to(authors))
            .route("/authors/{key}", web::get().to(author_books))
            .route("/libraries/{id}", web::get().to(library_books))
            .route("/search", web::get().to(search))
            .route("/opensearch.xml", web::get().to(opensearch))
            .route("/books/{id}/file", web::get().to(download))
            .route("/books/{id}/file", web::head().to(download))
            .route("/books/{id}/cover", web::get().to(cover))
            .route("/books/{id}/thumbnail", web::get().to(thumbnail)),
    );
}

/// Which catalogue route answered. The `route` label of
/// `pharos_opds_requests_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Root,
    Books,
    Recent,
    Authors,
    Author,
    Library,
    Search,
    OpenSearch,
    Download,
    Cover,
    Thumbnail,
}

impl Route {
    pub const ALL: [Self; 11] = [
        Self::Root,
        Self::Books,
        Self::Recent,
        Self::Authors,
        Self::Author,
        Self::Library,
        Self::Search,
        Self::OpenSearch,
        Self::Download,
        Self::Cover,
        Self::Thumbnail,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::Books => "books",
            Self::Recent => "recent",
            Self::Authors => "authors",
            Self::Author => "author",
            Self::Library => "library",
            Self::Search => "search",
            Self::OpenSearch => "opensearch",
            Self::Download => "download",
            Self::Cover => "cover",
            Self::Thumbnail => "thumbnail",
        }
    }
}

/// How a catalogue request ended, from its status. The `outcome` label of
/// `pharos_opds_requests_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Unauthorized,
    Locked,
    Forbidden,
    NotFound,
    BadRequest,
    Failed,
}

impl Outcome {
    pub const ALL: [Self; 7] = [
        Self::Ok,
        Self::Unauthorized,
        Self::Locked,
        Self::Forbidden,
        Self::NotFound,
        Self::BadRequest,
        Self::Failed,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Unauthorized => "unauthorized",
            Self::Locked => "locked",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::BadRequest => "bad_request",
            Self::Failed => "failed",
        }
    }

    fn of(status: StatusCode) -> Self {
        match status.as_u16() {
            200..=399 => Self::Ok,
            401 => Self::Unauthorized,
            429 => Self::Locked,
            403 => Self::Forbidden,
            404 => Self::NotFound,
            400 => Self::BadRequest,
            _ => Self::Failed,
        }
    }
}

/// Run one catalogue route and count it by route and outcome.
async fn counted(
    route: Route,
    work: impl std::future::Future<Output = Result<HttpResponse, actix_web::Error>>,
) -> Result<HttpResponse, actix_web::Error> {
    let result = work.await;
    let status = match &result {
        Ok(resp) => resp.status(),
        Err(e) => e.as_response_error().status_code(),
    };
    let outcome = Outcome::of(status);
    metrics::counter!(
        "pharos_opds_requests_total",
        "route" => route.label(),
        "outcome" => outcome.label()
    )
    .increment(1);
    if outcome == Outcome::Failed {
        tracing::warn!(
            route = route.label(),
            status = status.as_u16(),
            "opds request failed"
        );
    }
    result
}

/// The user behind the request's Basic credential, or the refusal as a
/// response: 401 with the Basic challenge, 429 with `Retry-After`, 403
/// outside the access schedule, 503 when nothing could be checked.
async fn reader(state: &AppState, req: &HttpRequest) -> Result<User, actix_web::Error> {
    use reader_password::AuthOutcome;
    let authorization = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let client_ip = crate::client_ip::client_ip(req, &state.trusted_proxies);
    match reader_password::authenticate(state, authorization, client_ip).await {
        Ok((user, _)) => Ok(user),
        Err(refused) => {
            let resp = match refused.outcome {
                AuthOutcome::Locked => HttpResponse::TooManyRequests()
                    .insert_header((
                        header::RETRY_AFTER,
                        refused.retry_after_secs.unwrap_or(1).to_string(),
                    ))
                    .body("too many failed attempts"),
                AuthOutcome::OutsideSchedule => {
                    HttpResponse::Forbidden().body("not allowed at this time")
                }
                AuthOutcome::Unavailable => {
                    HttpResponse::ServiceUnavailable().body("try again shortly")
                }
                _ => HttpResponse::Unauthorized()
                    .insert_header((header::WWW_AUTHENTICATE, REALM))
                    .body("e-reader password required"),
            };
            Err(
                actix_web::error::InternalError::from_response(refused.outcome.label(), resp)
                    .into(),
            )
        }
    }
}

fn bad_request(what: &'static str) -> actix_web::Error {
    actix_web::error::ErrorBadRequest(what)
}

/// `?page=` (1-based, default 1) and `?q=`.
#[derive(Debug, Default, serde::Deserialize)]
struct Params {
    page: Option<usize>,
    q: Option<String>,
}

fn params(req: &HttpRequest) -> Result<Params, actix_web::Error> {
    let p = web::Query::<Params>::from_query(req.query_string())
        .map_err(|_| bad_request("bad query"))?
        .into_inner();
    if p.page.is_some_and(|n| n == 0 || n > MAX_PAGE) {
        return Err(bad_request("page out of range"));
    }
    Ok(p)
}

/// Every book `user` may open, unsorted: books only, openable ones only
/// (FR-002), then narrowed by the user's policy exactly as the library is.
async fn shelf(state: &AppState, user: &User) -> Result<Vec<MediaItem>, actix_web::Error> {
    let all = state.list_items_cached().await.map_err(|e| {
        tracing::warn!(error = %e, "opds: item list unavailable");
        actix_web::error::ErrorServiceUnavailable("library unavailable")
    })?;
    let converts = state.books.is_some();
    let books: Vec<MediaItem> = all
        .iter()
        .filter(|i| catalog::openable(i, converts).is_some())
        .cloned()
        .collect();
    let visible = access::visible_items(state, user, Arc::new(books), "opds")
        .await
        .map_err(crate::api::jellyfin::access_error)?;
    Ok(Arc::try_unwrap(visible).unwrap_or_else(|shared| (*shared).clone()))
}

/// The Books libraries `user` may open, by name.
fn book_libraries(state: &AppState, user: &User) -> Vec<pharos_core::Library> {
    let mut libs: Vec<pharos_core::Library> = state
        .libraries()
        .iter()
        .filter(|l| l.kind == LibraryKind::Books && user.policy.allows_library(&l.wire_id))
        .cloned()
        .collect();
    libs.sort_by(|a, b| a.name.cmp(&b.name));
    libs
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn xml(kind: &'static str, body: String) -> HttpResponse {
    HttpResponse::Ok()
        .content_type(format!("{kind}; charset=utf-8"))
        .insert_header((header::CACHE_CONTROL, "private, no-cache"))
        .body(body)
}

/// Links every feed's head carries: start and both search forms.
fn common_links(self_href: String, self_kind: &'static str) -> Vec<Link> {
    vec![
        Link::new("self", self_href, self_kind),
        Link::new("start", format!("{PREFIX}/"), NAVIGATION_TYPE),
        Link::new(
            "search",
            format!("{PREFIX}/opensearch.xml"),
            OPENSEARCH_TYPE,
        ),
        Link::new(
            "search",
            format!("{PREFIX}/search?q={{searchTerms}}"),
            ACQUISITION_TYPE,
        ),
    ]
}

fn book_href(id: MediaId) -> String {
    format!(
        "{PREFIX}/books/{}",
        pharos_jellyfin_api::dto::wire_item_id(id)
    )
}

fn entry_of(item: &MediaItem, converts: bool) -> Option<feed::BookEntry> {
    let (format, mime) = catalog::openable(item, converts)?;
    let book = item.book.as_ref()?;
    let base = book_href(item.id);
    Some(feed::BookEntry {
        id: format!(
            "urn:pharos:book:{}",
            pharos_jellyfin_api::dto::wire_item_id(item.id)
        ),
        title: item.title.clone(),
        author: catalog::author_of(item).map(|name| {
            (
                name.to_owned(),
                format!("{PREFIX}/authors/{}", catalog::author_key(name)),
            )
        }),
        updated: item.metadata_refreshed_at.or(item.created_at).unwrap_or(0),
        summary: item
            .metadata
            .overview
            .clone()
            .filter(|s| !s.trim().is_empty()),
        language: item.metadata.original_language.clone(),
        publisher: book.publisher.clone(),
        isbn: book.isbn.clone(),
        images: item
            .has_primary_art
            .then(|| (format!("{base}/cover"), format!("{base}/thumbnail"))),
        acquisition: (format!("{base}/file"), mime, format.title()),
    })
}

/// What names an acquisition feed: its id, title, path, parent and any
/// query its paging links keep (ending in `&`).
struct FeedOf<'a> {
    id: &'a str,
    title: &'a str,
    path: &'a str,
    up: &'a str,
    query: &'a str,
}

/// One page of `books` (already in order) as an acquisition feed.
fn acquisition_page(
    state: &AppState,
    of: &FeedOf<'_>,
    books: &[MediaItem],
    page: usize,
) -> HttpResponse {
    let (id, title, self_path, up, extra_query) = (of.id, of.title, of.path, of.up, of.query);
    let (range, last) = catalog::page_range(books.len(), page);
    let href = |n: usize| format!("{self_path}?{extra_query}page={n}");
    let mut links = common_links(href(page), ACQUISITION_TYPE);
    links.push(Link::new("up", up.to_owned(), NAVIGATION_TYPE));
    links.push(Link::new("first", href(1), ACQUISITION_TYPE));
    links.push(Link::new("last", href(last), ACQUISITION_TYPE));
    if page > 1 {
        links.push(Link::new(
            "previous",
            href(page.saturating_sub(1).min(last)),
            ACQUISITION_TYPE,
        ));
    }
    if page < last {
        links.push(Link::new(
            "next",
            href(page.saturating_add(1)),
            ACQUISITION_TYPE,
        ));
    }
    let converts = state.books.is_some();
    let entries: Vec<feed::BookEntry> = books
        .get(range.clone())
        .unwrap_or_default()
        .iter()
        .filter_map(|b| entry_of(b, converts))
        .collect();
    let head = Head {
        id: id.to_owned(),
        title: title.to_owned(),
        updated: now(),
        links,
    };
    let paging = Paging {
        total: books.len(),
        per_page: catalog::PAGE_SIZE,
        start: range.start,
    };
    xml(
        ACQUISITION_TYPE,
        feed::acquisition(&head, Some(paging), &entries),
    )
}

async fn root(
    state: web::Data<AppState>,
    req: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    counted(Route::Root, async {
        let user = reader(&state, &req).await?;
        let mut entries = vec![
            NavEntry {
                id: "urn:pharos:opds:books".into(),
                title: "All books".into(),
                summary: Some("Every book, by title".into()),
                href: format!("{PREFIX}/books"),
                kind: ACQUISITION_TYPE,
            },
            NavEntry {
                id: "urn:pharos:opds:recent".into(),
                title: "Recently added".into(),
                summary: Some("The newest books first".into()),
                href: format!("{PREFIX}/recent"),
                kind: ACQUISITION_TYPE,
            },
            NavEntry {
                id: "urn:pharos:opds:authors".into(),
                title: "By author".into(),
                summary: None,
                href: format!("{PREFIX}/authors"),
                kind: NAVIGATION_TYPE,
            },
        ];
        let libraries = book_libraries(&state, &user);
        if libraries.len() > 1 {
            for lib in libraries {
                entries.push(NavEntry {
                    id: format!("urn:pharos:opds:library:{}", lib.wire_id),
                    title: lib.name.clone(),
                    summary: Some("A library, by title".into()),
                    href: format!("{PREFIX}/libraries/{}", lib.wire_id),
                    kind: ACQUISITION_TYPE,
                });
            }
        }
        let head = Head {
            id: "urn:pharos:opds:root".into(),
            title: "Pharos".into(),
            updated: now(),
            links: common_links(format!("{PREFIX}/"), NAVIGATION_TYPE),
        };
        Ok(xml(
            NAVIGATION_TYPE,
            feed::navigation(&head, None, &entries),
        ))
    })
    .await
}

async fn all_books(
    state: web::Data<AppState>,
    req: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    counted(Route::Books, async {
        let user = reader(&state, &req).await?;
        let p = params(&req)?;
        let mut books = shelf(&state, &user).await?;
        catalog::sort_by_title(&mut books);
        Ok(acquisition_page(
            &state,
            &FeedOf {
                id: "urn:pharos:opds:books",
                title: "All books",
                path: &format!("{PREFIX}/books"),
                up: &format!("{PREFIX}/"),
                query: "",
            },
            &books,
            p.page.unwrap_or(1),
        ))
    })
    .await
}

async fn recent(
    state: web::Data<AppState>,
    req: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    counted(Route::Recent, async {
        let user = reader(&state, &req).await?;
        let p = params(&req)?;
        let mut books = shelf(&state, &user).await?;
        catalog::sort_recent(&mut books);
        Ok(acquisition_page(
            &state,
            &FeedOf {
                id: "urn:pharos:opds:recent",
                title: "Recently added",
                path: &format!("{PREFIX}/recent"),
                up: &format!("{PREFIX}/"),
                query: "",
            },
            &books,
            p.page.unwrap_or(1),
        ))
    })
    .await
}

async fn authors(
    state: web::Data<AppState>,
    req: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    counted(Route::Authors, async {
        let user = reader(&state, &req).await?;
        let p = params(&req)?;
        let page = p.page.unwrap_or(1);
        let books = shelf(&state, &user).await?;
        let all = catalog::authors(&books);
        let (range, last) = catalog::page_range(all.len(), page);
        let href = |n: usize| format!("{PREFIX}/authors?page={n}");
        let mut links = common_links(href(page), NAVIGATION_TYPE);
        links.push(Link::new("up", format!("{PREFIX}/"), NAVIGATION_TYPE));
        if page > 1 {
            links.push(Link::new(
                "previous",
                href(page.saturating_sub(1).min(last)),
                NAVIGATION_TYPE,
            ));
        }
        if page < last {
            links.push(Link::new(
                "next",
                href(page.saturating_add(1)),
                NAVIGATION_TYPE,
            ));
        }
        let entries: Vec<NavEntry> = all
            .get(range.clone())
            .unwrap_or_default()
            .iter()
            .map(|(key, name, count)| NavEntry {
                id: format!("urn:pharos:opds:author:{key}"),
                title: name.clone(),
                summary: Some(if *count == 1 {
                    "1 book".to_owned()
                } else {
                    format!("{count} books")
                }),
                href: format!("{PREFIX}/authors/{key}"),
                kind: ACQUISITION_TYPE,
            })
            .collect();
        let head = Head {
            id: "urn:pharos:opds:authors".into(),
            title: "By author".into(),
            updated: now(),
            links,
        };
        let paging = Paging {
            total: all.len(),
            per_page: catalog::PAGE_SIZE,
            start: range.start,
        };
        Ok(xml(
            NAVIGATION_TYPE,
            feed::navigation(&head, Some(paging), &entries),
        ))
    })
    .await
}

/// A path key is 16 lowercase hex digits; anything else names no author.
fn valid_author_key(key: &str) -> bool {
    key.len() == 16 && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

async fn author_books(
    state: web::Data<AppState>,
    req: HttpRequest,
    key: web::Path<String>,
) -> Result<HttpResponse, actix_web::Error> {
    counted(Route::Author, async {
        let user = reader(&state, &req).await?;
        let p = params(&req)?;
        let key = key.into_inner();
        if !valid_author_key(&key) {
            return Err(actix_web::error::ErrorNotFound("no such author"));
        }
        let mut books: Vec<MediaItem> = shelf(&state, &user)
            .await?
            .into_iter()
            .filter(|b| catalog::author_of(b).is_some_and(|a| catalog::author_key(a) == key))
            .collect();
        let Some(name) = books
            .first()
            .and_then(catalog::author_of)
            .map(str::to_owned)
        else {
            return Err(actix_web::error::ErrorNotFound("no such author"));
        };
        catalog::sort_by_title(&mut books);
        Ok(acquisition_page(
            &state,
            &FeedOf {
                id: &format!("urn:pharos:opds:author:{key}"),
                title: &name,
                path: &format!("{PREFIX}/authors/{key}"),
                up: &format!("{PREFIX}/authors"),
                query: "",
            },
            &books,
            p.page.unwrap_or(1),
        ))
    })
    .await
}

async fn library_books(
    state: web::Data<AppState>,
    req: HttpRequest,
    id: web::Path<String>,
) -> Result<HttpResponse, actix_web::Error> {
    counted(Route::Library, async {
        let user = reader(&state, &req).await?;
        let p = params(&req)?;
        let id = id.into_inner();
        // Only a Books library this user may open is a feed; any other id
        // answers like a missing one.
        let Some(lib) = book_libraries(&state, &user)
            .into_iter()
            .find(|l| l.wire_id == id)
        else {
            return Err(actix_web::error::ErrorNotFound("no such library"));
        };
        let ids: std::collections::HashSet<MediaId> = state
            .stores
            .item_ids_for_library(&lib.wire_id)
            .await
            .map_err(|e| {
                tracing::warn!(library = %lib.wire_id, error = %e, "opds: library items unavailable");
                actix_web::error::ErrorServiceUnavailable("library unavailable")
            })?
            .into_iter()
            .collect();
        let mut books: Vec<MediaItem> = shelf(&state, &user)
            .await?
            .into_iter()
            .filter(|b| ids.contains(&b.id))
            .collect();
        catalog::sort_by_title(&mut books);
        Ok(acquisition_page(
            &state,
            &FeedOf {
                id: &format!("urn:pharos:opds:library:{}", lib.wire_id),
                title: &lib.name,
                path: &format!("{PREFIX}/libraries/{}", lib.wire_id),
                up: &format!("{PREFIX}/"),
                query: "",
            },
            &books,
            p.page.unwrap_or(1),
        ))
    })
    .await
}

async fn search(
    state: web::Data<AppState>,
    req: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    counted(Route::Search, async {
        let user = reader(&state, &req).await?;
        let p = params(&req)?;
        let q = p.q.unwrap_or_default();
        let words = catalog::query_words(&q);
        let mut books: Vec<MediaItem> = shelf(&state, &user)
            .await?
            .into_iter()
            .filter(|b| catalog::matches(b, &words))
            .collect();
        catalog::sort_by_title(&mut books);
        tracing::debug!(user = %user.name, words = words.len(), found = books.len(), "opds search");
        // The term goes back into the paging links percent-encoded, as the
        // words it was read as.
        let query = format!("q={}&", percent_encode(&words.join(" ")));
        Ok(acquisition_page(
            &state,
            &FeedOf {
                id: "urn:pharos:opds:search",
                title: "Search results",
                path: &format!("{PREFIX}/search"),
                up: &format!("{PREFIX}/"),
                query: &query,
            },
            &books,
            p.page.unwrap_or(1),
        ))
    })
    .await
}

/// `s` with every byte but unreserved ones as `%XX`.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

async fn opensearch(
    state: web::Data<AppState>,
    req: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    counted(Route::OpenSearch, async {
        reader(&state, &req).await?;
        Ok(xml(
            OPENSEARCH_TYPE,
            feed::opensearch(&format!("{PREFIX}/search?q={{searchTerms}}")),
        ))
    })
    .await
}

/// The book `id_str` names, if `user` may reach it and the catalogue would
/// list it; 404 otherwise, a hidden book answering like a missing one.
async fn listed_book(
    state: &AppState,
    user: &User,
    id_str: &str,
    route: &'static str,
) -> Result<(MediaItem, Format), actix_web::Error> {
    let not_found = || actix_web::error::ErrorNotFound("no such book");
    let id = pharos_jellyfin_api::dto::parse_item_id(id_str).ok_or_else(not_found)?;
    crate::api::jellyfin::require_item_access(state, user, id, route).await?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => not_found(),
        other => {
            tracing::warn!(media.id = id, error = %other, "opds: book unavailable");
            actix_web::error::ErrorServiceUnavailable("library unavailable")
        }
    })?;
    let (format, _) = catalog::openable(&item, state.books.is_some()).ok_or_else(|| {
        tracing::info!(media.id = id, route, "opds: not a listed book; refused");
        not_found()
    })?;
    Ok((item, format))
}

async fn download(
    state: web::Data<AppState>,
    req: HttpRequest,
    id: web::Path<String>,
) -> Result<HttpResponse, actix_web::Error> {
    counted(Route::Download, async {
        let user = reader(&state, &req).await?;
        let (item, format) = listed_book(&state, &user, &id, "opds_download").await?;
        let resp = crate::api::jellyfin::download::serve_item_file(&state, &req, &item).await?;
        if resp.status().is_success() && req.method() == actix_web::http::Method::GET {
            metrics::counter!("pharos_opds_downloads_total", "format" => format.label())
                .increment(1);
            tracing::info!(
                user = %user.name,
                media.id = item.id,
                format = format.label(),
                "opds: book downloaded"
            );
        }
        Ok(resp)
    })
    .await
}

async fn image(
    state: &AppState,
    req: &HttpRequest,
    id: &str,
    width: u32,
) -> Result<HttpResponse, actix_web::Error> {
    let user = reader(state, req).await?;
    let (item, _) = listed_book(state, &user, id, "opds_image").await?;
    crate::api::jellyfin::images::serve_opds_cover(state, &user, req, item.id, width).await
}

async fn cover(
    state: web::Data<AppState>,
    req: HttpRequest,
    id: web::Path<String>,
) -> Result<HttpResponse, actix_web::Error> {
    counted(Route::Cover, image(&state, &req, &id, COVER_WIDTH)).await
}

async fn thumbnail(
    state: web::Data<AppState>,
    req: HttpRequest,
    id: web::Path<String>,
) -> Result<HttpResponse, actix_web::Error> {
    counted(Route::Thumbnail, image(&state, &req, &id, THUMBNAIL_WIDTH)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_and_outcome_labels_are_distinct() {
        let routes: std::collections::HashSet<_> = Route::ALL.iter().map(|r| r.label()).collect();
        assert_eq!(routes.len(), Route::ALL.len());
        let outcomes: std::collections::HashSet<_> =
            Outcome::ALL.iter().map(|o| o.label()).collect();
        assert_eq!(outcomes.len(), Outcome::ALL.len());
    }

    #[test]
    fn statuses_map_to_outcomes() {
        assert_eq!(Outcome::of(StatusCode::OK), Outcome::Ok);
        assert_eq!(Outcome::of(StatusCode::PARTIAL_CONTENT), Outcome::Ok);
        assert_eq!(Outcome::of(StatusCode::NOT_MODIFIED), Outcome::Ok);
        assert_eq!(Outcome::of(StatusCode::UNAUTHORIZED), Outcome::Unauthorized);
        assert_eq!(Outcome::of(StatusCode::TOO_MANY_REQUESTS), Outcome::Locked);
        assert_eq!(
            Outcome::of(StatusCode::SERVICE_UNAVAILABLE),
            Outcome::Failed
        );
    }

    #[test]
    fn author_keys_are_checked_before_use() {
        assert!(valid_author_key(&catalog::author_key("Ada Sample")));
        assert!(!valid_author_key("../../etc/passwd"));
        assert!(!valid_author_key("ABCDEF0123456789"));
    }

    #[test]
    fn search_terms_are_percent_encoded() {
        assert_eq!(percent_encode("a b&c"), "a%20b%26c");
        assert_eq!(percent_encode("é"), "%C3%A9");
    }
}
