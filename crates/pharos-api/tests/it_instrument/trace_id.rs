//! 038 FR-404, V1216: every denial a client reads carries the call's trace
//! id, the same one the server's failure line records, so a viewer can quote
//! it and `{namespace="pharos"} | json | trace_id="<id>"` finds the line.
//! Disarm check: with `stamp_trace_id` returning at once, the four denial
//! tests fail (the success and handler-set cases pass by design).

use std::rc::Rc;

use pharos_api::pharos_capnp::{call_context, items_result, library, me_result};
use pharos_api::server;
use pharos_api::{Connection, Denial};

use crate::instrument::{Lib, Mode};
use crate::support::{block_on, fill_ctx, observe, DEVICE, TRACE_ID};

/// A context with everything but a trace: what the browser UI sends.
fn fill_untraced(mut ctx: call_context::Builder<'_>) {
    ctx.set_ui_route("/home");
    ctx.set_client_version("test/1");
    ctx.init_device().set_value(DEVICE);
}

/// `latest` answering `mode`, with or without the caller's trace. Returns the
/// trace id on the error the client read, `None` for a page.
fn latest(mode: Mode, traced: bool) -> (Option<String>, crate::support::Observed) {
    observe("info", || {
        let client = server::library::client(Lib(mode), &Connection::new());
        block_on(async move {
            let mut req = client.latest_request();
            if traced {
                fill_ctx(req.get().init_ctx());
            } else {
                fill_untraced(req.get().init_ctx());
            }
            req.get().init_parent_id().set_real(42);
            let reply = req.send().promise.await.expect("latest answers");
            match reply.get().unwrap().get_result().unwrap().which().unwrap() {
                items_result::Error(e) => Some(
                    e.unwrap()
                        .get_trace_id()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned(),
                ),
                items_result::Page(_) => None,
            }
        })
    })
}

fn logged_trace_id(o: &crate::support::Observed) -> String {
    let line = o
        .event("rpc call failed")
        .unwrap_or_else(|| panic!("no failure line in:\n{}", o.logs));
    line["fields"]["trace_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn a_denial_carries_the_callers_trace_id() {
    let (trace, o) = latest(Mode::NotFound, true);
    assert_eq!(trace.as_deref(), Some(TRACE_ID));
    assert_eq!(logged_trace_id(&o), TRACE_ID);
}

#[test]
fn an_untraced_call_is_given_one_trace_id_on_the_error_and_the_log_line() {
    let (trace, o) = latest(Mode::NotFound, false);
    let trace = trace.expect("an error");
    assert_eq!(trace.len(), 32, "a W3C trace id: {trace:?}");
    assert!(trace.bytes().all(|b| b.is_ascii_hexdigit()), "{trace:?}");
    assert_eq!(logged_trace_id(&o), trace, "logs:\n{}", o.logs);
    let span = o.span_close("rpc.library.latest").expect("span");
    assert_eq!(span["span"]["trace_id"], trace.as_str());
}

#[test]
fn two_untraced_calls_get_different_trace_ids() {
    let (a, _) = latest(Mode::NotFound, false);
    let (b, _) = latest(Mode::NotFound, false);
    assert_ne!(a, b);
}

#[test]
fn a_success_reply_is_left_alone() {
    let (trace, _) = latest(Mode::Ok, false);
    assert_eq!(trace, None);
}

#[test]
fn a_refused_session_denial_carries_the_trace_id() {
    let (trace, o) = observe("info", || {
        let conn = Rc::new(Connection::new());
        let denial = Rc::new(Denial::Unavailable {
            dependency: "token store",
        });
        let session =
            server::session::client(server::session::Refused(Rc::clone(&conn), denial), &conn);
        block_on(async move {
            let mut me = session.me_request();
            fill_untraced(me.get().init_ctx());
            let reply = me.send().promise.await.expect("me answers");
            let me_result::Error(err) = reply.get().unwrap().get_result().unwrap().which().unwrap()
            else {
                panic!("me must fail on a refused session");
            };
            err.unwrap()
                .get_trace_id()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
    });
    assert_eq!(trace.len(), 32, "{trace:?}");
    assert_eq!(logged_trace_id(&o), trace);
}

/// A handler that set its own trace id (none do today) keeps it.
struct OwnTrace;

impl server::library::Handler for OwnTrace {
    async fn latest(
        self: Rc<Self>,
        _params: library::LatestParams,
        results: &mut library::LatestResults,
    ) -> Result<(), capnp::Error> {
        let mut err = results.get().init_result().init_error();
        err.set_reason("x");
        err.set_trace_id("handler-set");
        err.reborrow().get_kind().set_other(());
        Ok(())
    }
}

#[test]
fn a_trace_id_the_handler_wrote_is_kept() {
    let (trace, _) = observe("info", || {
        let client = server::library::client(OwnTrace, &Connection::new());
        block_on(async move {
            let mut req = client.latest_request();
            fill_ctx(req.get().init_ctx());
            let reply = req.send().promise.await.expect("latest answers");
            match reply.get().unwrap().get_result().unwrap().which().unwrap() {
                items_result::Error(e) => e
                    .unwrap()
                    .get_trace_id()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned(),
                items_result::Page(_) => String::new(),
            }
        })
    });
    assert_eq!(trace, "handler-set");
}
