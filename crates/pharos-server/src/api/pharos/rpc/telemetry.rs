//! `Telemetry.submit`: a signed-in client's events and spans. Each event is
//! logged at its level and counted; each span is re-exported through the
//! server's OTLP pipeline (`client_spans`) and counted. Every batch is
//! bounded, and every string a client sends is cut and redacted before it
//! reaches a log line or a span (V8). `report` and the stored-report calls
//! live in `report.rs`.

use std::rc::Rc;
use std::time::Instant;

use pharos_api::pharos_capnp::{self, telemetry_event, Level};
use pharos_api::{redact, server, Denial};
use pharos_core::secret_prefix;

use super::library::read_item_id;
use super::session::Session;

/// Events one batch may carry; a client flushes a handful per tick.
pub(super) const MAX_EVENTS_PER_BATCH: u32 = 50;
/// Spans one batch may carry; the same cap as events, so one batch stays bounded.
pub(super) const MAX_SPANS_PER_BATCH: u32 = 50;
/// Batches one session may send in a burst before the rate applies.
pub(super) const TELEMETRY_BATCH_BURST: u32 = 20;
/// Sustained batches per second one session may send.
pub(super) const TELEMETRY_BATCHES_PER_SECOND: u32 = 1;
/// Seconds a refused client waits: one token refills in 1/rate, rounded up.
const RETRY_AFTER_SECS: u64 = 1;
/// Longest client string logged; a longer one is cut with its length noted.
const MAX_FIELD_BYTES: usize = 1024;

/// Field keys that name a credential, matched case-insensitively as substrings.
const CREDENTIAL_KEYS: &[&str] = &[
    "token",
    "password",
    "passwd",
    "secret",
    "auth",
    "cookie",
    "credential",
    "apikey",
    "api_key",
];

/// Auth schemes that open a value (`Bearer x`), then markers inside one.
const CREDENTIAL_MARKERS: &[&str] = &[
    "token=",
    "api_key=",
    "apikey=",
    "password=",
    "secret=",
    "x-emby-token",
    "authorization:",
];

/// Shortest run of token-shaped characters taken for a credential.
const TOKEN_SHAPE_MIN_LEN: usize = 32;

pub(super) struct Telemetry {
    session: Rc<Session>,
}

impl Telemetry {
    pub(super) fn new(session: Rc<Session>) -> Self {
        Self { session }
    }
}

impl server::telemetry::Handler for Telemetry {
    async fn submit(
        self: Rc<Self>,
        params: pharos_capnp::telemetry::SubmitParams,
        results: &mut pharos_capnp::telemetry::SubmitResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let events = params.get_events()?;
        let spans = params.get_spans()?;
        let mut out = results.get().init_result();
        let user = match self.session.current_user().await {
            Ok(user) => user,
            Err(denial) => {
                denial.write(out.init_error());
                return Ok(());
            }
        };
        match self.session.diagnostics_opted_in(user.id).await {
            Ok(true) => {}
            Ok(false) => {
                count_dropped(DropReason::NotOptedIn, events.len());
                tracing::debug!(
                    user = %user.id.0,
                    events = events.len(),
                    reason = DropReason::NotOptedIn.label(),
                    "client telemetry batch dropped"
                );
                Denial::NotPermitted {
                    what: "diagnostics not enabled for this user",
                }
                .write(out.init_error());
                return Ok(());
            }
            Err(e) => {
                count_dropped(DropReason::PrefStoreUnavailable, events.len());
                tracing::warn!(
                    user = %user.id.0,
                    error = %e,
                    "client telemetry batch dropped: opt-in unreadable"
                );
                Denial::Unavailable {
                    dependency: "pref store",
                }
                .write(out.init_error());
                return Ok(());
            }
        }
        let oversized = [
            ("telemetry_events", events.len(), MAX_EVENTS_PER_BATCH),
            ("telemetry_spans", spans.len(), MAX_SPANS_PER_BATCH),
        ]
        .into_iter()
        .find(|(_, len, max)| len > max);
        if let Some((name, len, max)) = oversized {
            count_dropped(DropReason::BatchTooLarge, events.len());
            tracing::warn!(
                batch = name,
                len,
                max,
                "client telemetry batch refused: too large"
            );
            Denial::Limit {
                name,
                value: u64::from(len),
                max: u64::from(max),
            }
            .write(out.init_error());
            return Ok(());
        }
        if !self
            .session
            .telemetry_bucket
            .borrow_mut()
            .admit(Instant::now())
        {
            count_dropped(DropReason::RateLimited, events.len());
            tracing::warn!(
                events = events.len(),
                "client telemetry batch refused: rate limited"
            );
            Denial::Throttled {
                name: "telemetry_batches",
                retry_after_secs: RETRY_AFTER_SECS,
            }
            .write(out.init_error());
            return Ok(());
        }
        for event in events.iter() {
            if let Err(e) = log_event(event) {
                tracing::debug!(error = %e, "client event unreadable, skipped");
            }
        }
        if !spans.is_empty() {
            let tracer = opentelemetry::global::tracer("pharos-ui");
            let ratio = crate::client_spans::sampling_ratio();
            let now = std::time::SystemTime::now();
            for span in spans.iter() {
                match read_span(span) {
                    Ok(Some(span)) => {
                        let outcome = match crate::client_spans::forward(&tracer, span, ratio, now)
                        {
                            Ok(()) => SpanOutcome::Forwarded,
                            Err(crate::client_spans::Refused::OutOfWindow) => {
                                SpanOutcome::OutOfWindow
                            }
                        };
                        count_span(outcome);
                    }
                    Ok(None) => count_span(SpanOutcome::InvalidId),
                    Err(e) => {
                        count_span(SpanOutcome::Unreadable);
                        tracing::debug!(error = %e, "client span unreadable, skipped");
                    }
                }
            }
        }
        out.set_ok(());
        Ok(())
    }

    async fn report(
        self: Rc<Self>,
        params: pharos_capnp::telemetry::ReportParams,
        results: &mut pharos_capnp::telemetry::ReportResults,
    ) -> Result<(), capnp::Error> {
        super::report::report(Rc::clone(&self.session), params, results).await
    }

    async fn list_reports(
        self: Rc<Self>,
        params: pharos_capnp::telemetry::ListReportsParams,
        results: &mut pharos_capnp::telemetry::ListReportsResults,
    ) -> Result<(), capnp::Error> {
        super::report::list_reports(Rc::clone(&self.session), params, results).await
    }

    async fn get_report(
        self: Rc<Self>,
        params: pharos_capnp::telemetry::GetReportParams,
        results: &mut pharos_capnp::telemetry::GetReportResults,
    ) -> Result<(), capnp::Error> {
        super::report::get_report(Rc::clone(&self.session), params, results).await
    }

    async fn delete_report(
        self: Rc<Self>,
        params: pharos_capnp::telemetry::DeleteReportParams,
        results: &mut pharos_capnp::telemetry::DeleteReportResults,
    ) -> Result<(), capnp::Error> {
        super::report::delete_report(Rc::clone(&self.session), params, results).await
    }
}

/// Attributes kept per client span; the rest are dropped.
const MAX_SPAN_ATTRS: u32 = 32;
/// Longest client span name; names are code-chosen, so a longer one is a bug.
const MAX_SPAN_NAME_BYTES: usize = 128;

/// What became of one client span: the `outcome` label of
/// `pharos_client_spans_total`.
#[derive(Clone, Copy, Debug)]
enum SpanOutcome {
    Forwarded,
    InvalidId,
    OutOfWindow,
    Unreadable,
}

impl SpanOutcome {
    fn label(self) -> &'static str {
        match self {
            SpanOutcome::Forwarded => "forwarded",
            SpanOutcome::InvalidId => "invalid_id",
            SpanOutcome::OutOfWindow => "out_of_window",
            SpanOutcome::Unreadable => "unreadable",
        }
    }
}

fn count_span(outcome: SpanOutcome) {
    metrics::counter!("pharos_client_spans_total", "outcome" => outcome.label()).increment(1);
}

/// `span` checked and made safe to export: `None` when its trace, span or
/// (non-empty) parent id is not a W3C id, which would orphan it in Tempo.
/// Its name, message and attributes are cut and redacted like event fields.
fn read_span(
    span: pharos_capnp::client_span::Reader<'_>,
) -> Result<Option<crate::client_spans::ClientSpan>, capnp::Error> {
    use crate::client_spans::{parse_span_id, parse_trace_id, ClientSpan};
    let trace_text = span.get_trace_id()?.to_str().unwrap_or("");
    let span_text = span.get_span_id()?.to_str().unwrap_or("");
    let parent_text = span.get_parent_span_id()?.to_str().unwrap_or("");
    let (Some(trace_id), Some(span_id)) = (parse_trace_id(trace_text), parse_span_id(span_text))
    else {
        tracing::debug!(
            trace_id = %redact::bounded_text(span.get_trace_id()?, 64),
            span_id = %redact::bounded_text(span.get_span_id()?, 64),
            "client span skipped: not a W3C trace/span id"
        );
        return Ok(None);
    };
    let parent_span_id = if parent_text.is_empty() {
        None
    } else if let Some(parent) = parse_span_id(parent_text) {
        Some(parent)
    } else {
        tracing::debug!(
            parent_span_id = %redact::bounded_text(span.get_parent_span_id()?, 64),
            "client span skipped: parent is not a W3C span id"
        );
        return Ok(None);
    };
    let message_text = span.get_message()?;
    let message = if message_text.to_str().map_or(true, looks_like_credential) {
        redact::REDACTED.to_owned()
    } else {
        redact::bounded_text(message_text, MAX_FIELD_BYTES)
    };
    let mut attrs = Vec::new();
    for field in span.get_attrs()?.iter().take(MAX_SPAN_ATTRS as usize) {
        let key_text = field.get_key()?;
        let value_text = field.get_value()?;
        let secret = key_text.to_str().map_or(true, is_credential_key)
            || value_text.to_str().map_or(true, looks_like_credential);
        let value = if secret {
            redact::REDACTED.to_owned()
        } else {
            redact::bounded_text(value_text, MAX_FIELD_BYTES)
        };
        attrs.push((redact::bounded_text(key_text, MAX_FIELD_BYTES), value));
    }
    Ok(Some(ClientSpan {
        trace_id,
        span_id,
        parent_span_id,
        name: redact::bounded_text(span.get_name()?, MAX_SPAN_NAME_BYTES),
        start_wall_ms: span.get_start_wall_ms(),
        duration_ms: span.get_duration_ms(),
        ok: span.get_ok(),
        message,
        attrs,
    }))
}

/// Why a batch's events were dropped: the `reason` label of
/// `pharos_client_events_dropped_total`.
#[derive(Clone, Copy, Debug)]
enum DropReason {
    BatchTooLarge,
    RateLimited,
    NotOptedIn,
    PrefStoreUnavailable,
}

impl DropReason {
    fn label(self) -> &'static str {
        match self {
            DropReason::BatchTooLarge => "batch_too_large",
            DropReason::RateLimited => "rate_limited",
            DropReason::NotOptedIn => "not_opted_in",
            DropReason::PrefStoreUnavailable => "pref_store_unavailable",
        }
    }
}

fn count_dropped(reason: DropReason, events: u32) {
    metrics::counter!("pharos_client_events_dropped_total", "reason" => reason.label())
        .increment(u64::from(events));
}

/// Counts one event and logs it at its level. An event whose kind or level
/// this server does not know is skipped, not counted under a guessed label.
fn log_event(event: telemetry_event::Reader<'_>) -> Result<(), capnp::Error> {
    let (Ok(kind), Ok(level)) = (event.get_kind(), event.get_level()) else {
        tracing::debug!("client event with an unknown kind or level skipped");
        return Ok(());
    };
    let kind_label = kind_label(kind);
    let level_label = level_label(level);
    metrics::counter!(
        "pharos_client_events_total",
        "kind" => kind_label,
        "level" => level_label
    )
    .increment(1);
    let route = redact::bounded_text(event.get_route()?, MAX_FIELD_BYTES);
    let trace_id = redact::bounded_text(event.get_trace_id()?, MAX_FIELD_BYTES);
    let item = if event.has_item() {
        read_item_id(event.get_item()?).map_or_else(|_| "invalid".to_owned(), |id| id.to_string())
    } else {
        String::new()
    };
    let play_session = if event.has_play_session() {
        let id = event
            .get_play_session()?
            .get_value()?
            .to_str()
            .unwrap_or("");
        secret_prefix(id).to_owned()
    } else {
        String::new()
    };
    let repeat = event.get_repeat();
    let fields = render_fields(event.get_fields()?)?;
    match level {
        Level::Error => tracing::error!(
            kind = kind_label,
            route = %route,
            item = %item,
            play_session = %play_session,
            trace_id = %trace_id,
            repeat,
            fields = %fields,
            "client event"
        ),
        Level::Warn => tracing::warn!(
            kind = kind_label,
            route = %route,
            item = %item,
            play_session = %play_session,
            trace_id = %trace_id,
            repeat,
            fields = %fields,
            "client event"
        ),
        Level::Info => tracing::info!(
            kind = kind_label,
            route = %route,
            item = %item,
            play_session = %play_session,
            trace_id = %trace_id,
            repeat,
            fields = %fields,
            "client event"
        ),
    }
    Ok(())
}

/// `key="value"` pairs joined by spaces. A credential-looking key or value is
/// written as `<redacted>`; a string is cut to [`MAX_FIELD_BYTES`].
fn render_fields(
    fields: capnp::struct_list::Reader<'_, telemetry_event::field::Owned>,
) -> Result<String, capnp::Error> {
    let mut out = Vec::new();
    for field in fields.iter() {
        let key_text = field.get_key()?;
        let value_text = field.get_value()?;
        let secret = key_text.to_str().map_or(true, is_credential_key)
            || value_text.to_str().map_or(true, looks_like_credential);
        let key = redact::bounded_text(key_text, MAX_FIELD_BYTES);
        let value = if secret {
            redact::REDACTED.to_owned()
        } else {
            redact::bounded_text(value_text, MAX_FIELD_BYTES)
        };
        out.push(format!("{key}={value:?}"));
    }
    Ok(out.join(" "))
}

pub(super) fn is_credential_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    CREDENTIAL_KEYS.iter().any(|k| lower.contains(k))
}

/// Whether `value` looks like a credential: an auth scheme or a token marker,
/// or one long run of token characters with a digit in it.
pub(super) fn looks_like_credential(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let scheme = ["bearer ", "basic "].iter().any(|s| lower.starts_with(s));
    if scheme || CREDENTIAL_MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    let token_chars = value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '/' | '='));
    token_chars && value.len() >= TOKEN_SHAPE_MIN_LEN && value.chars().any(|c| c.is_ascii_digit())
}

fn kind_label(kind: pharos_capnp::EventKind) -> &'static str {
    use pharos_capnp::EventKind as K;
    match kind {
        K::Panic => "panic",
        K::JsError => "jsError",
        K::RpcFailed => "rpcFailed",
        K::HttpFailed => "httpFailed",
        K::WsClosed => "wsClosed",
        K::RendererMode => "rendererMode",
        K::WebglContextLost => "webglContextLost",
        K::LongFrame => "longFrame",
        K::RoutePaint => "routePaint",
        K::SlowInteraction => "slowInteraction",
        K::PlaybackDecision => "playbackDecision",
        K::PlaybackStall => "playbackStall",
        K::PlaybackError => "playbackError",
        K::QualitySwitch => "qualitySwitch",
        K::Seek => "seek",
        K::ThemeFallback => "themeFallback",
        K::FocusRecovered => "focusRecovered",
        K::CacheServed => "cacheServed",
        K::CspViolation => "cspViolation",
        K::GroupDrift => "groupDrift",
        K::GroupHardSeek => "groupHardSeek",
        K::GroupWait => "groupWait",
        K::PlaybackResume => "playbackResume",
        K::PlaybackStart => "playbackStart",
        K::PlaybackSummary => "playbackSummary",
    }
}

fn level_label(level: Level) -> &'static str {
    match level {
        Level::Error => "error",
        Level::Warn => "warn",
        Level::Info => "info",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashSet;

    use pharos_capnp::EventKind as K;

    use super::*;

    const ALL_KINDS: [K; 25] = [
        K::Panic,
        K::JsError,
        K::RpcFailed,
        K::HttpFailed,
        K::WsClosed,
        K::RendererMode,
        K::WebglContextLost,
        K::LongFrame,
        K::RoutePaint,
        K::SlowInteraction,
        K::PlaybackDecision,
        K::PlaybackStall,
        K::PlaybackError,
        K::QualitySwitch,
        K::Seek,
        K::ThemeFallback,
        K::FocusRecovered,
        K::CacheServed,
        K::CspViolation,
        K::GroupDrift,
        K::GroupHardSeek,
        K::GroupWait,
        K::PlaybackResume,
        K::PlaybackStart,
        K::PlaybackSummary,
    ];

    /// Disarm check: a repeated label would merge two kinds' counts under
    /// one dashboard series.
    #[test]
    fn kind_and_level_labels_are_distinct() {
        let kinds: HashSet<_> = ALL_KINDS.iter().map(|k| kind_label(*k)).collect();
        assert_eq!(kinds.len(), ALL_KINDS.len());
        let levels = [Level::Error, Level::Warn, Level::Info].map(level_label);
        assert_eq!(levels.iter().collect::<HashSet<_>>().len(), levels.len());
    }

    /// Disarm check: a repeated label would merge two drop reasons under
    /// one dashboard series.
    #[test]
    fn drop_reason_labels_are_distinct() {
        let labels = ALL_DROP_REASONS.map(DropReason::label);
        assert_eq!(
            labels.iter().collect::<HashSet<_>>().len(),
            ALL_DROP_REASONS.len()
        );
    }

    #[test]
    fn span_outcome_labels_are_distinct() {
        let labels = [
            SpanOutcome::Forwarded,
            SpanOutcome::InvalidId,
            SpanOutcome::OutOfWindow,
            SpanOutcome::Unreadable,
        ]
        .map(SpanOutcome::label);
        assert_eq!(labels.iter().collect::<HashSet<_>>().len(), labels.len());
    }

    const ALL_DROP_REASONS: [DropReason; 4] = [
        DropReason::BatchTooLarge,
        DropReason::RateLimited,
        DropReason::NotOptedIn,
        DropReason::PrefStoreUnavailable,
    ];

    #[test]
    fn credential_keys_are_recognised_whatever_their_case() {
        for key in ["accessToken", "X-Emby-Authorization", "Password", "api_key"] {
            assert!(is_credential_key(key), "{key} should be redacted");
        }
        for key in ["route", "reason", "bufferedSeconds"] {
            assert!(!is_credential_key(key), "{key} should be logged");
        }
    }

    #[test]
    fn credential_values_are_recognised_and_plain_values_are_not() {
        for value in [
            "Bearer abc.def",
            "token=xyz",
            "x-emby-token: t",
            "aB3dE5fG7hJ9kL1mN3pQ5rS7tU9vW1xY3",
        ] {
            assert!(looks_like_credential(value), "{value} should be redacted");
        }
        for value in ["decode failed", "/Items/42", "1280x720", "Pharos Night"] {
            assert!(!looks_like_credential(value), "{value} should be logged");
        }
    }
}
