//! Client spans re-exported through the server's own OTLP pipeline (spec 016
//! US1, FR-005), so a browser action and the server spans it caused share one
//! trace in Tempo and the collector is never exposed to browsers.
//!
//! The SDK always mints a span's id itself, and a root span's trace id too.
//! [`ClientIdGenerator`] lets [`forward`] hand it the ids the client already
//! chose, so the server's child spans, which carry those ids as their
//! parent, still hang off the right node.

use std::cell::Cell;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use opentelemetry::trace::{
    Span as _, SpanContext, SpanId, SpanKind, Status, TraceContextExt as _, TraceFlags, TraceId,
    TraceState, Tracer,
};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::{
    IdGenerator, RandomIdGenerator, Sampler, SamplingDecision, ShouldSample,
};

thread_local! {
    static FORCED: Cell<Option<(TraceId, SpanId)>> = const { Cell::new(None) };
}

/// The provider's id generator: random ids, except inside [`forward`],
/// where it returns the client's.
#[derive(Debug, Default)]
pub struct ClientIdGenerator(RandomIdGenerator);

impl IdGenerator for ClientIdGenerator {
    fn new_trace_id(&self) -> TraceId {
        FORCED
            .with(Cell::get)
            .map_or_else(|| self.0.new_trace_id(), |(trace, _)| trace)
    }

    fn new_span_id(&self) -> SpanId {
        FORCED
            .with(Cell::get)
            .map_or_else(|| self.0.new_span_id(), |(_, span)| span)
    }
}

/// Clears the forced ids even if starting the span panics.
struct ForcedIds;

impl ForcedIds {
    fn set(ids: (TraceId, SpanId)) -> Self {
        FORCED.with(|c| c.set(Some(ids)));
        Self
    }
}

impl Drop for ForcedIds {
    fn drop(&mut self) {
        FORCED.with(|c| c.set(None));
    }
}

/// One finished client span, ids already checked.
#[derive(Debug, Clone)]
pub struct ClientSpan {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    pub parent_span_id: Option<SpanId>,
    pub name: String,
    pub start_wall_ms: u64,
    pub duration_ms: u64,
    pub ok: bool,
    pub message: String,
    pub attrs: Vec<(String, String)>,
}

/// A W3C id parsed from lowercase hex of exactly `len` digits, not all zero.
fn hex_id(text: &str, len: usize) -> Option<u128> {
    let valid = text.len() == len && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    valid
        .then(|| u128::from_str_radix(text, 16).ok())
        .flatten()
        .filter(|v| *v != 0)
}

pub fn parse_trace_id(text: &str) -> Option<TraceId> {
    hex_id(text, 32).map(TraceId::from)
}

pub fn parse_span_id(text: &str) -> Option<SpanId> {
    hex_id(text, 16)
        .and_then(|v| u64::try_from(v).ok())
        .map(SpanId::from)
}

/// Oldest client span start accepted: a page that queued spans while
/// offline flushes them within a day.
const MAX_SPAN_AGE: Duration = Duration::from_secs(24 * 3600);
/// Furthest ahead of the server clock a client start may be (clock skew).
const MAX_SPAN_LEAD: Duration = Duration::from_secs(5 * 60);
/// Longest client span accepted; a UI action or playback session span is
/// far shorter.
const MAX_SPAN_DURATION: Duration = Duration::from_secs(3600);

/// The server's `[obs].sampling_ratio`, set once at OTLP init.
static SAMPLING_RATIO: OnceLock<f64> = OnceLock::new();

/// Records the ratio [`forward`] decides a client trace's sampling with,
/// the same one the provider's root sampler uses.
pub fn set_sampling_ratio(ratio: f64) {
    let _ = SAMPLING_RATIO.set(ratio);
}

/// The ratio in force: 1.0 (keep everything) until OTLP init sets one.
pub fn sampling_ratio() -> f64 {
    SAMPLING_RATIO.get().copied().unwrap_or(1.0)
}

/// Whether the provider's root sampler keeps a trace that starts with
/// `trace_id` at `ratio`. Deterministic on the trace id, so a client child
/// span's synthetic parent gets the verdict its root span got.
pub fn trace_sampled(trace_id: TraceId, ratio: f64) -> bool {
    if ratio >= 1.0 {
        return true;
    }
    Sampler::TraceIdRatioBased(ratio.max(0.0))
        .should_sample(None, trace_id, "client", &SpanKind::Client, &[], &[])
        .decision
        == SamplingDecision::RecordAndSample
}

/// Why a client span was not forwarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// Its start is more than a day old or minutes ahead of the server
    /// clock, or it lasts over an hour.
    OutOfWindow,
}

/// The span's start and end, when they fall inside the accepted window
/// around `now`.
fn times(span: &ClientSpan, now: SystemTime) -> Option<(SystemTime, SystemTime)> {
    let duration = Duration::from_millis(span.duration_ms);
    if duration > MAX_SPAN_DURATION {
        return None;
    }
    let start = UNIX_EPOCH.checked_add(Duration::from_millis(span.start_wall_ms))?;
    let end = start.checked_add(duration)?;
    let earliest = now.checked_sub(MAX_SPAN_AGE).unwrap_or(UNIX_EPOCH);
    let latest = now.checked_add(MAX_SPAN_LEAD)?;
    (start >= earliest && start <= latest).then_some((start, end))
}

/// Starts and ends `span` on `tracer` with the client's ids, times and
/// status, parented on its parent span when it has one. A root span is kept
/// or dropped by the provider's sampler; a child's synthetic parent carries
/// that same verdict for its trace id at `ratio`, so a trace is kept or
/// dropped whole and a client can't force retention by naming a parent.
pub fn forward<T: Tracer>(
    tracer: &T,
    span: ClientSpan,
    ratio: f64,
    now: SystemTime,
) -> Result<(), Refused> {
    let (start, end) = times(&span, now).ok_or(Refused::OutOfWindow)?;
    let parent = span.parent_span_id.map_or_else(Context::new, |parent| {
        let flags = if trace_sampled(span.trace_id, ratio) {
            TraceFlags::SAMPLED
        } else {
            TraceFlags::default()
        };
        Context::new().with_remote_span_context(SpanContext::new(
            span.trace_id,
            parent,
            flags,
            true,
            TraceState::default(),
        ))
    });
    let mut attrs: Vec<KeyValue> = span
        .attrs
        .into_iter()
        .map(|(k, v)| KeyValue::new(k, v))
        .collect();
    attrs.push(KeyValue::new("ui.source", "pharos-ui"));
    let builder = tracer
        .span_builder(span.name)
        .with_kind(SpanKind::Client)
        .with_start_time(start)
        .with_attributes(attrs);
    let mut started = {
        let _ids = ForcedIds::set((span.trace_id, span.span_id));
        builder.start_with_context(tracer, &parent)
    };
    if !span.ok {
        started.set_status(Status::error(span.message));
    }
    started.end_with_timestamp(end);
    Ok(())
}

/// `t` as milliseconds since the epoch, 0 before it.
pub fn wall_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::{Arc, Mutex};

    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};

    use super::*;

    #[derive(Debug, Clone, Default)]
    struct Collect(Arc<Mutex<Vec<SpanData>>>);

    impl SpanExporter for Collect {
        async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
            self.0.lock().unwrap().extend(batch);
            Ok(())
        }
    }

    /// The client spans' start time sits a minute before this.
    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(1_762_700_060_000)
    }

    fn exported(spans: Vec<ClientSpan>, with_generator: bool) -> Vec<SpanData> {
        exported_at(spans, with_generator, 1.0)
    }

    /// Spans exported by a provider sampling like the server's at `ratio`.
    fn exported_at(spans: Vec<ClientSpan>, with_generator: bool, ratio: f64) -> Vec<SpanData> {
        let sink = Collect::default();
        let builder = SdkTracerProvider::builder()
            .with_simple_exporter(sink.clone())
            .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
                ratio,
            ))));
        let builder = if with_generator {
            builder.with_id_generator(ClientIdGenerator::default())
        } else {
            builder
        };
        let provider = builder.build();
        let tracer = provider.tracer("pharos-ui");
        for span in spans {
            forward(&tracer, span, ratio, now()).unwrap();
        }
        let out = sink.0.lock().unwrap().clone();
        out
    }

    const TRACE: &str = "0af7651916cd43dd8448eb211c80319c";
    const ROOT: &str = "b7ad6b7169203331";
    const CHILD: &str = "00f067aa0ba902b7";

    fn span(id: &str, parent: Option<&str>, ok: bool) -> ClientSpan {
        ClientSpan {
            trace_id: parse_trace_id(TRACE).unwrap(),
            span_id: parse_span_id(id).unwrap(),
            parent_span_id: parent.map(|p| parse_span_id(p).unwrap()),
            name: "ui.action play".into(),
            start_wall_ms: 1_762_700_000_000,
            duration_ms: 250,
            ok,
            message: if ok {
                String::new()
            } else {
                "segment 404".into()
            },
            attrs: vec![("ui.route".into(), "/play/42".into())],
        }
    }

    /// A root span and its child keep the client's trace and span ids, the
    /// parent link, the client's times and an error status. Disarm check:
    /// without [`ClientIdGenerator`] on the provider the ids are random and
    /// the first assertions fail.
    #[test]
    fn a_client_span_keeps_its_ids_parent_times_and_status() {
        let out = exported(
            vec![span(ROOT, None, true), span(CHILD, Some(ROOT), false)],
            true,
        );
        assert_eq!(out.len(), 2);
        let root = &out[0];
        assert_eq!(root.span_context.trace_id().to_string(), TRACE);
        assert_eq!(root.span_context.span_id().to_string(), ROOT);
        assert_eq!(root.parent_span_id, SpanId::INVALID);
        assert_eq!(wall_ms(root.start_time), 1_762_700_000_000);
        assert_eq!(wall_ms(root.end_time), 1_762_700_000_250);
        let child = &out[1];
        assert_eq!(child.span_context.trace_id().to_string(), TRACE);
        assert_eq!(child.span_context.span_id().to_string(), CHILD);
        assert_eq!(child.parent_span_id.to_string(), ROOT);
        assert_eq!(child.status, Status::error("segment 404"));
        assert!(child
            .attributes
            .iter()
            .any(|kv| kv.key.as_str() == "ui.route" && kv.value.as_str() == "/play/42"));
    }

    #[test]
    fn without_the_generator_the_ids_are_not_the_clients() {
        let out = exported(vec![span(ROOT, None, true)], false);
        assert_ne!(out[0].span_context.span_id().to_string(), ROOT);
    }

    /// Ids outside a forward are random again, not the last client's.
    #[test]
    fn forced_ids_do_not_leak_past_a_forward() {
        let out = exported(vec![span(ROOT, None, true)], true);
        assert_eq!(out.len(), 1);
        let generator = ClientIdGenerator::default();
        assert_ne!(generator.new_span_id().to_string(), ROOT);
        assert_ne!(generator.new_trace_id().to_string(), TRACE);
    }

    /// At a ratio below 1 a client trace is kept or dropped whole: the
    /// child's synthetic parent carries the root sampler's verdict. Disarm
    /// check: with the parent hard-coded `SAMPLED`, the dropped trace's
    /// child is exported alone.
    #[test]
    fn a_sampled_out_trace_drops_its_children_too() {
        let ratio = 0.5;
        let pick = |want: bool| {
            // The ratio sampler reads the id's low 64 bits, so spread them:
            // small counters all fall under the threshold.
            (1u128..1_000)
                .map(|i| TraceId::from(i.wrapping_mul(0x9e37_79b9_7f4a_7c15_f39c_c060_5ced_c835)))
                .find(|t| trace_sampled(*t, ratio) == want)
                .unwrap()
        };
        for (trace_id, kept) in [(pick(true), 2), (pick(false), 0)] {
            let root = ClientSpan {
                trace_id,
                ..span(ROOT, None, true)
            };
            let child = ClientSpan {
                trace_id,
                ..span(CHILD, Some(ROOT), true)
            };
            assert_eq!(exported_at(vec![root, child], true, ratio).len(), kept);
        }
    }

    #[test]
    fn spans_outside_the_time_window_are_refused() {
        let provider = SdkTracerProvider::builder().build();
        let tracer = provider.tracer("pharos-ui");
        let day_old = ClientSpan {
            start_wall_ms: 1_762_700_060_000 - 25 * 3_600_000,
            ..span(ROOT, None, true)
        };
        let future = ClientSpan {
            start_wall_ms: 1_762_700_060_000 + 10 * 60_000,
            ..span(ROOT, None, true)
        };
        let endless = ClientSpan {
            duration_ms: u64::MAX,
            ..span(ROOT, None, true)
        };
        let far = ClientSpan {
            start_wall_ms: u64::MAX,
            ..span(ROOT, None, true)
        };
        for bad in [day_old, future, endless, far] {
            assert_eq!(forward(&tracer, bad, 1.0, now()), Err(Refused::OutOfWindow));
        }
        assert_eq!(forward(&tracer, span(ROOT, None, true), 1.0, now()), Ok(()));
    }

    #[test]
    fn ids_parse_only_as_lowercase_hex_of_their_length() {
        assert!(parse_trace_id(TRACE).is_some());
        assert!(parse_span_id(ROOT).is_some());
        for bad in [
            "",
            "0af7",
            &TRACE.to_uppercase(),
            &"0".repeat(32),
            &format!("{TRACE}0"),
        ] {
            assert!(parse_trace_id(bad).is_none(), "{bad:?}");
        }
        for bad in [
            "",
            "B7AD6B7169203331",
            "0000000000000000",
            "b7ad6b716920333g",
        ] {
            assert!(parse_span_id(bad).is_none(), "{bad:?}");
        }
    }
}
