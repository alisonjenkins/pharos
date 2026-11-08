//! B730 / V680: a server that drops its `RpcSystem` while calls are running
//! must drop those calls. capnp-rpc alone leaks them (a cycle through the
//! answer table), and a leaked call parked on the database pool keeps its slot
//! for good: eight leaked calls wedged the sqlite pool with every connection
//! idle. `pharos_rpc_calls_in_flight` is the signal that shows a leak.

use std::cell::Cell;
use std::rc::Rc;

use capnp_rpc::rpc_twoparty_capnp::Side;
use capnp_rpc::{twoparty, RpcSystem};
use pharos_api::pharos_capnp::library;
use pharos_api::{server, Connection, DisconnectOnDrop};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::instrument::latest_request;
use crate::support::{observe, Observed};

const LATEST: &[(&str, &str)] = &[("interface", "library"), ("method", "latest")];
const IN_FLIGHT: &str = "pharos_rpc_calls_in_flight";

/// Sets its flag when the handler's future is dropped.
struct OnDrop(Rc<Cell<bool>>);

impl Drop for OnDrop {
    fn drop(&mut self) {
        self.0.set(true);
    }
}

/// A `latest` that never answers, like a call parked on a saturated pool.
struct Parked {
    started: Rc<Cell<bool>>,
    dropped: Rc<Cell<bool>>,
}

impl server::library::Handler for Parked {
    async fn latest(
        self: Rc<Self>,
        _params: library::LatestParams,
        _results: &mut library::LatestResults,
    ) -> Result<(), capnp::Error> {
        let _dropped = OnDrop(Rc::clone(&self.dropped));
        self.started.set(true);
        std::future::pending::<()>().await;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum End {
    /// The client drops the call: capnp-rpc sends `Finish`.
    CallerGivesUp,
    /// The server drops a bare `RpcSystem` mid-call.
    DropBareSystem,
    /// The server drops a [`DisconnectOnDrop`] mid-call.
    DropGuardedSystem,
}

struct Seen {
    /// Whether the handler's future was dropped by the end.
    dropped: bool,
    o: Observed,
}

fn park_then(end: End) -> Seen {
    let started = Rc::new(Cell::new(false));
    let dropped = Rc::new(Cell::new(false));
    let handler = Parked {
        started: Rc::clone(&started),
        dropped: Rc::clone(&dropped),
    };
    let ((), o) = observe("info", move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let (sr, sw) = tokio::io::split(server_io);
            let net = twoparty::VatNetwork::new(
                sr.compat(),
                sw.compat_write(),
                Side::Server,
                Default::default(),
            );
            let cap = server::library::client(handler, &Connection::new());
            let system = RpcSystem::new(Box::new(net), Some(cap.client));
            let server_task = match end {
                End::DropGuardedSystem => tokio::task::spawn_local(DisconnectOnDrop::new(system)),
                End::CallerGivesUp | End::DropBareSystem => tokio::task::spawn_local(system),
            };

            let (cr, cw) = tokio::io::split(client_io);
            let client_net = twoparty::VatNetwork::new(
                cr.compat(),
                cw.compat_write(),
                Side::Client,
                Default::default(),
            );
            let mut client_rpc = RpcSystem::new(Box::new(client_net), None);
            let lib: library::Client = client_rpc.bootstrap(Side::Server);
            let _client_task = tokio::task::spawn_local(client_rpc);
            let call = tokio::task::spawn_local(latest_request(&lib).promise);
            for _ in 0..200 {
                if started.get() {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert!(started.get(), "the handler never ran");

            match end {
                End::CallerGivesUp => call.abort(),
                // The server goes first, before it has read anything from
                // the client: the shape of a socket the server closes itself.
                End::DropBareSystem | End::DropGuardedSystem => server_task.abort(),
            }
            for _ in 0..200 {
                tokio::task::yield_now().await;
            }
        });
    });
    Seen {
        dropped: dropped.get(),
        o,
    }
}

#[test]
fn a_parked_call_counts_in_flight_until_its_caller_gives_up() {
    let seen = park_then(End::CallerGivesUp);
    assert!(seen.dropped);
    assert_eq!(seen.o.gauge(IN_FLIGHT, LATEST), Some(0.0));
}

#[test]
fn a_bare_rpc_system_dropped_mid_call_leaks_the_call() {
    // Pins the capnp-rpc behaviour `DisconnectOnDrop` exists for. If this
    // starts failing, capnp-rpc breaks the cycle itself and the guard can go.
    let seen = park_then(End::DropBareSystem);
    assert!(!seen.dropped, "capnp-rpc now drops the call itself");
    assert_eq!(seen.o.gauge(IN_FLIGHT, LATEST), Some(1.0));
}

#[test]
fn dropping_a_guarded_rpc_system_drops_its_calls() {
    let seen = park_then(End::DropGuardedSystem);
    assert!(seen.dropped, "the parked call outlived its connection");
    assert_eq!(
        seen.o.gauge(IN_FLIGHT, LATEST),
        Some(0.0),
        "logs:\n{}",
        seen.o.logs
    );
    assert_eq!(
        seen.o.counter("pharos_rpc_cancelled_total", LATEST),
        Some(1),
        "the released call is counted cancelled; logs:\n{}",
        seen.o.logs
    );
}
