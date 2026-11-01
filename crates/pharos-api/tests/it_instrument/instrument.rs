//! T010: a call through the generated wrapper records its span, RED series,
//! cancel counter and log line, on the failure path and the success path
//! alike. Disarm check: with `CallScope::finish` / `Drop` returning before they
//! record anything, every test here fails.

use std::cell::Cell;
use std::future::Future;
use std::pin::pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use pharos_api::pharos_capnp::{items_result, library};
use pharos_api::server;
use pharos_api::Connection;

use crate::support::{block_on, fill_ctx, observe, Observed, DEVICE, TRACE_ID};

pub(crate) enum Mode {
    NotFound,
    CapnpError,
    Ok,
    /// Marks the flag, then never completes.
    Hang(Rc<Cell<bool>>),
}

pub(crate) struct Lib(pub(crate) Mode);

impl server::library::Handler for Lib {
    // A handler can be a plain `async fn`; the trait's `impl Future` return
    // type accepts it.
    async fn latest(
        self: Rc<Self>,
        _params: library::LatestParams,
        results: &mut library::LatestResults,
    ) -> Result<(), capnp::Error> {
        match &self.0 {
            Mode::NotFound => {
                let mut err = results.get().init_result().init_error();
                err.set_reason("no item with that id");
                err.reborrow().get_kind().set_not_found("item-42");
                Ok(())
            }
            Mode::CapnpError => Err(capnp::Error::failed("store unreachable: db-3".into())),
            Mode::Ok => {
                results.get().init_result().init_page().set_total(3);
                Ok(())
            }
            Mode::Hang(started) => {
                started.set(true);
                std::future::pending::<()>().await;
                Ok(())
            }
        }
    }
}

pub(crate) fn latest_request(
    client: &library::Client,
) -> capnp::capability::RemotePromise<library::latest_results::Owned> {
    let mut req = client.latest_request();
    fill_ctx(req.get().init_ctx());
    req.get().init_parent_id().set_real(42);
    req.get().set_limit(5);
    req.send()
}

fn call(mode: Mode) -> (Result<u32, capnp::Error>, Observed) {
    observe("info", || {
        let client = server::library::client(Lib(mode), &Connection::new());
        block_on(async move {
            let resp = latest_request(&client).promise.await?;
            match resp.get()?.get_result()?.which()? {
                items_result::Page(p) => Ok(p?.get_total()),
                items_result::Error(_) => Ok(u32::MAX),
            }
        })
    })
}

const LATEST: [(&str, &str); 2] = [("interface", "library"), ("method", "latest")];

fn calls(o: &Observed, outcome: &str) -> Option<u64> {
    o.counter(
        "pharos_rpc_calls_total",
        &[
            ("interface", "library"),
            ("method", "latest"),
            ("outcome", outcome),
        ],
    )
}

/// The span and the log line carry the call's identity: name, call id, the
/// caller's trace id and device, and the outcome.
fn assert_identity(o: &Observed, outcome: &str) {
    let span = o
        .span_close("rpc.library.latest")
        .unwrap_or_else(|| panic!("no rpc.library.latest span in:\n{}", o.logs));
    let fields = &span["span"];
    assert_eq!(fields["interface"], "library");
    assert_eq!(fields["method"], "latest");
    assert_eq!(fields["outcome"], outcome);
    assert_eq!(fields["trace_id"], TRACE_ID);
    assert_eq!(fields["device"], DEVICE);
    assert!(
        fields["call_id"].as_str().is_some_and(|c| c.contains('.')),
        "call_id missing: {span}"
    );
}

#[test]
fn typed_error_records_outcome_span_and_a_warn_line_with_the_cause() {
    let (result, o) = call(Mode::NotFound);
    assert_eq!(
        result.unwrap(),
        u32::MAX,
        "handler's error arm reaches the caller"
    );

    assert_eq!(calls(&o, "not_found"), Some(1), "logs:\n{}", o.logs);
    assert_eq!(calls(&o, "ok"), None);
    assert_eq!(
        o.histogram_samples("pharos_rpc_duration_seconds", &LATEST),
        1
    );
    assert_identity(&o, "not_found");

    let line = o
        .event("rpc call failed")
        .unwrap_or_else(|| panic!("no failure line in:\n{}", o.logs));
    assert_eq!(line["level"], "WARN");
    assert_eq!(line["target"], "pharos_api::rpc::library");
    let f = &line["fields"];
    assert_eq!(f["outcome"], "not_found");
    assert_eq!(f["reason"], "no item with that id");
    assert!(
        f["detail"].as_str().is_some_and(|d| d.contains("item-42")),
        "failure line must carry the missing id: {line}"
    );
    assert_eq!(f["trace_id"], TRACE_ID);
    assert_eq!(f["device"], DEVICE);
    assert!(f["call_id"].as_str().is_some_and(|c| !c.is_empty()));
}

#[test]
fn capnp_error_is_counted_as_failed_with_its_message() {
    let (result, o) = call(Mode::CapnpError);
    assert!(result.is_err());

    assert_eq!(calls(&o, "failed"), Some(1), "logs:\n{}", o.logs);
    assert_identity(&o, "failed");
    let line = o.event("rpc call failed").expect("failure line");
    assert!(
        line["fields"]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("db-3")),
        "reason must carry the cause value: {line}"
    );
}

#[test]
fn success_records_the_same_fields_at_debug_and_no_failure_line() {
    let (result, o) = observe("debug", || {
        let client = server::library::client(Lib(Mode::Ok), &Connection::new());
        block_on(async move {
            let resp = latest_request(&client).promise.await?;
            match resp.get()?.get_result()?.which()? {
                items_result::Page(p) => Ok::<_, capnp::Error>(p?.get_total()),
                items_result::Error(_) => Ok(u32::MAX),
            }
        })
    });
    assert_eq!(result.unwrap(), 3);
    assert_eq!(calls(&o, "ok"), Some(1));
    assert_eq!(
        o.histogram_samples("pharos_rpc_duration_seconds", &LATEST),
        1
    );
    assert_identity(&o, "ok");
    assert!(o.event("rpc call failed").is_none());

    // Symmetry: the success line has every field the failure line has,
    // except the failure's own cause.
    let ok = o.event("rpc call ok").expect("success line at debug");
    let (_, failed) = call(Mode::NotFound);
    let failed = failed.event("rpc call failed").expect("failure line");
    for key in failed["fields"].as_object().expect("fields").keys() {
        if ["message", "reason", "detail"].contains(&key.as_str()) {
            continue;
        }
        assert!(
            ok["fields"].get(key).is_some(),
            "success line lacks `{key}` the failure line records: {ok}"
        );
    }
}

#[test]
fn dropping_the_call_counts_it_cancelled() {
    let started = Rc::new(Cell::new(false));
    let flag = Rc::clone(&started);
    let ((), o) = observe("info", move || {
        let client = server::library::client(Lib(Mode::Hang(flag)), &Connection::new());
        block_on(async {
            let mut promise = pin!(latest_request(&client).promise);
            let mut cx = Context::from_waker(Waker::noop());
            for _ in 0..16 {
                assert!(matches!(promise.as_mut().poll(&mut cx), Poll::Pending));
                tokio::task::yield_now().await;
            }
            // `promise` is dropped here, mid-call.
        });
        drop(client);
    });
    assert!(
        started.get(),
        "the handler never ran, so nothing was cancelled"
    );

    assert_eq!(
        o.counter("pharos_rpc_cancelled_total", &LATEST),
        Some(1),
        "logs:\n{}",
        o.logs
    );
    assert_eq!(calls(&o, "cancelled"), Some(1));
    assert_eq!(
        o.event("rpc call cancelled").expect("cancel line")["fields"]["outcome"],
        "cancelled"
    );
}

/// A client one schema version ahead calls a method the server has no
/// handler for: counted as `unimplemented`, so it shows up by query.
#[test]
fn a_method_without_a_handler_is_counted_unimplemented() {
    let (result, o) = observe("info", || {
        let client = server::library::client(Lib(Mode::Ok), &Connection::new());
        block_on(async move {
            let mut req = client.views_request();
            fill_ctx(req.get().init_ctx());
            req.send().promise.await.map(|_| ())
        })
    });
    assert!(result.is_err());
    assert_eq!(
        o.counter(
            "pharos_rpc_calls_total",
            &[
                ("interface", "library"),
                ("method", "views"),
                ("outcome", "unimplemented")
            ],
        ),
        Some(1),
        "logs:\n{}",
        o.logs
    );
}
