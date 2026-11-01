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

use crate::labels::{
    client_schema_label, MethodMeta, Outcome, CALLS_TOTAL, CANCELLED_TOTAL, CLIENT_SCHEMA_TOTAL,
    DURATION_SECONDS, LIMIT_TOTAL,
};
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
    in_flight: Rc<Cell<u32>>,
    /// Whether `pharos_rpc_client_schema_total` has counted this connection.
    schema_counted: Cell<bool>,
}

/// Calls one connection may have running at once (FR-010). The UI's busiest
/// screen issues about a dozen; past this a call answers `limit` at once.
pub const MAX_CALLS_IN_FLIGHT: u32 = 32;

impl Connection {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            id: NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed),
            next_call: Cell::new(1),
            user: RefCell::new(String::new()),
            in_flight: Rc::new(Cell::new(0)),
            schema_counted: Cell::new(false),
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
    in_flight: Rc<Cell<u32>>,
    refusal: Option<Denial>,
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
                    if !conn.schema_counted.replace(true) {
                        let version = client_schema_label(u32_field(ctx, "schemaVersion"));
                        metrics::counter!(CLIENT_SCHEMA_TOTAL, "version" => version).increment(1);
                    }
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
        let in_flight = Rc::clone(&conn.in_flight);
        let running = in_flight.get();
        in_flight.set(running.saturating_add(1));
        let refusal = (running >= MAX_CALLS_IN_FLIGHT).then_some(Denial::Limit {
            name: "calls_in_flight",
            value: u64::from(running),
            max: u64::from(MAX_CALLS_IN_FLIGHT),
        });
        Self {
            meta,
            log,
            span,
            record,
            started: Instant::now(),
            finished: false,
            in_flight,
            refusal,
        }
    }

    /// Set when the connection already had [`MAX_CALLS_IN_FLIGHT`] calls
    /// running: the generated wrapper answers with it instead of calling the
    /// handler.
    pub fn refusal(&self) -> Option<&Denial> {
        self.refusal.as_ref()
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
        let (outcome, reason, detail, limit) = match &returned {
            Err(e) => (Outcome::from_capnp(e), e.to_string(), String::new(), None),
            Ok(()) => outcome_of(reply),
        };
        if let Some(limit) = limit {
            metrics::counter!(LIMIT_TOTAL, "limit" => limit).increment(1);
        }
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
        self.in_flight.set(self.in_flight.get().saturating_sub(1));
        if self.finished {
            return;
        }
        // Unwinding out of the handler is a server fault, not a caller giving
        // up: count and log it as one.
        let panicked = std::thread::panicking();
        self.count(if panicked {
            Outcome::Failed
        } else {
            Outcome::Cancelled
        });
        let _entered = self.span.enter();
        if panicked {
            (self.log.failure)(&self.record, "handler panicked", "");
        } else {
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
/// just its class. A `limit` also yields its name, for `LIMIT_TOTAL`.
fn outcome_of(reply: dynamic_value::Reader<'_>) -> (Outcome, String, String, Option<String>) {
    let ok = (Outcome::Ok, String::new(), String::new(), None);
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
            None,
        );
    };
    let reason = text_field(error, "reason");
    let Some(kind) = struct_field(error, "kind") else {
        return (Outcome::Other, reason, String::new(), None);
    };
    let Ok(Some(variant)) = kind.which() else {
        return (Outcome::Other, reason, String::new(), None);
    };
    let name = redact::name_of(variant.get_proto().get_name());
    let detail = kind.get(variant).map(redact::render).unwrap_or_default();
    let outcome = Outcome::from_error_kind(name).unwrap_or(Outcome::Other);
    let limit = match kind.get(variant) {
        Ok(dynamic_value::Reader::Struct(l)) if outcome == Outcome::Limit => {
            Some(limit_label(text_field(l, "name")))
        }
        _ => None,
    };
    (outcome, reason, detail, limit)
}

/// A limit's name as the `limit` label. Names are fixed snake_case strings;
/// anything else is `other`, so a handler bug cannot grow the label set.
fn limit_label(name: String) -> String {
    let fixed = !name.is_empty()
        && name.len() <= 40
        && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
    if fixed {
        name
    } else {
        "other".to_owned()
    }
}

/// Why a refused session fails every call. The generated `Refused` handlers
/// carry one, so a call on a session refused for a store outage says
/// `unavailable`, not `revoked` (B216).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Denial {
    /// The credential is missing, invalid or revoked: sign in again.
    Revoked { reason: &'static str },
    /// A dependency is down. Transient: the client keeps its token and
    /// resumes again.
    Unavailable { dependency: &'static str },
    /// The caller may not do this; `what` names it, a fixed string.
    NotPermitted { what: &'static str },
    /// `what` (a fixed name, never the caller's input) does not exist, or
    /// no longer does.
    NotFound { what: &'static str },
    /// Too many failures; try again in `retry_after_secs`. Written as a
    /// `limit` named `name` whose `value` is the wait.
    Throttled {
        name: &'static str,
        retry_after_secs: u64,
    },
    /// A cap was reached; resume again once the user holds fewer.
    Limit {
        name: &'static str,
        value: u64,
        max: u64,
    },
}

impl Denial {
    /// Writes this denial into `error`. The reason is a fixed string, never a
    /// backend's error text: that stays in the server's own log.
    pub fn write(&self, mut error: crate::pharos_capnp::error::Builder<'_>) {
        match *self {
            Denial::Revoked { reason } => {
                error.set_reason(reason);
                error.get_kind().set_revoked(());
            }
            Denial::Unavailable { dependency } => {
                error.set_reason(format!("{dependency} unavailable").as_str());
                error.get_kind().set_unavailable(dependency);
            }
            Denial::NotPermitted { what } => {
                error.set_reason(what);
                error.get_kind().set_not_permitted(what);
            }
            Denial::NotFound { what } => {
                error.set_reason(format!("{what} not found").as_str());
                error.get_kind().set_not_found(what);
            }
            Denial::Throttled {
                name,
                retry_after_secs,
            } => {
                error.set_reason(
                    format!("too many failed attempts; retry in {retry_after_secs} s").as_str(),
                );
                let mut limit = error.get_kind().init_limit();
                limit.set_name(name);
                limit.set_value(retry_after_secs);
            }
            Denial::Limit { name, value, max } => {
                error.set_reason(format!("{name} is {value}, at most {max}").as_str());
                let mut limit = error.get_kind().init_limit();
                limit.set_name(name);
                limit.set_value(value);
                limit.set_max(max);
            }
        }
    }
}

/// Fills a method's results as a refused session answers: every struct
/// result with an `error` union member (`MeResult`, `ItemsResult`, `Done`, …)
/// is set to `denial`, which [`CallScope::finish`] counts under its kind.
/// Used by the generated `Refused` handlers.
pub fn fill_denied(
    results: dynamic_value::Builder<'_>,
    denial: &Denial,
) -> Result<(), capnp::Error> {
    let mut results = results.downcast::<capnp::dynamic_struct::Builder<'_>>();
    for f in results.get_schema().get_non_union_fields()? {
        if !matches!(
            f.get_type().which(),
            capnp::introspect::TypeVariant::Struct(_)
        ) {
            continue;
        }
        let mut result = results
            .reborrow()
            .init(f)?
            .downcast::<capnp::dynamic_struct::Builder<'_>>();
        let Some(error) = result.get_schema().find_field_by_name("error")? else {
            continue;
        };
        denial.write(
            result
                .reborrow()
                .init(error)?
                .downcast_struct::<crate::pharos_capnp::error::Owned>(),
        );
    }
    Ok(())
}

/// Whether `s`'s active union member is named `name`.
fn active_is(s: capnp::dynamic_struct::Reader<'_>, name: &str) -> bool {
    matches!(s.which(), Ok(Some(f)) if redact::name_of(f.get_proto().get_name()) == name)
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

fn u32_field(s: capnp::dynamic_struct::Reader<'_>, name: &str) -> u32 {
    let Ok(Some(field)) = s.get_schema().find_field_by_name(name) else {
        return 0;
    };
    match s.get(field) {
        Ok(dynamic_value::Reader::UInt32(v)) => v,
        _ => 0,
    }
}

/// Bytes kept of a string read from a message (context fields, error reasons):
/// it lands in every span and log line, so a caller cannot inflate them.
const MAX_FIELD: usize = 128;

fn text_field(s: capnp::dynamic_struct::Reader<'_>, name: &str) -> String {
    let Ok(Some(field)) = s.get_schema().find_field_by_name(name) else {
        return String::new();
    };
    match s.get(field) {
        Ok(dynamic_value::Reader::Text(t)) => crate::redact::bounded_text(t, MAX_FIELD),
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
