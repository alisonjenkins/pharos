//! 045 — the OPDS catalogue under `/opds/` as an e-reader sees it: Basic
//! auth with an e-reader password, feeds parsed back as XML, streamed
//! downloads, and the same library access the rest of the server applies.
//! Every title, author and path here is invented.

use actix_web::{http::StatusCode, test, web, App};
use base64::Engine as _;
use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use pharos_core::{
    BookFormat, BookMeta, LibraryKind, LibraryStore, MediaItem, MediaKind, MediaStore,
    SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::{jellyfin, jellyfin::items::library_id_for_root, opds},
    auth::BuiltinAuth,
    middleware::LowercasePath,
    services::reader_password,
    state::{AppState, Stores},
};
use quick_xml::events::Event;
use tempfile::TempDir;

const MAIN_PASSWORD: &str = "main-account-pass";

struct Rig {
    _dir: TempDir,
    state: web::Data<AppState>,
    /// The full-access user's e-reader password.
    kobo: String,
    /// The same user's Jellyfin token, for the "other routes" checks.
    token: String,
    lib_a: String,
    lib_b: String,
    alpha_bytes: Vec<u8>,
}

fn basic(name: &str, password: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{name}:{password}"))
    )
}

async fn user(stores: &Stores, name: &str, policy: UserPolicy) -> UserId {
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth
        .hash_password(&SecretString::new(MAIN_PASSWORD))
        .unwrap();
    let id = UserId::new();
    stores
        .create(UserRecord {
            id,
            name: name.into(),
            password_hash: hash,
            policy,
        })
        .await
        .unwrap();
    id
}

fn book(id: u64, path: std::path::PathBuf, title: &str, format: BookFormat) -> MediaItem {
    MediaItem {
        id,
        path,
        title: title.into(),
        kind: MediaKind::Book,
        book: Some(BookMeta {
            format,
            ..Default::default()
        }),
        created_at: Some(1_700_000_000 + i64::try_from(id).unwrap()),
        ..Default::default()
    }
}

/// Two Books libraries. A: an epub with a cover (Ada Sample), a pdf, a cbz,
/// a DRM Kindle file and a film. B: an epub (Wren Example) and `fillers`
/// plain epubs.
async fn rig(fillers: usize) -> Rig {
    let dir = TempDir::new().unwrap();
    let (root_a, root_b) = (dir.path().join("a"), dir.path().join("b"));
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();
    let alpha_bytes: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();

    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let wire = |p: &std::path::Path| library_id_for_root(p);
    let (lib_a, lib_b) = (wire(&root_a), wire(&root_b));
    stores
        .upsert_library(
            "Shelf A",
            root_a.to_str().unwrap(),
            LibraryKind::Books,
            &lib_a,
        )
        .await
        .unwrap();
    stores
        .upsert_library(
            "Shelf B",
            root_b.to_str().unwrap(),
            LibraryKind::Books,
            &lib_b,
        )
        .await
        .unwrap();

    let mut items = Vec::new();
    let mut alpha = book(
        1,
        root_a.join("alpha.epub"),
        "Alpha Lantern",
        BookFormat::Epub,
    );
    alpha.metadata.overview = Some("A keeper & her <lamp>.".into());
    alpha.metadata.original_language = Some("en".into());
    if let Some(b) = alpha.book.as_mut() {
        b.author = Some("Ada Sample".into());
    }
    items.push(alpha);
    items.push(book(
        2,
        root_a.join("beta.pdf"),
        "Beta Garden",
        BookFormat::Pdf,
    ));
    items.push(book(
        3,
        root_a.join("gamma.cbz"),
        "Gamma Comic",
        BookFormat::Comic,
    ));
    items.push(book(
        4,
        root_a.join("locked.azw3"),
        "Locked Kindle",
        BookFormat::Unreadable,
    ));
    let mut film = book(5, root_a.join("film.mkv"), "A Film", BookFormat::Epub);
    film.kind = MediaKind::Movie;
    film.book = None;
    items.push(film);
    let mut delta = book(
        6,
        root_b.join("delta.epub"),
        "Delta River",
        BookFormat::Epub,
    );
    if let Some(b) = delta.book.as_mut() {
        b.author = Some("Wren Example".into());
    }
    items.push(delta);
    for n in 0..fillers {
        let id = 100 + u64::try_from(n).unwrap();
        items.push(book(
            id,
            root_b.join(format!("filler-{n:03}.epub")),
            &format!("Filler {n:03}"),
            BookFormat::Epub,
        ));
    }
    for item in items {
        std::fs::write(
            &item.path,
            if item.id == 1 { &alpha_bytes[..] } else { b"x" },
        )
        .unwrap();
        stores.put(item).await.unwrap();
    }
    stores.backfill_library_ids().await.unwrap();
    // A stored Primary is what marks a book as having a cover.
    let cover = root_a.join("alpha-cover.jpg");
    std::fs::write(&cover, b"jpeg").unwrap();
    stores
        .set_artwork(1, "Primary", "local", cover.to_str().unwrap())
        .await
        .unwrap();

    let reader = user(&stores, "reader", UserPolicy::default()).await;
    let no_libraries = UserPolicy {
        enable_all_folders: false,
        enabled_folders: Vec::new(),
        ..UserPolicy::default()
    };
    user(&stores, "kid", no_libraries).await;
    let only_b = UserPolicy {
        enable_all_folders: false,
        enabled_folders: vec![lib_b.clone()],
        ..UserPolicy::default()
    };
    user(&stores, "partial", only_b).await;
    let token = stores
        .issue(reader, "t")
        .await
        .unwrap()
        .0
        .expose()
        .to_string();

    let libraries = stores.libraries().await.unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()).with_libraries(libraries));
    let kobo = new_password(&state, "reader", "Kobo").await;
    Rig {
        _dir: dir,
        state,
        kobo,
        token,
        lib_a,
        lib_b,
        alpha_bytes,
    }
}

async fn new_password(state: &AppState, name: &str, label: &str) -> String {
    let record = state.auth.account_named(name).await.unwrap().unwrap();
    let created = reader_password::create(state, &record.into_user(), label)
        .await
        .unwrap();
    created.password.expose().to_owned()
}

macro_rules! app {
    ($state:expr) => {
        test::init_service(
            App::new()
                .app_data($state.clone())
                .wrap(LowercasePath)
                .configure(jellyfin::configure)
                .configure(opds::configure),
        )
        .await
    };
}

struct Reply {
    status: StatusCode,
    headers: actix_web::http::header::HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> String {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    fn text(&self) -> String {
        String::from_utf8(self.body.clone()).unwrap()
    }
}

async fn reply_of(result: Result<actix_web::dev::ServiceResponse, actix_web::Error>) -> Reply {
    match result {
        Ok(resp) => {
            let status = resp.status();
            let headers = resp.headers().clone();
            let body = test::read_body(resp).await.to_vec();
            Reply {
                status,
                headers,
                body,
            }
        }
        Err(e) => {
            let resp = e.error_response();
            Reply {
                status: resp.status(),
                headers: resp.headers().clone(),
                body: Vec::new(),
            }
        }
    }
}

macro_rules! send {
    ($app:expr, $req:expr $(,)?) => {
        reply_of(test::try_call_service(&$app, $req.to_request()).await).await
    };
}

fn get(uri: &str, auth: Option<&str>) -> test::TestRequest {
    let mut req = test::TestRequest::get().uri(uri);
    if let Some(a) = auth {
        req = req.insert_header(("Authorization", a));
    }
    req
}

/// A link as parsed back: rel, href, type.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Link {
    rel: String,
    href: String,
    kind: String,
}

#[derive(Debug, Default)]
struct Entry {
    title: String,
    author: String,
    summary: String,
    language: String,
    links: Vec<Link>,
}

#[derive(Debug, Default)]
struct Feed {
    title: String,
    total: Option<usize>,
    links: Vec<Link>,
    entries: Vec<Entry>,
}

impl Feed {
    fn link(&self, rel: &str) -> Option<&Link> {
        self.links.iter().find(|l| l.rel == rel)
    }
    fn titles(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.title.as_str()).collect()
    }
    fn entry(&self, title: &str) -> &Entry {
        self.entries.iter().find(|e| e.title == title).unwrap()
    }
}

impl Entry {
    fn link(&self, rel: &str) -> Option<&Link> {
        self.links.iter().find(|l| l.rel == rel)
    }
}

fn attr(e: &quick_xml::events::BytesStart<'_>, want: &str) -> String {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == want)
        .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok())
        .map(|v| v.to_string())
        .unwrap_or_default()
}

/// Parses `xml` as an Atom feed, failing on anything that is not
/// well-formed: a reader would refuse it too.
fn parse(xml: &str) -> Feed {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut feed = Feed::default();
    let mut path: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut entry: Option<Entry> = None;
    loop {
        match reader.read_event().expect("well-formed XML") {
            Event::Start(e) => {
                let name = e.name().as_ref().to_owned();
                if name == "entry" {
                    entry = Some(Entry::default());
                }
                path.push(name);
                text.clear();
            }
            Event::Empty(e) if e.name().as_ref() == "link" => {
                let link = Link {
                    rel: attr(&e, "rel"),
                    href: attr(&e, "href"),
                    kind: attr(&e, "type"),
                };
                match entry.as_mut() {
                    Some(en) => en.links.push(link),
                    None => feed.links.push(link),
                }
            }
            Event::Text(t) => text.push_str(&t),
            Event::GeneralRef(r) => {
                let s = match r.as_ref() {
                    "amp" => "&",
                    "lt" => "<",
                    "gt" => ">",
                    "quot" => "\"",
                    "apos" => "'",
                    _ => "",
                };
                text.push_str(s);
            }
            Event::End(_) => {
                let name = path.pop().unwrap_or_default();
                let parent = path.last().map(String::as_str).unwrap_or_default();
                let value = std::mem::take(&mut text);
                match (parent, name.as_str()) {
                    ("feed", "title") => feed.title = value,
                    ("feed", "opensearch:totalResults") => feed.total = value.parse().ok(),
                    ("entry", "title") => entry.as_mut().unwrap().title = value,
                    ("entry", "summary") => entry.as_mut().unwrap().summary = value,
                    ("entry", "dc:language") => entry.as_mut().unwrap().language = value,
                    ("author", "name") if path.len() == 3 => entry.as_mut().unwrap().author = value,
                    (_, "entry") => feed.entries.extend(entry.take()),
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    feed
}

fn feed_of(reply: Reply, uri: &str) -> Feed {
    assert_eq!(reply.status, StatusCode::OK, "{uri}: {}", reply.text());
    assert!(
        reply
            .header("content-type")
            .starts_with("application/atom+xml;profile=opds-catalog"),
        "{uri}: {}",
        reply.header("content-type")
    );
    parse(&reply.text())
}

macro_rules! feed {
    ($app:expr, $uri:expr, $auth:expr) => {{
        let uri: &str = &$uri;
        let auth: &str = &$auth;
        feed_of(send!($app, get(uri, Some(auth))), uri)
    }};
}

fn counter(
    snap: &pharos_test_metrics::CumulativeSnapshotter,
    name: &str,
    label: (&str, &str),
) -> u64 {
    snap.snapshot()
        .into_vec()
        .into_iter()
        .filter_map(|(key, _, _, value)| {
            let hit = key.key().name() == name
                && key
                    .key()
                    .labels()
                    .any(|l| l.key() == label.0 && l.value() == label.1);
            match value {
                DebugValue::Counter(n) if hit => Some(n),
                _ => None,
            }
        })
        .sum()
}

#[actix_web::test]
async fn no_credential_is_challenged_for_basic_auth() {
    let rig = rig(0).await;
    let app = app!(rig.state);
    for uri in [
        "/opds/",
        "/opds",
        "/opds/books",
        "/opds/books/1/file",
        "/opds/opensearch.xml",
    ] {
        let reply = send!(app, get(uri, None));
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{uri}");
        assert!(
            reply
                .header("www-authenticate")
                .starts_with("Basic realm=\"Pharos\""),
            "{uri}: {:?}",
            reply.headers
        );
    }
}

/// FR-007, V1394: the account password never opens the catalogue, an
/// e-reader password does, and a revoked one does not. Disarm check: with
/// the `pharos_opds_auth_total` increment removed from `record_auth`, the
/// counter assertions at the end fail.
#[actix_web::test]
async fn only_a_live_e_reader_password_opens_the_catalogue() {
    let rig = rig(0).await;
    let app = app!(rig.state);
    let recorder = DebuggingRecorder::new();
    let snap = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let main = send!(app, get("/opds/", Some(&basic("reader", MAIN_PASSWORD))));
    assert_eq!(
        main.status,
        StatusCode::UNAUTHORIZED,
        "the account password is refused"
    );
    let ok = send!(app, get("/opds/", Some(&basic("reader", &rig.kobo))));
    assert_eq!(ok.status, StatusCode::OK);
    // Typed in capitals without dashes, as an e-reader keyboard might.
    let typed = rig.kobo.replace('-', "").to_uppercase();
    let ok = send!(app, get("/opds/", Some(&basic("READER", &typed))));
    assert_eq!(
        ok.status,
        StatusCode::OK,
        "case and dashes are not part of it"
    );
    let other_user = send!(app, get("/opds/", Some(&basic("kid", &rig.kobo))));
    assert_eq!(
        other_user.status,
        StatusCode::UNAUTHORIZED,
        "another user's password"
    );
    let nobody = send!(app, get("/opds/", Some(&basic("nobody", &rig.kobo))));
    assert_eq!(nobody.status, StatusCode::UNAUTHORIZED);

    let record = rig
        .state
        .auth
        .account_named("reader")
        .await
        .unwrap()
        .unwrap();
    let user = record.into_user();
    let listed = reader_password::list(&rig.state, &user).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].label, "Kobo");
    assert!(listed[0].last_used_at.is_some(), "a use is recorded");
    assert!(reader_password::revoke(&rig.state, &user, &listed[0].id)
        .await
        .unwrap());
    let revoked = send!(app, get("/opds/", Some(&basic("reader", &rig.kobo))));
    assert_eq!(
        revoked.status,
        StatusCode::UNAUTHORIZED,
        "revoked at once, cache or not"
    );

    let auth = |o| counter(&snap, "pharos_opds_auth_total", ("outcome", o));
    assert_eq!(auth("ok"), 2);
    assert_eq!(auth("bad_password"), 2, "account password + another user's");
    assert_eq!(auth("unknown_user"), 1);
    assert_eq!(auth("revoked"), 1);
    assert_eq!(
        counter(
            &snap,
            "pharos_opds_requests_total",
            ("outcome", "unauthorized")
        ),
        4
    );
}

/// V1394: an e-reader password opens nothing outside `/opds/`, as Basic
/// auth or as a token.
#[actix_web::test]
async fn an_e_reader_password_opens_no_other_route() {
    let rig = rig(0).await;
    let app = app!(rig.state);
    let auth = basic("reader", &rig.kobo);
    for uri in ["/Users/Me", "/Items", "/Items/1/Download"] {
        let reply = send!(app, get(uri, Some(&auth)));
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{uri} with Basic");
        let as_token = send!(
            app,
            get(uri, None).insert_header(("X-Emby-Token", rig.kobo.as_str()))
        );
        assert_eq!(
            as_token.status,
            StatusCode::UNAUTHORIZED,
            "{uri} as a token"
        );
        let query = send!(app, get(&format!("{uri}?api_key={}", rig.kobo), None));
        assert_eq!(query.status, StatusCode::UNAUTHORIZED, "{uri} as api_key");
    }
    // The account's real token is not an e-reader credential either.
    let reply = send!(app, get("/opds/", Some(&format!("Bearer {}", rig.token))));
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[actix_web::test]
async fn the_root_links_every_section_and_both_searches() {
    let rig = rig(0).await;
    let app = app!(rig.state);
    let root = feed!(app, "/opds/", &basic("reader", &rig.kobo));
    assert_eq!(
        root.titles(),
        [
            "All books",
            "Recently added",
            "By author",
            "Shelf A",
            "Shelf B"
        ],
        "two Books libraries: each gets its own feed"
    );
    assert_eq!(root.link("start").unwrap().href, "/opds/");
    let searches: Vec<&Link> = root.links.iter().filter(|l| l.rel == "search").collect();
    assert!(searches
        .iter()
        .any(|l| l.kind == "application/opensearchdescription+xml"
            && l.href == "/opds/opensearch.xml"));
    assert!(searches
        .iter()
        .any(|l| l.href == "/opds/search?q={searchTerms}"));
    let all = root.entry("All books").link("subsection").unwrap();
    assert_eq!(all.href, "/opds/books");
    assert!(all.kind.ends_with("kind=acquisition"));
    let lib = root.entry("Shelf B").link("subsection").unwrap();
    assert_eq!(lib.href, format!("/opds/libraries/{}", rig.lib_b));

    // One library visible: no per-library entries.
    let partial = new_password(&rig.state, "partial", "Kindle").await;
    let root = feed!(app, "/opds/", &basic("partial", &partial));
    assert_eq!(root.titles(), ["All books", "Recently added", "By author"]);
    let refused = send!(
        app,
        get(
            &format!("/opds/libraries/{}", rig.lib_a),
            Some(&basic("partial", &partial))
        ),
    );
    assert_eq!(
        refused.status,
        StatusCode::NOT_FOUND,
        "a library the user may not open"
    );
}

/// FR-002/FR-003: books only, openable only, each with the right
/// acquisition type, by title.
#[actix_web::test]
async fn all_books_lists_openable_books_with_their_types() {
    let rig = rig(0).await;
    let app = app!(rig.state);
    let books = feed!(app, "/opds/books", &basic("reader", &rig.kobo));
    assert_eq!(
        books.titles(),
        ["Alpha Lantern", "Beta Garden", "Delta River", "Gamma Comic"],
        "no DRM Kindle file, no film"
    );
    assert_eq!(books.total, Some(4));
    let acq = |t: &str| {
        books
            .entry(t)
            .link("http://opds-spec.org/acquisition")
            .unwrap()
            .clone()
    };
    assert_eq!(acq("Alpha Lantern").kind, "application/epub+zip");
    assert_eq!(acq("Beta Garden").kind, "application/pdf");
    assert_eq!(acq("Gamma Comic").kind, "application/vnd.comicbook+zip");
    assert_eq!(
        acq("Alpha Lantern").href,
        "/opds/books/00000000000000000000000000000001/file"
    );

    let alpha = books.entry("Alpha Lantern");
    assert_eq!(alpha.author, "Ada Sample");
    assert_eq!(
        alpha.summary, "A keeper & her <lamp>.",
        "escaped and read back"
    );
    assert_eq!(alpha.language, "en");
    let cover = alpha.link("http://opds-spec.org/image").unwrap();
    assert_eq!(cover.kind, "image/jpeg");
    assert!(cover.href.ends_with("/cover"));
    assert!(alpha
        .link("http://opds-spec.org/image/thumbnail")
        .unwrap()
        .href
        .ends_with("/thumbnail"));
    assert!(
        books
            .entry("Beta Garden")
            .link("http://opds-spec.org/image")
            .is_none(),
        "no cover, no image link"
    );

    let recent = feed!(app, "/opds/recent", &basic("reader", &rig.kobo));
    assert_eq!(
        recent.titles(),
        ["Delta River", "Gamma Comic", "Beta Garden", "Alpha Lantern"],
        "newest first"
    );
}

#[actix_web::test]
async fn long_feeds_page_with_next_and_previous() {
    let rig = rig(60).await;
    let app = app!(rig.state);
    let auth = basic("reader", &rig.kobo);
    let first = feed!(app, "/opds/books", &auth);
    assert_eq!(first.entries.len(), 50);
    assert_eq!(first.total, Some(64));
    assert_eq!(first.link("next").unwrap().href, "/opds/books?page=2");
    assert!(first.link("previous").is_none());
    assert_eq!(first.link("last").unwrap().href, "/opds/books?page=2");
    let second = feed!(app, first.link("next").unwrap().href, auth);
    assert_eq!(second.entries.len(), 14);
    assert_eq!(second.link("previous").unwrap().href, "/opds/books?page=1");
    assert!(second.link("next").is_none());
    let mut seen: Vec<&str> = first.titles();
    seen.extend(second.titles());
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), 64, "every book once across the pages");
    let bad = send!(app, get("/opds/books?page=0", Some(&auth)));
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let bad = send!(app, get("/opds/books?page=two", Some(&auth)));
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
}

#[actix_web::test]
async fn search_finds_by_title_or_author_and_describes_itself() {
    let rig = rig(0).await;
    let app = app!(rig.state);
    let auth = basic("reader", &rig.kobo);
    let hits = feed!(app, "/opds/search?q=lantern", &auth);
    assert_eq!(hits.titles(), ["Alpha Lantern"]);
    let hits = feed!(app, "/opds/search?q=wren%20example", &auth);
    assert_eq!(hits.titles(), ["Delta River"]);
    let none = feed!(app, "/opds/search?q=locked", &auth);
    assert!(
        none.entries.is_empty(),
        "an unreadable book is not found either"
    );
    let none = feed!(app, "/opds/search?q=%3Cscript%3E", &auth);
    assert!(none.entries.is_empty());

    let osd = send!(app, get("/opds/opensearch.xml", Some(&auth)));
    assert_eq!(osd.status, StatusCode::OK);
    assert!(osd
        .header("content-type")
        .starts_with("application/opensearchdescription+xml"));
    let body = osd.text();
    assert!(
        body.contains("template=\"/opds/search?q={searchTerms}\""),
        "{body}"
    );
    let mut reader = quick_xml::Reader::from_str(&body);
    while !matches!(reader.read_event().expect("well-formed"), Event::Eof) {}
}

#[actix_web::test]
async fn by_author_leads_to_each_authors_books() {
    let rig = rig(0).await;
    let app = app!(rig.state);
    let auth = basic("reader", &rig.kobo);
    let authors = feed!(app, "/opds/authors", &auth);
    assert_eq!(authors.titles(), ["Ada Sample", "Wren Example"]);
    let href = authors
        .entry("Wren Example")
        .link("subsection")
        .unwrap()
        .href
        .clone();
    let books = feed!(app, href, auth);
    assert_eq!(books.title, "Wren Example");
    assert_eq!(books.titles(), ["Delta River"]);
    let missing = send!(app, get("/opds/authors/0123456789abcdef", Some(&auth)));
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

/// FR-005, V1395: the catalogue applies the user's library access exactly
/// as the library does.
#[actix_web::test]
async fn a_user_sees_only_the_books_they_may_open() {
    let rig = rig(0).await;
    let app = app!(rig.state);
    let kid = new_password(&rig.state, "kid", "Kobo").await;
    let books = feed!(app, "/opds/books", &basic("kid", &kid));
    assert!(
        books.entries.is_empty(),
        "no library, no books: {:?}",
        books.titles()
    );
    let authors = feed!(app, "/opds/authors", &basic("kid", &kid));
    assert!(authors.entries.is_empty());
    for path in ["file", "cover", "thumbnail"] {
        let reply = send!(
            app,
            get(&format!("/opds/books/1/{path}"), Some(&basic("kid", &kid))),
        );
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{path}");
    }

    let partial = new_password(&rig.state, "partial", "Kobo").await;
    let books = feed!(app, "/opds/books", &basic("partial", &partial));
    assert_eq!(books.titles(), ["Delta River"]);
}

/// FR-006: the download streams the file's bytes with its type, a safe
/// name and range support, and is counted by format.
#[actix_web::test]
async fn a_download_streams_the_exact_bytes() {
    let rig = rig(0).await;
    let app = app!(rig.state);
    let recorder = DebuggingRecorder::new();
    let snap = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let auth = basic("reader", &rig.kobo);
    let uri = "/opds/books/00000000000000000000000000000001/file";

    let reply = send!(app, get(uri, Some(&auth)));
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("content-type"), "application/epub+zip");
    assert_eq!(
        reply.header("content-disposition"),
        "attachment; filename=\"alpha.epub\""
    );
    assert_eq!(reply.header("accept-ranges"), "bytes");
    assert_eq!(reply.body, rig.alpha_bytes);

    let ranged = send!(
        app,
        get(uri, Some(&auth)).insert_header(("Range", "bytes=100-199"))
    );
    assert_eq!(ranged.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(ranged.body, rig.alpha_bytes[100..200]);

    // The length is stamped by the h1 encoder from the body's declared size,
    // which the in-process harness never materialises: assert the size.
    let head = test::call_service(
        &app,
        test::TestRequest::default()
            .method(actix_web::http::Method::HEAD)
            .uri(uri)
            .insert_header(("Authorization", auth.as_str()))
            .to_request(),
    )
    .await;
    assert_eq!(head.status(), StatusCode::OK);
    use actix_web::body::MessageBody;
    assert_eq!(
        head.into_body().size(),
        actix_web::body::BodySize::Sized(rig.alpha_bytes.len() as u64)
    );

    let drm = send!(app, get("/opds/books/4/file", Some(&auth)));
    assert_eq!(
        drm.status,
        StatusCode::NOT_FOUND,
        "an unlisted book cannot be fetched"
    );
    let film = send!(app, get("/opds/books/5/file", Some(&auth)));
    assert_eq!(film.status, StatusCode::NOT_FOUND, "nor a film");

    assert_eq!(
        counter(&snap, "pharos_opds_downloads_total", ("format", "epub")),
        2
    );
    assert_eq!(
        counter(&snap, "pharos_opds_requests_total", ("route", "download")),
        5
    );
}
