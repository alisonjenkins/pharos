//! Client spans re-exported through the server's own OTLP pipeline (spec 016
//! US1, FR-005), so a browser action and the server spans it caused share one
//! trace in Tempo and the collector is never exposed to browsers.
//!
//! The SDK always mints a span's id itself, and a root span's trace id too.
//! [`ClientIdGenerator`] lets [`forward`] hand it the ids the client already
//! chose, so the server's child spans, which carry those ids as their
//! parent, still hang off the right node.

use std::cell::Cell;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use opentelemetry::trace::{
    Span as _, SpanContext, SpanId, SpanKind, Status, TraceContextExt as _, TraceFlags, TraceId,
    TraceState, Tracer,
};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::{IdGenerator, RandomIdGenerator};

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

/// Starts and ends `span` on `tracer` with the client's ids, times and
/// status, parented on its parent span when it has one. Whether it is kept
/// is the provider's sampler's call, as for any server span.
pub fn forward<T: Tracer>(tracer: &T, span: ClientSpan) {
    let start = UNIX_EPOCH + Duration::from_millis(span.start_wall_ms);
    let end = start + Duration::from_millis(span.duration_ms);
    let parent = span.parent_span_id.map_or_else(Context::new, |parent| {
        Context::new().with_remote_span_context(SpanContext::new(
            span.trace_id,
            parent,
            TraceFlags::SAMPLED,
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

    fn exported(spans: Vec<ClientSpan>, with_generator: bool) -> Vec<SpanData> {
        let sink = Collect::default();
        let builder = SdkTracerProvider::builder().with_simple_exporter(sink.clone());
        let builder = if with_generator {
            builder.with_id_generator(ClientIdGenerator::default())
        } else {
            builder
        };
        let provider = builder.build();
        let tracer = provider.tracer("pharos-ui");
        for span in spans {
            forward(&tracer, span);
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
