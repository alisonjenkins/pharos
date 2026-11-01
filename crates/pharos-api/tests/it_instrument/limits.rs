//! T026 (FR-010): every limit answer counts under
//! `pharos_rpc_limit_total{limit}`, named as the typed `Limit` names it.

use std::rc::Rc;

use pharos_api::server;
use pharos_api::Connection;

use crate::support::{block_on, observe};

/// A limit a handler answers (a per-user connection cap, say) counts too.
#[test]
fn a_limit_written_by_a_handler_is_counted_by_name() {
    let ((), o) = observe("info", || {
        let conn = Connection::new();
        let denial = Rc::new(pharos_api::Denial::Limit {
            name: "connections_per_user",
            value: 16,
            max: 16,
        });
        let session =
            server::session::client(server::session::Refused(Rc::clone(&conn), denial), &conn);
        block_on(async move {
            let mut me = session.me_request();
            crate::support::fill_ctx(me.get().init_ctx());
            me.send().promise.await.expect("me answers");
        });
    });
    assert_eq!(
        o.counter(
            "pharos_rpc_limit_total",
            &[("limit", "connections_per_user")]
        ),
        Some(1),
        "logs:\n{}",
        o.logs
    );
}

/// The `limit` label stays bounded: a name that is not a fixed snake_case
/// string counts as `other`. Disarm check: passing the name through makes
/// this read `None`.
#[test]
fn a_limit_name_outside_the_fixed_shape_counts_as_other() {
    let ((), o) = observe("info", || {
        let conn = Connection::new();
        let denial = Rc::new(pharos_api::Denial::Limit {
            name: "Not A Name 42",
            value: 1,
            max: 1,
        });
        let session =
            server::session::client(server::session::Refused(Rc::clone(&conn), denial), &conn);
        block_on(async move {
            let mut me = session.me_request();
            crate::support::fill_ctx(me.get().init_ctx());
            me.send().promise.await.expect("me answers");
        });
    });
    assert_eq!(
        o.counter("pharos_rpc_limit_total", &[("limit", "other")]),
        Some(1),
        "logs:\n{}",
        o.logs
    );
}
