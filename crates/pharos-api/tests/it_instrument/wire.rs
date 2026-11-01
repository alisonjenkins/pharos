//! T010, over the wire: a remote caller that drops its call, or its whole
//! connection, mid-call is counted `cancelled` on the server, like HTTP's
//! `http_client_aborted_total`. The local-capability case in `instrument.rs`
//! cannot show this; only a real two-party connection sends `Finish` or
//! disconnects.

use std::cell::Cell;
use std::rc::Rc;

use capnp_rpc::rpc_twoparty_capnp::Side;
use capnp_rpc::{twoparty, RpcSystem};
use pharos_api::pharos_capnp::library;
use pharos_api::server;
use pharos_api::Connection;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::instrument::{latest_request, Lib, Mode};
use crate::support::{observe, Observed};

enum Abort {
    /// The caller drops the promise: capnp-rpc sends `Finish`.
    Call,
    /// The caller's whole connection goes away.
    Connection,
}

fn over_the_wire(abort: Abort) -> (bool, Observed) {
    let started = Rc::new(Cell::new(false));
    let flag = Rc::clone(&started);
    let seen = Rc::clone(&started);
    let ((), o) = observe("info", move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let (sr, sw) = tokio::io::split(server_io);
            let server_net = twoparty::VatNetwork::new(
                sr.compat(),
                sw.compat_write(),
                Side::Server,
                Default::default(),
            );
            let cap = server::library::client(Lib(Mode::Hang(flag)), &Connection::new());
            let server_rpc = RpcSystem::new(Box::new(server_net), Some(cap.client));
            let server_task = tokio::task::spawn_local(server_rpc);

            let (cr, cw) = tokio::io::split(client_io);
            let client_net = twoparty::VatNetwork::new(
                cr.compat(),
                cw.compat_write(),
                Side::Client,
                Default::default(),
            );
            let mut client_rpc = RpcSystem::new(Box::new(client_net), None);
            let lib: library::Client = client_rpc.bootstrap(Side::Server);
            let client_task = tokio::task::spawn_local(client_rpc);

            let call = tokio::task::spawn_local(latest_request(&lib).promise);
            for _ in 0..200 {
                if seen.get() {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert!(seen.get(), "the handler never ran");

            match abort {
                Abort::Call => call.abort(),
                Abort::Connection => {
                    client_task.abort();
                    call.abort();
                }
            }
            drop(lib);
            // Let the server read the Finish / EOF and drop the call.
            for _ in 0..200 {
                tokio::task::yield_now().await;
            }
            server_task.abort();
        });
    });
    (started.get(), o)
}

fn assert_cancelled(o: &Observed) {
    assert_eq!(
        o.counter(
            "pharos_rpc_cancelled_total",
            &[("interface", "library"), ("method", "latest")]
        ),
        Some(1),
        "logs:\n{}",
        o.logs
    );
}

#[test]
fn a_remote_caller_dropping_its_call_is_counted_cancelled() {
    let (started, o) = over_the_wire(Abort::Call);
    assert!(started);
    assert_cancelled(&o);
}

#[test]
fn a_remote_caller_disconnecting_is_counted_cancelled() {
    let (started, o) = over_the_wire(Abort::Connection);
    assert!(started);
    assert_cancelled(&o);
}
