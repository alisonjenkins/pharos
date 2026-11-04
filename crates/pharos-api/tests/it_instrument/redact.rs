//! T012: a `$secret` param or result never reaches a log line or span, even
//! with the interface at debug. Disarm check: with `redact::is_secret`
//! returning false, every test here fails.

use std::collections::BTreeSet;
use std::future::Future;
use std::rc::Rc;

use capnp::introspect::{Introspect, TypeVariant};
use capnp::schema::StructSchema;
use pharos_api::pharos_capnp::{
    bootstrap, playback_grant, session, telemetry, telemetry_event, EventKind, Level,
};
use pharos_api::redact::{is_secret, REDACTED};
use pharos_api::server::{self, SECRET_METHODS};
use pharos_api::Connection;

use crate::support::{block_on, fill_ctx, observe, Observed};

const PASSWORD: &str = "hunter2-password-secret";
const NEW_PASSWORD: &str = "correct-horse-new-secret";
const DEVICE_TOKEN: &str = "devtok-0123456789-secret";
const QC_SECRET: &str = "qc-poll-secret-abcdef";
const FIELD_SECRET: &str = "clientfield-secret-xyz";

/// Echoes secrets back in its results so both directions are exercised.
struct Boot;

/// Answers `Telemetry.submit` ok, so the call's payload is logged and the
/// coverage check sees it called.
struct TelemetryBoot;

/// Answers `PlaybackGrant.decision` with an error, so the call completes.
struct GrantBoot;

/// Answers `Session.changePassword` ok, so the call's payload is logged.
struct SessionBoot;

impl server::session::Handler for SessionBoot {
    fn change_password(
        self: Rc<Self>,
        _params: session::ChangePasswordParams,
        results: &mut session::ChangePasswordResults,
    ) -> impl Future<Output = Result<(), capnp::Error>> {
        results.get().init_result().set_ok(());
        std::future::ready(Ok(()))
    }
}

impl server::telemetry::Handler for TelemetryBoot {
    fn submit(
        self: Rc<Self>,
        _params: telemetry::SubmitParams,
        results: &mut telemetry::SubmitResults,
    ) -> impl Future<Output = Result<(), capnp::Error>> {
        results.get().init_result().set_ok(());
        std::future::ready(Ok(()))
    }
}

impl server::playback_grant::Handler for GrantBoot {
    fn decision(
        self: Rc<Self>,
        _params: playback_grant::DecisionParams,
        results: &mut playback_grant::DecisionResults,
    ) -> impl Future<Output = Result<(), capnp::Error>> {
        results
            .get()
            .init_result()
            .init_error()
            .set_reason("no grant");
        std::future::ready(Ok(()))
    }
}

impl server::bootstrap::Handler for Boot {
    fn submit_anonymous(
        self: Rc<Self>,
        _params: bootstrap::SubmitAnonymousParams,
        results: &mut bootstrap::SubmitAnonymousResults,
    ) -> impl Future<Output = Result<(), capnp::Error>> {
        results.get().init_result().set_ok(());
        std::future::ready(Ok(()))
    }

    fn resume(
        self: Rc<Self>,
        _params: bootstrap::ResumeParams,
        results: &mut bootstrap::ResumeResults,
    ) -> impl Future<Output = Result<(), capnp::Error>> {
        let mut err = results.get().init_result().init_status().init_error();
        err.set_reason("token not recognised");
        err.get_kind().set_revoked(());
        std::future::ready(Ok(()))
    }

    fn login(
        self: Rc<Self>,
        _params: bootstrap::LoginParams,
        results: &mut bootstrap::LoginResults,
    ) -> impl Future<Output = Result<(), capnp::Error>> {
        results
            .get()
            .init_result()
            .init_ok()
            .set_device_token(DEVICE_TOKEN);
        std::future::ready(Ok(()))
    }

    fn quick_connect_initiate(
        self: Rc<Self>,
        _params: bootstrap::QuickConnectInitiateParams,
        results: &mut bootstrap::QuickConnectInitiateResults,
    ) -> impl Future<Output = Result<(), capnp::Error>> {
        let mut pending = results.get().init_result().init_pending();
        pending.set_code("ABC123");
        pending.set_secret(QC_SECRET);
        std::future::ready(Ok(()))
    }

    fn quick_connect_poll(
        self: Rc<Self>,
        _params: bootstrap::QuickConnectPollParams,
        results: &mut bootstrap::QuickConnectPollResults,
    ) -> impl Future<Output = Result<(), capnp::Error>> {
        results
            .get()
            .init_result()
            .init_ok()
            .set_device_token(DEVICE_TOKEN);
        std::future::ready(Ok(()))
    }
}

/// One client event whose field value is `FIELD_SECRET`.
fn fill_event(mut e: telemetry_event::Builder<'_>) {
    e.set_kind(EventKind::PlaybackError);
    e.set_level(Level::Error);
    let mut field = e.init_fields(1).get(0);
    field.set_key("authorization");
    field.set_value(FIELD_SECRET);
}

/// Calls every `$secret` method once at debug level, with known secrets in.
fn call_all() -> Observed {
    let ((), o) = observe("pharos_api=debug", || {
        let client: bootstrap::Client = server::bootstrap::client(Boot, &Connection::new());
        block_on(async move {
            let mut r = client.resume_request();
            fill_ctx(r.get().init_ctx());
            r.get().set_device_token(DEVICE_TOKEN);
            r.send().promise.await.unwrap();

            let mut r = client.login_request();
            fill_ctx(r.get().init_ctx());
            r.get().set_name("alice");
            r.get().set_password(PASSWORD);
            r.send().promise.await.unwrap();

            let mut r = client.quick_connect_initiate_request();
            fill_ctx(r.get().init_ctx());
            r.send().promise.await.unwrap();

            let mut r = client.quick_connect_poll_request();
            fill_ctx(r.get().init_ctx());
            r.get().set_secret(QC_SECRET);
            r.send().promise.await.unwrap();

            let mut r = client.submit_anonymous_request();
            fill_ctx(r.get().init_ctx());
            fill_event(r.get().init_events(1).get(0));
            r.send().promise.await.unwrap();

            let tel: telemetry::Client =
                server::telemetry::client(TelemetryBoot, &Connection::new());
            let mut r = tel.submit_request();
            fill_ctx(r.get().init_ctx());
            fill_event(r.get().init_events(1).get(0));
            r.get().init_spans(0);
            r.send().promise.await.unwrap();

            let grant: playback_grant::Client =
                server::playback_grant::client(GrantBoot, &Connection::new());
            let mut r = grant.decision_request();
            fill_ctx(r.get().init_ctx());
            r.send().promise.await.unwrap();

            let sess: session::Client = server::session::client(SessionBoot, &Connection::new());
            let mut r = sess.change_password_request();
            fill_ctx(r.get().init_ctx());
            r.get().set_current(PASSWORD);
            r.get().set_new(NEW_PASSWORD);
            r.send().promise.await.unwrap();
        });
    });
    o
}

/// Each method listed here is called by `call_all`. A `$secret` added to
/// another method fails this until the test calls it too.
const COVERED: [(&str, &str); 8] = [
    ("bootstrap", "resume"),
    ("bootstrap", "login"),
    ("bootstrap", "quick_connect_initiate"),
    ("bootstrap", "quick_connect_poll"),
    ("bootstrap", "submit_anonymous"),
    ("telemetry", "submit"),
    ("playback_grant", "decision"),
    ("session", "change_password"),
];

#[test]
fn every_secret_method_is_covered() {
    let generated: BTreeSet<_> = SECRET_METHODS.iter().copied().collect();
    let covered: BTreeSet<_> = COVERED.iter().copied().collect();
    assert_eq!(generated, covered);
}

#[test]
fn secrets_never_reach_logs_or_spans() {
    let o = call_all();
    for secret in [
        PASSWORD,
        NEW_PASSWORD,
        DEVICE_TOKEN,
        QC_SECRET,
        FIELD_SECRET,
    ] {
        assert!(
            !o.logs.contains(secret),
            "{secret} leaked into:\n{}",
            o.logs
        );
    }
    // Not vacuous: the payloads were logged, with the secrets masked, and
    // the non-secret values beside them are visible.
    let payloads: Vec<_> = o
        .lines()
        .into_iter()
        .filter(|l| l["fields"]["message"] == "rpc payload")
        .collect();
    assert_eq!(
        payloads.len(),
        16,
        "a request and a response per call:\n{}",
        o.logs
    );
    assert!(o.logs.contains(REDACTED));
    assert!(
        o.logs.contains("alice"),
        "non-secret param missing:\n{}",
        o.logs
    );
    assert!(
        o.logs.contains("ABC123"),
        "non-secret result missing:\n{}",
        o.logs
    );
}

fn fields_of(ty: capnp::introspect::Type) -> StructSchema {
    match ty.which() {
        TypeVariant::Struct(raw) => StructSchema::new(raw),
        other => panic!("not a struct: {other:?}"),
    }
}

#[test]
fn secret_annotation_is_read_from_the_schema() {
    let login = fields_of(<bootstrap::login_params::Owned as Introspect>::introspect());
    assert!(is_secret(login.get_field_by_name("password").unwrap()));
    assert!(!is_secret(login.get_field_by_name("name").unwrap()));
}

#[test]
fn both_passwords_of_a_password_change_are_secret() {
    let change = fields_of(<session::change_password_params::Owned as Introspect>::introspect());
    assert!(is_secret(change.get_field_by_name("current").unwrap()));
    assert!(is_secret(change.get_field_by_name("new").unwrap()));
    assert!(!is_secret(change.get_field_by_name("ctx").unwrap()));
}
