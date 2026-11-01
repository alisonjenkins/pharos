//! T026 (FR-010): calls in flight per connection are capped. The call past
//! the cap answers `limit` naming `calls_in_flight`, its value and the max,
//! and every limit answer counts under `pharos_rpc_limit_total{limit}`.
//! Disarm check: with `CallScope::begin` never setting a refusal, the
//! overflow call hangs like the others and the test times out.

use std::cell::Cell;
use std::future::Future;
use std::pin::{pin, Pin};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use pharos_api::instrument::MAX_CALLS_IN_FLIGHT;
use pharos_api::pharos_capnp::{error, items_result};
use pharos_api::server;
use pharos_api::Connection;

use crate::instrument::{latest_request, Lib, Mode};
use crate::support::{block_on, observe};

fn poll_pending<F: Future + Unpin>(f: &mut F, cx: &mut Context<'_>) -> bool {
    matches!(Pin::new(f).poll(cx), Poll::Pending)
}

#[test]
fn the_call_past_the_cap_answers_limit_and_the_cap_frees_as_calls_end() {
    let (limit, o) = observe("info", || {
        let conn = Connection::new();
        let client = server::library::client(Lib(Mode::Hang(Rc::new(Cell::new(false)))), &conn);
        block_on(async {
            let mut cx = Context::from_waker(Waker::noop());
            let mut hanging: Vec<_> = (0..MAX_CALLS_IN_FLIGHT)
                .map(|_| Box::pin(latest_request(&client).promise))
                .collect();
            for _ in 0..4 {
                for p in &mut hanging {
                    assert!(poll_pending(p, &mut cx));
                }
                tokio::task::yield_now().await;
            }

            let reply = latest_request(&client).promise.await.expect("answers");
            let items_result::Error(err) =
                reply.get().unwrap().get_result().unwrap().which().unwrap()
            else {
                panic!("the call past the cap must be refused");
            };
            let error::kind::Limit(limit) = err.unwrap().get_kind().which().unwrap() else {
                panic!("refused as a limit");
            };
            let limit = limit.unwrap();
            let seen = (
                limit.get_name().unwrap().to_string().unwrap(),
                limit.get_value(),
                limit.get_max(),
            );

            // Ending the hanging calls frees their places: the next call runs.
            drop(hanging);
            tokio::task::yield_now().await;
            let mut next = pin!(latest_request(&client).promise);
            for _ in 0..4 {
                assert!(
                    poll_pending(&mut next, &mut cx),
                    "once the calls ended, a new one runs (and hangs)"
                );
                tokio::task::yield_now().await;
            }
            seen
        })
    });
    let max = u64::from(MAX_CALLS_IN_FLIGHT);
    assert_eq!(limit, ("calls_in_flight".to_owned(), max, max));
    assert_eq!(
        o.counter("pharos_rpc_limit_total", &[("limit", "calls_in_flight")]),
        Some(1),
        "logs:\n{}",
        o.logs
    );
    assert_eq!(
        o.counter(
            "pharos_rpc_calls_total",
            &[
                ("interface", "library"),
                ("method", "latest"),
                ("outcome", "limit")
            ],
        ),
        Some(1)
    );
    assert!(
        o.logs
            .contains(&format!("calls_in_flight is {max}, at most {max}")),
        "the log names the value; logs:\n{}",
        o.logs
    );
}

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
