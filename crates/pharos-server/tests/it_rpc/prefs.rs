//! Spec 017 T068: `Prefs` stores a signed-in user's own values under an
//! allow-listed key, bounds their size, and refuses (and logs) the rest.
//! Driven over an in-memory pipe on the test thread so the thread-local
//! subscriber and recorder see the handler. Disarm checks are noted on each
//! test.
use pharos_api::pharos_capnp::{done, pref_result, prefs, session};
use pharos_server::api::pharos::rpc;

use crate::common::{seed_rich, Fixture};
use crate::telemetry::{connect, session_for, subscriber, Captured};

const KEY: &str = "diagnostics.opt_in";

async fn prefs_of(session: &session::Client) -> prefs::Client {
    session.prefs_request().send().pipeline.get_prefs()
}

/// What `get` answered: the value, `notSet`, or the refusal's reason.
#[derive(Debug, PartialEq)]
enum Got {
    Value(Vec<u8>),
    NotSet,
    Refused(String),
}

async fn get(prefs: &prefs::Client, key: &str) -> Got {
    let mut req = prefs.get_request();
    req.get().init_ctx();
    req.get().set_key(key);
    let reply = req.send().promise.await.expect("get answers");
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        pref_result::Value(v) => Got::Value(v.unwrap().to_vec()),
        pref_result::NotSet(()) => Got::NotSet,
        pref_result::Error(e) => Got::Refused(reason(e.unwrap())),
    }
}

/// `Ok` when stored; `Err` with the refusal's reason.
async fn set(prefs: &prefs::Client, key: &str, value: &[u8]) -> Result<(), String> {
    let mut req = prefs.set_request();
    req.get().init_ctx();
    req.get().set_key(key);
    req.get().set_value(value);
    let reply = req.send().promise.await.expect("set answers");
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        done::Ok(()) => Ok(()),
        done::Error(e) => Err(reason(e.unwrap())),
    }
}

fn reason(e: pharos_api::pharos_capnp::error::Reader<'_>) -> String {
    e.get_reason().unwrap().to_str().unwrap().to_owned()
}

async fn prefs_for(fx: &Fixture, token: &str) -> prefs::Client {
    let client = connect(fx);
    prefs_of(&session_for(&client, token).await).await
}

/// A pref nobody set reads as `notSet`, not as an empty value.
#[actix_web::test]
async fn an_unset_pref_reads_as_not_set() {
    let fx = seed_rich().await;
    tokio::task::LocalSet::new()
        .run_until(async {
            let prefs = prefs_for(&fx, &fx.user_token).await;
            assert_eq!(get(&prefs, KEY).await, Got::NotSet);
        })
        .await;
}

/// A set value reads back byte for byte, and a second set replaces it.
#[actix_web::test]
async fn a_set_pref_reads_back_and_a_second_set_replaces_it() {
    let fx = seed_rich().await;
    tokio::task::LocalSet::new()
        .run_until(async {
            let prefs = prefs_for(&fx, &fx.user_token).await;
            set(&prefs, KEY, b"true").await.expect("stored");
            assert_eq!(get(&prefs, KEY).await, Got::Value(b"true".to_vec()));
            set(&prefs, KEY, b"false").await.expect("stored");
            assert_eq!(get(&prefs, KEY).await, Got::Value(b"false".to_vec()));
        })
        .await;
}

/// An unknown key is refused on both calls, nothing is stored, and the WARN
/// names the key and the size but not the value. Disarm check: with the
/// allow-list test in `allowed_key` removed, `set` succeeds and this fails.
#[actix_web::test]
async fn an_unknown_key_is_refused_and_logged_without_its_value() {
    let fx = seed_rich().await;
    let logs = Captured::default();
    let _log = tracing::subscriber::set_default(subscriber(&logs));
    tokio::task::LocalSet::new()
        .run_until(async {
            let prefs = prefs_for(&fx, &fx.user_token).await;
            let refused = set(&prefs, "not.a.key", b"hunter2-secret").await;
            assert_eq!(refused, Err("unknown pref key".to_owned()));
            assert_eq!(
                get(&prefs, "not.a.key").await,
                Got::Refused("unknown pref key".to_owned())
            );
        })
        .await;
    let text = logs.text();
    assert!(
        text.contains("WARN") && text.contains("pref refused: unknown key"),
        "{text}"
    );
    assert!(
        text.contains("not.a.key") && text.contains("size=14"),
        "{text}"
    );
    assert!(
        !text.contains("hunter2-secret"),
        "the value reached the log:\n{text}"
    );
}

/// A value over the cap is refused with a WARN that carries the key and the
/// size, and the earlier value survives. Disarm check: with the size test in
/// `write` removed, the oversize set is stored and this fails.
#[actix_web::test]
async fn an_oversize_value_is_refused_and_logged_and_the_old_value_stays() {
    let fx = seed_rich().await;
    let logs = Captured::default();
    let _log = tracing::subscriber::set_default(subscriber(&logs));
    let too_big = vec![b'x'; rpc::MAX_PREF_VALUE_BYTES + 1];
    tokio::task::LocalSet::new()
        .run_until(async {
            let prefs = prefs_for(&fx, &fx.user_token).await;
            set(&prefs, KEY, b"true").await.expect("stored");
            let refused = set(&prefs, KEY, &too_big).await.expect_err("refused");
            assert!(refused.contains("pref_value_bytes"), "{refused}");
            assert_eq!(get(&prefs, KEY).await, Got::Value(b"true".to_vec()));
        })
        .await;
    let text = logs.text();
    assert!(text.contains("pref refused: value too large"), "{text}");
    assert!(
        text.contains("diagnostics.opt_in") && text.contains(&format!("size={}", too_big.len())),
        "{text}"
    );
}

/// One user's pref is invisible to another user. Disarm check: with the
/// store key ignoring the user, the second user reads the first's value.
#[actix_web::test]
async fn one_user_cannot_read_anothers_pref() {
    let fx = seed_rich().await;
    tokio::task::LocalSet::new()
        .run_until(async {
            let guest = prefs_for(&fx, &fx.user_token).await;
            let admin = prefs_for(&fx, &fx.admin_token).await;
            set(&guest, KEY, b"true").await.expect("stored");
            assert_eq!(get(&admin, KEY).await, Got::NotSet);
            set(&admin, KEY, b"false").await.expect("stored");
            assert_eq!(get(&guest, KEY).await, Got::Value(b"true".to_vec()));
        })
        .await;
}

/// Prefs calls are counted under their interface, so the dashboard that
/// charts `pharos_rpc_calls_total` sees them. Disarm check: serving the
/// capability without `server::prefs::client` leaves the series absent.
#[actix_web::test]
async fn prefs_calls_are_counted_under_their_interface() {
    let fx = seed_rich().await;
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    tokio::task::LocalSet::new()
        .run_until(async {
            let prefs = prefs_for(&fx, &fx.user_token).await;
            set(&prefs, KEY, b"true").await.expect("stored");
            get(&prefs, KEY).await;
        })
        .await;
    for method in ["get", "set"] {
        assert_eq!(
            crate::telemetry::counter(
                &snapshotter,
                "pharos_rpc_calls_total",
                &[("interface", "prefs"), ("method", method)]
            ),
            1,
            "{method}"
        );
    }
}

/// `description.language` is the user's Jellyfin `DescriptionLanguagePreference`
/// (ADR-0062), not a pref row: a set shows in the configuration blob next to
/// the fields already there, reads back, and a value that is not a language
/// is refused. Disarm check: with the `DESCRIPTION_LANGUAGE` branch in
/// `Prefs::write` removed, the value lands in `user_prefs` and the blob
/// assertion fails.
#[actix_web::test]
async fn the_description_language_is_kept_in_the_user_configuration() {
    use pharos_core::PreferenceStore;
    let fx = seed_rich().await;
    let stores = fx.state.stores.clone();
    stores
        .set_user_configuration(fx.user_id, r#"{"AudioLanguagePreference":"jpn"}"#)
        .await
        .unwrap();
    tokio::task::LocalSet::new()
        .run_until(async {
            let prefs = prefs_for(&fx, &fx.user_token).await;
            const LANG: &str = "description.language";
            assert_eq!(get(&prefs, LANG).await, Got::NotSet);
            set(&prefs, LANG, b"fra").await.expect("stored");
            assert_eq!(get(&prefs, LANG).await, Got::Value(b"fra".to_vec()));
            let raw: serde_json::Value = serde_json::from_str(
                &stores
                    .get_user_configuration(fx.user_id)
                    .await
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(raw["DescriptionLanguagePreference"], "fra");
            assert_eq!(raw["AudioLanguagePreference"], "jpn");
            assert!(set(&prefs, LANG, b"not a language").await.is_err());
            assert_eq!(get(&prefs, LANG).await, Got::Value(b"fra".to_vec()));
            set(&prefs, LANG, b"").await.expect("cleared");
            assert_eq!(get(&prefs, LANG).await, Got::NotSet);
        })
        .await;
}

/// `whats_new.seen` keeps the newest what's-new entry a user was shown
/// (spec 027, V495): a number reads back, and anything else is refused with
/// a WARN while the stored number stays. Disarm check: with the
/// `is_whats_new_entry` test in `Prefs::write` removed, `b"soon"` is stored
/// and the read-back assertion fails.
#[actix_web::test]
async fn whats_new_seen_holds_only_an_entry_number() {
    let fx = seed_rich().await;
    let logs = Captured::default();
    let _log = tracing::subscriber::set_default(subscriber(&logs));
    tokio::task::LocalSet::new()
        .run_until(async {
            let prefs = prefs_for(&fx, &fx.user_token).await;
            const SEEN: &str = "whats_new.seen";
            assert_eq!(get(&prefs, SEEN).await, Got::NotSet);
            set(&prefs, SEEN, b"3").await.expect("stored");
            for bad in [&b"soon"[..], b"", b"-1", b"1234567890"] {
                assert!(set(&prefs, SEEN, bad).await.is_err(), "{bad:?}");
            }
            assert_eq!(get(&prefs, SEEN).await, Got::Value(b"3".to_vec()));
        })
        .await;
    let text = logs.text();
    assert!(
        text.contains("pref refused: not an entry number") && text.contains("whats_new.seen"),
        "{text}"
    );
}
