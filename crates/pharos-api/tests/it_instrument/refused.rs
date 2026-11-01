//! A refused or revoked session still hands out capabilities (the schema's
//! getters never fail, and `resume → library → home` must pipeline), so every
//! interface has a generated `Refused` handler: each call answers with its
//! `Denial` and is counted under that kind, never `unimplemented`.
//! Disarm check: with `fill_denied` doing nothing, every test here fails.

use std::rc::Rc;

use pharos_api::pharos_capnp::{error, items_result, me_result};
use pharos_api::server;
use pharos_api::{Connection, Denial};

use crate::support::{block_on, fill_ctx, observe};

#[test]
fn every_call_on_a_revoked_session_and_its_capabilities_fails_revoked() {
    let ((), o) = observe("info", || {
        let conn = Rc::new(Connection::new());
        let denial = Rc::new(Denial::Revoked {
            reason: "session revoked",
        });
        let session =
            server::session::client(server::session::Refused(Rc::clone(&conn), denial), &conn);
        block_on(async move {
            let mut me = session.me_request();
            fill_ctx(me.get().init_ctx());
            let reply = me.send().promise.await.expect("me answers");
            let me_result::Error(err) = reply.get().unwrap().get_result().unwrap().which().unwrap()
            else {
                panic!("me must fail on a revoked session");
            };
            assert!(matches!(
                err.unwrap().get_kind().which().unwrap(),
                error::kind::Revoked(())
            ));

            // Pipelined: the getter answers with a capability, whose calls fail.
            let mut get = session.library_request();
            fill_ctx(get.get().init_ctx());
            let library = get.send().pipeline.get_library();
            let mut latest = library.latest_request();
            fill_ctx(latest.get().init_ctx());
            let reply = latest.send().promise.await.expect("latest answers");
            let items_result::Error(err) =
                reply.get().unwrap().get_result().unwrap().which().unwrap()
            else {
                panic!("library.latest must fail on a revoked session");
            };
            assert!(matches!(
                err.unwrap().get_kind().which().unwrap(),
                error::kind::Revoked(())
            ));
        });
    });
    for (iface, method) in [("session", "me"), ("library", "latest")] {
        assert_eq!(
            o.counter(
                "pharos_rpc_calls_total",
                &[
                    ("interface", iface),
                    ("method", method),
                    ("outcome", "revoked")
                ],
            ),
            Some(1),
            "{iface}.{method} not counted revoked; logs:\n{}",
            o.logs
        );
    }
    assert_eq!(
        o.counter(
            "pharos_rpc_calls_total",
            &[
                ("interface", "session"),
                ("method", "library"),
                ("outcome", "ok")
            ],
        ),
        Some(1),
        "a capability getter succeeds even when revoked"
    );
}

/// B216: a session refused because the token store was down says
/// `unavailable` on every call, so the client retries instead of discarding
/// its token. The reason names the dependency, never the backend's error.
#[test]
fn a_session_refused_for_an_outage_fails_unavailable_not_revoked() {
    let ((), o) = observe("info", || {
        let conn = Rc::new(Connection::new());
        let denial = Rc::new(Denial::Unavailable {
            dependency: "token store",
        });
        let session =
            server::session::client(server::session::Refused(Rc::clone(&conn), denial), &conn);
        block_on(async move {
            let mut get = session.library_request();
            fill_ctx(get.get().init_ctx());
            let library = get.send().pipeline.get_library();
            let mut latest = library.latest_request();
            fill_ctx(latest.get().init_ctx());
            let reply = latest.send().promise.await.expect("latest answers");
            let items_result::Error(err) =
                reply.get().unwrap().get_result().unwrap().which().unwrap()
            else {
                panic!("library.latest must fail on a refused session");
            };
            let err = err.unwrap();
            assert_eq!(err.get_reason().unwrap(), "token store unavailable");
            assert!(matches!(
                err.get_kind().which().unwrap(),
                error::kind::Unavailable(Ok(d)) if d == "token store"
            ));
        });
    });
    assert_eq!(
        o.counter(
            "pharos_rpc_calls_total",
            &[
                ("interface", "library"),
                ("method", "latest"),
                ("outcome", "unavailable")
            ],
        ),
        Some(1),
        "logs:\n{}",
        o.logs
    );
}
