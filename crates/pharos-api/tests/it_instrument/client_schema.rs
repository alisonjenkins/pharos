//! T027: `pharos_rpc_client_schema_total{version}` counts each connection
//! once, by the `CallContext.schemaVersion` of its first call that carries
//! one (every method takes a `ctx`), so a UI ahead of or behind the server
//! shows up by query. The label is bucketed: the value is the client's to
//! choose. Disarm check: without the count in
//! `CallScope::begin`, the counter test reads `None`.

use pharos_api::labels::client_schema_label;
use pharos_api::pharos_capnp::SCHEMA_VERSION;
use pharos_api::server;
use pharos_api::Connection;

use crate::instrument::{Lib, Mode};
use crate::support::{block_on, fill_ctx, observe};

#[test]
fn the_label_is_unset_a_known_version_or_newer() {
    assert_eq!(client_schema_label(0), "unset");
    assert_eq!(client_schema_label(1), "1");
    assert_eq!(
        client_schema_label(SCHEMA_VERSION),
        SCHEMA_VERSION.to_string()
    );
    assert_eq!(client_schema_label(SCHEMA_VERSION + 1), "newer");
    assert_eq!(client_schema_label(u32::MAX), "newer");
}

fn calls_on_one_connection(schema: u32, calls: usize) -> crate::support::Observed {
    let ((), o) = observe("info", || {
        let client = server::library::client(Lib(Mode::Ok), &Connection::new());
        block_on(async move {
            for _ in 0..calls {
                let mut req = client.latest_request();
                let mut ctx = req.get().init_ctx();
                fill_ctx(ctx.reborrow());
                ctx.set_schema_version(schema);
                req.send().promise.await.expect("latest answers");
            }
        });
    });
    o
}

#[test]
fn a_connection_counts_once_by_the_schema_it_was_built_against() {
    let o = calls_on_one_connection(SCHEMA_VERSION, 3);
    assert_eq!(
        o.counter(
            "pharos_rpc_client_schema_total",
            &[("version", &SCHEMA_VERSION.to_string())]
        ),
        Some(1),
        "logs:\n{}",
        o.logs
    );

    let o = calls_on_one_connection(u32::MAX, 1);
    assert_eq!(
        o.counter("pharos_rpc_client_schema_total", &[("version", "newer")]),
        Some(1)
    );
}
