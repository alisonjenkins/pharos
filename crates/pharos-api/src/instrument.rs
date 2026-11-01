//! Runtime half of the generated `Instrumented<S>` wrappers (017 research R4,
//! V173). `build/gen.rs` emits one wrapper per interface. Each method opens its
//! `rpc.<interface>.<method>` span and hands the call to [`CallScope`], which
//! records everything §Observability lists for a call: the span fields, the
//! RED series, the cancel counter, and a log line for the failure path with the
//! same fields as the success path.
//!
//! capnpc 0.27's generated `Server` methods look like this:
//! `fn m(self: Rc<Self>, MParams, MResults) -> impl Future<Output = Result<(), capnp::Error>> + 'static`.
//! They take `Results` by value, so a wrapper that passed `Results` on could not
//! read the outcome afterwards. The generated `Handler` traits take
//! `&mut MResults` instead, and the wrapper reads the `result` union back
//! through capnp's dynamic reflection once the handler returns.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use capnp::dynamic_value;
use tracing::{Instrument, Span};
use web_time::Instant;

use crate::labels::{MethodMeta, Outcome, CALLS_TOTAL, CANCELLED_TOTAL, DURATION_SECONDS};
use crate::redact;

static NEXT_CONNECTION: AtomicU64 = AtomicU64::new(1);
static TRACE_PARENT_HOOK: OnceLock<fn(&Span, &str)> = OnceLock::new();

/// Installs the function that parents each call span on the caller's W3C
/// `traceparent` (FR-006). The server installs one backed by
/// tracing-opentelemetry. Keeping it a hook keeps OpenTelemetry out of the
/// wasm build. Only the first installation takes effect.
pub fn set_trace_parent_hook(hook: fn(&Span, &str)) {
    let _ = TRACE_PARENT_HOOK.set(hook);
}

/// One RPC connection. Every capability served on it shares this, so each
/// `call_id` reads `<connection>.<call>`. capnp-rpc does not expose the
/// question id to a `Server`, so the call number is this connection's own
/// sequence. Single-threaded by construction, like the `RpcSystem` it serves
/// (research R2).
#[derive(Debug)]
pub struct Connection {
    id: u64,
    next_call: Cell<u64>,
    user: RefCell<String>,
}

impl Connection {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            id: NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed),
            next_call: Cell::new(1),
            user: RefCell::new(String::new()),
        })
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// Records the authenticated user on every later call's span and log.
    pub fn set_user(&self, user: &str) {
        user.clone_into(&mut self.user.borrow_mut());
    }

    fn next_call_id(&self) -> String {
        let n = self.next_call.get();
        self.next_call.set(n.wrapping_add(1));
        format!("{}.{n}", self.id)
    }
}

/// The fields every log line about a call carries, on success and failure
/// alike.
#[derive(Debug, Default)]
pub struct CallRecord {
    pub interface: &'static str,
    pub method: &'static str,
    pub call_id: String,
    pub trace_id: String,
    pub device: String,
    pub client_version: String,
    pub user: String,
    pub outcome: &'static str,
    pub duration_ms: f64,
    pub request_bytes: u64,
    pub response_bytes: u64,
}

/// Log callsites for one interface, generated with a literal target
/// (`pharos_api::rpc::<interface>`) so log levels can be set per interface.
pub struct InterfaceLog {
    pub failure: fn(&CallRecord, &str, &str),
    pub success: fn(&CallRecord),
    pub cancelled: fn(&CallRecord),
    pub debug_enabled: fn() -> bool,
    pub payload: fn(&CallRecord, &'static str, &str),
}

/// One in-flight call. Dropping it before [`CallScope::finish`] counts the
/// call as cancelled (the caller dropped the promise or disconnected).
pub struct CallScope {
    meta: &'static MethodMeta,
    log: &'static InterfaceLog,
    span: Span,
    record: CallRecord,
    started: Instant,
    finished: bool,
}

impl CallScope {
    pub fn begin(
        conn: &Connection,
        meta: &'static MethodMeta,
        log: &'static InterfaceLog,
        span: Span,
        params: capnp::Result<dynamic_value::Reader<'_>>,
    ) -> Self {
        let mut record = CallRecord {
            interface: meta.interface,
            method: meta.method,
            call_id: conn.next_call_id(),
            user: conn.user.borrow().clone(),
            ..CallRecord::default()
        };
        span.record("call_id", record.call_id.as_str());
        if !record.user.is_empty() {
            span.record("user", record.user.as_str());
        }
        match &params {
            Ok(dynamic_value::Reader::Struct(p)) => {
                record.request_bytes = p.total_size().map_or(0, |s| s.word_count * 8);
                if let Some(ctx) = struct_field(*p, "ctx") {
                    let traceparent = text_field(ctx, "traceparent");
                    record.trace_id = trace_id_of(&traceparent).to_owned();
                    record.client_version = text_field(ctx, "clientVersion");
                    record.device = struct_field(ctx, "device")
                        .map(|d| text_field(d, "value"))
                        .unwrap_or_default();
                    span.record("ui_route", text_field(ctx, "uiRoute").as_str());
                    span.record("ui_action", text_field(ctx, "uiAction").as_str());
                    if !traceparent.is_empty() {
                        if let Some(hook) = TRACE_PARENT_HOOK.get() {
                            hook(&span, &traceparent);
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                // An undecodable request still runs through the handler, which
                // sees the same error; the debug line says why.
                if (log.debug_enabled)() {
                    (log.payload)(&record, "request", &format!("<undecodable: {e}>"));
                }
            }
        }
        span.record("trace_id", record.trace_id.as_str());
        span.record("device", record.device.as_str());
        span.record("client_version", record.client_version.as_str());
        span.record("request_bytes", record.request_bytes);
        if let Ok(p) = params {
            if (log.debug_enabled)() {
                (log.payload)(&record, "request", &redact::render(p));
            }
        }
        Self {
            meta,
            log,
            span,
            record,
            started: Instant::now(),
            finished: false,
        }
    }

    /// Runs the handler's future inside the call span.
    pub fn run<F: Future>(&self, fut: F) -> tracing::instrument::Instrumented<F> {
        fut.instrument(self.span.clone())
    }

    /// Records the call's outcome from the handler's return value and, when
    /// that is `Ok`, the `result` union the handler wrote.
    pub fn finish(
        mut self,
        returned: Result<(), capnp::Error>,
        reply: dynamic_value::Reader<'_>,
    ) -> Result<(), capnp::Error> {
        self.finished = true;
        let (outcome, reason, detail) = match &returned {
            Err(e) => (Outcome::from_capnp(e), e.to_string(), String::new()),
            Ok(()) => outcome_of(reply),
        };
        if let dynamic_value::Reader::Struct(r) = reply {
            self.record.response_bytes = r.total_size().map_or(0, |s| s.word_count * 8);
        }
        self.count(outcome);
        if outcome == Outcome::Ok {
            (self.log.success)(&self.record);
        } else {
            (self.log.failure)(&self.record, &reason, &detail);
        }
        if returned.is_ok() && (self.log.debug_enabled)() {
            (self.log.payload)(&self.record, "response", &redact::render(reply));
        }
        returned
    }

    fn count(&mut self, outcome: Outcome) {
        let elapsed = self.started.elapsed();
        self.record.outcome = outcome.label();
        self.record.duration_ms = elapsed.as_secs_f64() * 1000.0;
        self.span.record("outcome", outcome.label());
        self.span
            .record("response_bytes", self.record.response_bytes);
        let (interface, method) = (self.meta.interface, self.meta.method);
        metrics::counter!(CALLS_TOTAL, "interface" => interface, "method" => method, "outcome" => outcome.label())
            .increment(1);
        metrics::histogram!(DURATION_SECONDS, "interface" => interface, "method" => method)
            .record(elapsed.as_secs_f64());
        if outcome == Outcome::Cancelled {
            metrics::counter!(CANCELLED_TOTAL, "interface" => interface, "method" => method)
                .increment(1);
        }
    }
}

impl Drop for CallScope {
    fn drop(&mut self) {
        if !self.finished {
            self.count(Outcome::Cancelled);
            let _entered = self.span.enter();
            (self.log.cancelled)(&self.record);
        }
    }
}

/// Every `(interface, method, outcome)` RED series and every cancel series at
/// zero, so `rate()` works from the first scrape (017 §Observability).
pub fn register_metrics() {
    for iface in crate::server::INTERFACES {
        for m in iface.methods {
            for outcome in Outcome::ALL {
                metrics::counter!(CALLS_TOTAL, "interface" => m.interface, "method" => m.method, "outcome" => outcome.label())
                    .absolute(0);
            }
            metrics::counter!(CANCELLED_TOTAL, "interface" => m.interface, "method" => m.method)
                .absolute(0);
        }
    }
}

/// Reads the outcome a handler wrote: the `error` arm of the `result` union,
/// or, for a result that cannot be a union because it carries a pipelined
/// capability (`ResumeResult`), the `error` arm of its `status`. Returns the
/// outcome, the error's `reason`, and the variant's value (the id not found,
/// the dependency unavailable, …) so the failure log carries the cause, not
/// just its class.
fn outcome_of(reply: dynamic_value::Reader<'_>) -> (Outcome, String, String) {
    let ok = (Outcome::Ok, String::new(), String::new());
    let dynamic_value::Reader::Struct(reply) = reply else {
        return ok;
    };
    let Some(result) = struct_field(reply, "result") else {
        return ok;
    };
    let carrier = if active_is(result, "error") {
        result
    } else {
        match struct_field(result, "status") {
            Some(status) if active_is(status, "error") => status,
            _ => return ok,
        }
    };
    let Some(error) = struct_field(carrier, "error") else {
        return (
            Outcome::Other,
            "error arm unreadable".to_owned(),
            String::new(),
        );
    };
    let reason = text_field(error, "reason");
    let Some(kind) = struct_field(error, "kind") else {
        return (Outcome::Other, reason, String::new());
    };
    let Ok(Some(variant)) = kind.which() else {
        return (Outcome::Other, reason, String::new());
    };
    let name = variant
        .get_proto()
        .get_name()
        .ok()
        .and_then(|n| n.to_str().ok())
        .unwrap_or_default();
    let detail = kind.get(variant).map(redact::render).unwrap_or_default();
    let outcome = Outcome::from_error_kind(name).unwrap_or(Outcome::Other);
    (outcome, reason, detail)
}

/// Whether `s`'s active union member is named `name`.
fn active_is(s: capnp::dynamic_struct::Reader<'_>, name: &str) -> bool {
    matches!(s.which(), Ok(Some(f)) if f.get_proto().get_name().ok().and_then(|n| n.to_str().ok()) == Some(name))
}

fn struct_field<'a>(
    s: capnp::dynamic_struct::Reader<'a>,
    name: &str,
) -> Option<capnp::dynamic_struct::Reader<'a>> {
    let field = s.get_schema().find_field_by_name(name).ok()??;
    match s.get(field).ok()? {
        dynamic_value::Reader::Struct(v) => Some(v),
        _ => None,
    }
}

fn text_field(s: capnp::dynamic_struct::Reader<'_>, name: &str) -> String {
    let Ok(Some(field)) = s.get_schema().find_field_by_name(name) else {
        return String::new();
    };
    match s.get(field) {
        Ok(dynamic_value::Reader::Text(t)) => t.to_string().unwrap_or_default(),
        _ => String::new(),
    }
}

/// The trace id of a W3C `traceparent` (`00-<32 hex>-<16 hex>-<2 hex>`), or ""
/// when it is not well-formed.
fn trace_id_of(traceparent: &str) -> &str {
    let mut parts = traceparent.split('-');
    match (parts.next(), parts.next()) {
        (Some(_), Some(id)) if id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) => id,
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::trace_id_of;

    #[test]
    fn trace_id_is_the_second_traceparent_field() {
        assert_eq!(
            trace_id_of("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
        assert_eq!(trace_id_of("garbage"), "");
        assert_eq!(trace_id_of(""), "");
    }
}
