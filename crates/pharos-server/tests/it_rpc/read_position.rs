//! Spec 023 slice 2: a book's native read position (`UserData.setReadPosition`)
//! stored apart from its playback position, refused for anything but a
//! readable book, and listed as "Continue reading" until the book is played.
//! Driven over an in-memory pipe on the test thread so the thread-local
//! recorder and subscriber see the handler.

use pharos_api::pharos_capnp::{
    detail_result, error, home_result, item_id, items_result, session, user_state_result,
    BookFormat, Kind,
};
use pharos_core::{
    BookFormat as CoreFormat, BookMeta, MediaItem, MediaKind, MediaStore as _,
    ReadPositionStore as _, UserDataStore as _,
};
use pharos_jellyfin_api::dto::WireId;

use crate::common::Fixture;
use crate::telemetry::{connect, counter, session_for, subscriber, Captured};

const EPUB: u64 = 7001;
const SECOND_EPUB: u64 = 7002;
const MOBI: u64 = 7003;

/// A saved position as (played, locator, fraction, position in ms).
type Saved = (bool, String, f32, u64);

async fn seed_books(fx: &Fixture) {
    for (id, name, format, author) in [
        (EPUB, "Book A", CoreFormat::Epub, Some("Author A")),
        (SECOND_EPUB, "Emma", CoreFormat::Epub, None),
        (MOBI, "Locked", CoreFormat::Unreadable, None),
    ] {
        fx.state
            .stores
            .put(MediaItem {
                id,
                path: format!("/libA/books/{name}.bin").into(),
                title: name.into(),
                kind: MediaKind::Book,
                book: Some(BookMeta {
                    format,
                    author: author.map(Into::into),
                    page_count: None,
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();
    }
}

fn set_id(mut out: item_id::Builder<'_>, id: WireId) {
    match id {
        WireId::Real(n) => out.set_real(n),
        WireId::Synth(n) => out.set_synth(n),
    }
}

async fn save(
    session: &session::Client,
    id: WireId,
    locator: &str,
    fraction: f32,
) -> Result<Saved, String> {
    save_at(session, id, locator, fraction, 0).await
}

async fn save_at(
    session: &session::Client,
    id: WireId,
    locator: &str,
    fraction: f32,
    read_at_ms: i64,
) -> Result<Saved, String> {
    let user_data = session.user_data_request().send().pipeline.get_user_data();
    let mut req = user_data.set_read_position_request();
    req.get().init_ctx();
    set_id(req.get().init_id(), id);
    req.get().set_locator(locator);
    req.get().set_fraction(fraction);
    req.get().set_read_at(read_at_ms);
    let reply = req.send().promise.await.expect("setReadPosition");
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        user_state_result::State(s) => {
            let s = s.unwrap();
            Ok((
                s.get_played(),
                s.get_read_locator().unwrap().to_str().unwrap().to_owned(),
                s.get_read_fraction(),
                s.get_position_ms(),
            ))
        }
        user_state_result::Error(e) => Err(match e.unwrap().get_kind().which().unwrap() {
            error::kind::Invalid(i) => format!(
                "invalid: {}",
                i.unwrap().get_field().unwrap().to_str().unwrap()
            ),
            error::kind::NotFound(what) => {
                format!("not found: {}", what.unwrap().to_str().unwrap())
            }
            _ => "other".to_owned(),
        }),
    }
}

/// `Library.detail` of a book: its format, author and saved position.
struct BookDetail {
    format: Option<BookFormat>,
    author: String,
    saved: Saved,
}

async fn detail(session: &session::Client, id: u64) -> BookDetail {
    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.detail_request();
    req.get().init_ctx();
    req.get().init_id().set_real(id);
    let reply = req.send().promise.await.expect("detail");
    let detail_result::Detail(d) = reply.get().unwrap().get_result().unwrap().which().unwrap()
    else {
        panic!("detail {id} answered an error");
    };
    let d = d.unwrap();
    let state = d.get_summary().unwrap().get_user_state().unwrap();
    BookDetail {
        format: d
            .has_book()
            .then(|| d.get_book().unwrap().get_format().unwrap()),
        author: if d.has_book() {
            d.get_book()
                .unwrap()
                .get_author()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        } else {
            String::new()
        },
        saved: (
            state.get_played(),
            state
                .get_read_locator()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned(),
            state.get_read_fraction(),
            state.get_position_ms(),
        ),
    }
}

fn real_ids(
    list: capnp::struct_list::Reader<'_, pharos_api::pharos_capnp::item_summary::Owned>,
) -> Vec<u64> {
    list.iter()
        .map(|item| match item.get_id().unwrap().which().unwrap() {
            item_id::Real(n) => n,
            item_id::Synth(_) => panic!("a stored item"),
        })
        .collect()
}

/// The ids in `Home.reading` and the saved locators on them.
async fn home_reading(session: &session::Client) -> (Vec<u64>, Vec<String>) {
    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.home_request();
    req.get().init_ctx();
    let reply = req.send().promise.await.expect("home");
    let home_result::Home(home) = reply.get().unwrap().get_result().unwrap().which().unwrap()
    else {
        panic!("home answered an error");
    };
    let reading = home.unwrap().get_reading().unwrap();
    let locators = reading
        .iter()
        .map(|i| {
            i.get_user_state()
                .unwrap()
                .get_read_locator()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    (real_ids(reading), locators)
}

async fn resume_ids(session: &session::Client, kinds: &[Kind]) -> Vec<u64> {
    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.resume_request();
    req.get().init_ctx();
    let mut list = req.get().init_kinds(kinds.len() as u32);
    for (i, kind) in kinds.iter().enumerate() {
        list.set(i as u32, *kind);
    }
    let reply = req.send().promise.await.expect("resume");
    let items_result::Page(page) = reply.get().unwrap().get_result().unwrap().which().unwrap()
    else {
        panic!("resume answered an error");
    };
    real_ids(page.unwrap().get_items().unwrap())
}

/// A position saved over the wire reads back on the book's detail with its
/// format and author, and on the `reading` row, and the item is not played.
/// Disarm check: with `write_read_state` not called in `Library.detail`, the
/// detail's locator is empty.
#[actix_web::test]
async fn a_saved_position_reads_back_with_the_books_format() {
    let fx = crate::common::seed_rich().await;
    seed_books(&fx).await;
    tokio::task::LocalSet::new()
        .run_until(async {
            let client = connect(&fx);
            let session = session_for(&client, &fx.user_token).await;
            let before = detail(&session, EPUB).await;
            assert_eq!(before.format, Some(BookFormat::Epub));
            assert_eq!(before.author, "Author A");
            assert_eq!(before.saved, (false, String::new(), 0.0, 0));

            let saved = save(&session, WireId::Real(EPUB), "epub:3:120", 0.25).await;
            assert_eq!(saved, Ok((false, "epub:3:120".to_owned(), 0.25, 0)));
            assert_eq!(detail(&session, EPUB).await.saved, saved.unwrap());
            // A later save replaces the earlier one.
            save(&session, WireId::Real(EPUB), "epub:4:0", 0.5)
                .await
                .unwrap();
            assert_eq!(detail(&session, EPUB).await.saved.1, "epub:4:0");

            assert_eq!(
                home_reading(&session).await,
                (vec![EPUB], vec!["epub:4:0".to_owned()])
            );
            assert_eq!(detail(&session, EPUB).await.format, Some(BookFormat::Epub));
            // A film's detail names no book.
            assert_eq!(detail(&session, fx.rich_item_id).await.format, None);
            assert_eq!(
                detail(&session, MOBI).await.format,
                Some(BookFormat::Unreadable)
            );
        })
        .await;
}

/// V355: each refusal answers `invalid` naming the argument, stores nothing,
/// counts under its outcome and logs the item id and reason.
/// Disarm check: with any one check removed from
/// `services::user_data::set_read_position`, that case stores a position.
#[actix_web::test]
async fn a_refused_position_stores_nothing() {
    let fx = crate::common::seed_rich().await;
    seed_books(&fx).await;
    let logs = Captured::default();
    let _log = tracing::subscriber::set_default(subscriber(&logs));
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let long = "x".repeat(257);
    tokio::task::LocalSet::new()
        .run_until(async {
            let client = connect(&fx);
            let session = session_for(&client, &fx.user_token).await;
            let invalid = |field: &str| Err(format!("invalid: {field}"));
            let book = WireId::Real(EPUB);
            assert_eq!(
                save(&session, WireId::Real(fx.rich_item_id), "l", 0.5).await,
                invalid("id")
            );
            assert_eq!(
                save(&session, WireId::Real(MOBI), "l", 0.5).await,
                invalid("id")
            );
            assert_eq!(
                save(&session, WireId::Synth(9), "l", 0.5).await,
                invalid("id")
            );
            assert_eq!(save(&session, book, "", 0.5).await, invalid("locator"));
            assert_eq!(save(&session, book, &long, 0.5).await, invalid("locator"));
            for fraction in [f32::NAN, f32::INFINITY, -0.1, 1.5] {
                assert_eq!(
                    save(&session, book, "l", fraction).await,
                    invalid("fraction")
                );
            }
            // The largest locator and both ends of the range are accepted.
            assert!(save(&session, book, &"x".repeat(256), 0.0).await.is_ok());
            assert!(save(&session, WireId::Real(SECOND_EPUB), "l", 1.0)
                .await
                .is_ok());
        })
        .await;
    let (user, stores) = (fx.user_id, &fx.state.stores);
    for id in [fx.rich_item_id, MOBI] {
        assert_eq!(stores.get_read_position(user, id).await.unwrap(), None);
    }
    // The rejected saves left the one accepted locator, not a refused one.
    let kept = stores.get_read_position(user, EPUB).await.unwrap().unwrap();
    assert_eq!((kept.locator.len(), kept.fraction), (256, 0.0));
    let outcome = |o| {
        counter(
            &snapshotter,
            "pharos_read_position_total",
            &[("outcome", o)],
        )
    };
    assert_eq!(outcome("not_book"), 2);
    assert_eq!(outcome("unreadable"), 1);
    assert_eq!(outcome("bad_locator"), 2);
    assert_eq!(outcome("bad_fraction"), 4);
    assert_eq!(outcome("saved"), 1);
    assert_eq!(outcome("finished"), 1);
    let text = logs.text();
    assert!(
        text.contains("read position refused")
            && text.contains("reason=\"unreadable\"")
            && text.contains(&format!("media.id={MOBI}")),
        "{text}"
    );
}

/// V354, both directions: a read position leaves the resume position alone,
/// and a Jellyfin playback report leaves the read position alone.
/// Disarm check: with `set_read_position` writing `last_played_position_ticks`,
/// or the progress report clearing the read position, the matching half fails.
#[actix_web::test]
async fn read_and_playback_positions_stay_apart() {
    use actix_web::test;
    let fx = crate::common::seed_rich().await;
    seed_books(&fx).await;
    let app = test::init_service(crate::common::build_app(fx.state.clone())).await;
    let stores = &fx.state.stores;
    let mut data = stores.get_user_data(fx.user_id, EPUB).await.unwrap();
    data.last_played_position_ticks = 6_000_000_000;
    stores.set_user_data(fx.user_id, EPUB, data).await.unwrap();

    tokio::task::LocalSet::new()
        .run_until(async {
            let client = connect(&fx);
            let session = session_for(&client, &fx.user_token).await;
            let saved = save(&session, WireId::Real(EPUB), "epub:1:1", 0.9)
                .await
                .unwrap();
            assert_eq!(
                saved.3, 600_000,
                "the playback position, in ms, is untouched"
            );
            // Finishing the book marks it played and still leaves the ticks.
            let finished = save(&session, WireId::Real(EPUB), "epub:9:9", 0.99)
                .await
                .unwrap();
            assert!(finished.0);
            assert_eq!(finished.3, 600_000);
        })
        .await;
    let stored = stores.get_user_data(fx.user_id, EPUB).await.unwrap();
    assert_eq!(stored.last_played_position_ticks, 6_000_000_000);

    let report = test::TestRequest::post()
        .uri("/Sessions/Playing/Progress")
        .insert_header(("X-Emby-Token", fx.user_token.as_str()))
        .set_json(serde_json::json!({
            "ItemId": WireId::Real(EPUB).to_string(),
            "PlaySessionId": "s1",
            "PositionTicks": 1_234_000u64,
            "IsPaused": false,
        }))
        .to_request();
    assert!(test::call_service(&app, report).await.status().is_success());
    let stopped = test::TestRequest::post()
        .uri("/Sessions/Playing/Stopped")
        .insert_header(("X-Emby-Token", fx.user_token.as_str()))
        .set_json(serde_json::json!({
            "ItemId": WireId::Real(EPUB).to_string(),
            "PlaySessionId": "s1",
            "PositionTicks": 1_234_000u64,
        }))
        .to_request();
    assert!(test::call_service(&app, stopped)
        .await
        .status()
        .is_success());
    let progress = stores.get_user_data(fx.user_id, EPUB).await.unwrap();
    assert_eq!(
        progress.last_played_position_ticks, 1_234_000,
        "the report landed"
    );
    let kept = stores
        .get_read_position(fx.user_id, EPUB)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((kept.locator.as_str(), kept.fraction), ("epub:9:9", 0.99));
}

/// FR-012: a part-read book is on `Home.reading` and in `Library.resume` for
/// the book kind (not for the film kinds), newest read first; saving at 0.99
/// plays it, and a played book leaves both lists; a film's resume row does
/// not mention books.
/// Disarm check: with part-read books not merged into `resume`, the
/// resume assertions fail; with the played filter dropped in
/// `part_read_items`, the finished book stays listed.
#[actix_web::test]
async fn a_part_read_book_is_listed_until_played() {
    let fx = crate::common::seed_rich().await;
    seed_books(&fx).await;
    tokio::task::LocalSet::new()
        .run_until(async {
            let client = connect(&fx);
            let session = session_for(&client, &fx.user_token).await;
            assert_eq!(home_reading(&session).await.0, Vec::<u64>::new());
            assert_eq!(resume_ids(&session, &[Kind::Book]).await, Vec::<u64>::new());

            save_at(
                &session,
                WireId::Real(EPUB),
                "a",
                0.2,
                (now_secs() - 10) * 1000,
            )
            .await
            .unwrap();
            // Reads are stamped in unix seconds; this one is newer.
            fx.state
                .stores
                .set_read_position(
                    fx.user_id,
                    SECOND_EPUB,
                    pharos_core::ReadPosition {
                        locator: "b".into(),
                        fraction: 0.4,
                        updated_at: now_secs(),
                    },
                )
                .await
                .unwrap();
            assert_eq!(home_reading(&session).await.0, vec![SECOND_EPUB, EPUB]);
            assert_eq!(
                resume_ids(&session, &[Kind::Book]).await,
                vec![SECOND_EPUB, EPUB]
            );
            assert_eq!(
                resume_ids(&session, &[Kind::Movie, Kind::Episode]).await,
                Vec::<u64>::new()
            );

            // A save of 0 is not "part read".
            save(&session, WireId::Real(EPUB), "a", 0.0).await.unwrap();
            assert_eq!(home_reading(&session).await.0, vec![SECOND_EPUB]);
            save(&session, WireId::Real(EPUB), "a", 0.3).await.unwrap();

            let done = save(&session, WireId::Real(SECOND_EPUB), "z", 0.99)
                .await
                .unwrap();
            assert!(done.0, "0.99 marks the book played");
            assert_eq!(home_reading(&session).await.0, vec![EPUB]);
            assert_eq!(resume_ids(&session, &[Kind::Book]).await, vec![EPUB]);
            // Playing again does not count another play.
            save(&session, WireId::Real(SECOND_EPUB), "z", 1.0)
                .await
                .unwrap();
            let again = stores_play_count(&fx, SECOND_EPUB).await;
            assert_eq!(again, 1);
        })
        .await;
}

/// V1131: of two readings, the one read last is kept, whichever arrives last.
/// A save that loses is answered with the stored position, not an error,
/// counts as `stale`, and never marks the book played.
#[actix_web::test]
async fn the_newest_reading_wins_whatever_arrives_last() {
    let fx = crate::common::seed_rich().await;
    seed_books(&fx).await;
    let logs = Captured::default();
    let _log = tracing::subscriber::set_default(subscriber(&logs));
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let book = WireId::Real(EPUB);
    tokio::task::LocalSet::new()
        .run_until(async {
            let client = connect(&fx);
            let session = session_for(&client, &fx.user_token).await;
            let ago = |secs: i64| (now_secs() - secs) * 1000;
            // Read on the desktop 100 s ago, then the phone uploads a reading
            // from 1000 s ago, and one that finishes the book from 500 s ago.
            assert_eq!(
                save_at(&session, book, "desktop", 0.3, ago(100)).await,
                Ok((false, "desktop".to_owned(), 0.3, 0))
            );
            assert_eq!(
                save_at(&session, book, "phone", 0.6, ago(1000)).await,
                Ok((false, "desktop".to_owned(), 0.3, 0)),
                "an older reading must not replace the newer one"
            );
            assert_eq!(
                save_at(&session, book, "phone-end", 1.0, ago(500)).await,
                Ok((false, "desktop".to_owned(), 0.3, 0)),
                "a stale finishing reading must not mark the book played"
            );
            assert_eq!(stores_play_count(&fx, EPUB).await, 0);
            // A newer reading from the phone replaces it.
            assert_eq!(
                save_at(&session, book, "phone-new", 0.5, ago(10)).await,
                Ok((false, "phone-new".to_owned(), 0.5, 0))
            );
        })
        .await;
    let outcome = |o| {
        counter(
            &snapshotter,
            "pharos_read_position_total",
            &[("outcome", o)],
        )
    };
    assert_eq!((outcome("stale"), outcome("saved")), (2, 2));
    assert_eq!(outcome("finished"), 0);
    let text = logs.text();
    assert!(
        text.contains("older than the stored") && text.contains(&format!("media.id={EPUB}")),
        "{text}"
    );
}

/// FR-002: a time ahead of the server's clock counts as now, so it cannot
/// outrank later readings; 0 (old clients) is now.
#[actix_web::test]
async fn a_future_read_time_counts_as_now_and_zero_is_now() {
    let fx = crate::common::seed_rich().await;
    seed_books(&fx).await;
    let book = WireId::Real(EPUB);
    tokio::task::LocalSet::new()
        .run_until(async {
            let client = connect(&fx);
            let session = session_for(&client, &fx.user_token).await;
            let hour_ahead = (now_secs() + 3600) * 1000;
            save_at(&session, book, "ahead", 0.2, hour_ahead)
                .await
                .unwrap();
            let kept = fx.state.stores.get_read_position(fx.user_id, EPUB).await;
            let kept = kept.unwrap().unwrap();
            assert!(
                kept.updated_at <= now_secs(),
                "a future readAt was stored as {}",
                kept.updated_at
            );
            // A reading made a minute ago loses to the clamped one; an old
            // client's (0, now) wins over it.
            assert_eq!(
                save_at(&session, book, "minute-ago", 0.1, (now_secs() - 60) * 1000)
                    .await
                    .unwrap()
                    .1,
                "ahead"
            );
            assert_eq!(
                save(&session, book, "legacy", 0.4).await.unwrap().1,
                "legacy"
            );
        })
        .await;
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

async fn stores_play_count(fx: &Fixture, id: u64) -> u32 {
    fx.state
        .stores
        .get_user_data(fx.user_id, id)
        .await
        .unwrap()
        .play_count
}
