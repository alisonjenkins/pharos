use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::json;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing_subscriber::layer::{Context, SubscriberExt as _};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

use crate::http::{post, serve, silent};

#[derive(Debug, Default, Clone)]
struct Recorded {
    name: &'static str,
    parent: Option<u64>,
    fields: HashMap<String, String>,
}

type Spans = Arc<Mutex<HashMap<u64, Recorded>>>;

struct Fields<'a>(&'a mut HashMap<String, String>);

impl Visit for Fields<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(
            field.name().to_owned(),
            format!("{value:?}").trim_matches('"').to_owned(),
        );
    }
}

struct Capture(Spans);

impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Capture {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut rec = Recorded {
            name: attrs.metadata().name(),
            parent: ctx
                .span(id)
                .and_then(|s| s.parent())
                .map(|p| p.id().into_u64()),
            fields: HashMap::new(),
        };
        attrs.record(&mut Fields(&mut rec.fields));
        self.0.lock().unwrap().insert(id.into_u64(), rec);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, _: Context<'_, S>) {
        if let Some(rec) = self.0.lock().unwrap().get_mut(&id.into_u64()) {
            values.record(&mut Fields(&mut rec.fields));
        }
    }
}

/// Every span from every thread of this test binary; other tests' spans are
/// told apart by their song's line count.
fn spans() -> Spans {
    static SPANS: OnceLock<Spans> = OnceLock::new();
    SPANS
        .get_or_init(|| {
            let spans = Spans::default();
            let subscriber = tracing_subscriber::registry().with(Capture(Arc::clone(&spans)));
            tracing::subscriber::set_global_default(subscriber).unwrap();
            spans
        })
        .clone()
}

fn descends_from(all: &HashMap<u64, Recorded>, mut id: u64, root: u64) -> bool {
    while let Some(parent) = all.get(&id).and_then(|r| r.parent) {
        if parent == root {
            return true;
        }
        id = parent;
    }
    false
}

#[test]
fn a_song_traces_its_wait_decodes_and_model_calls_under_one_span() {
    let spans = spans();
    let s = serve("spans", silent(), None);
    let body = json!({"path": "song.mp3", "lines": [
        {"start_ms": 20_000, "end_ms": 30_000, "text": "one line sung here"},
        {"start_ms": 30_000, "end_ms": 40_000, "text": "another line sung there"},
        {"start_ms": 40_000, "end_ms": 50_000, "text": "the last line sung"},
        {"start_ms": 50_000, "end_ms": 51_000, "text": "\u{266a}"},
    ]});
    assert_eq!(post(s.addr, &body).0, 200);

    let all = spans.lock().unwrap().clone();
    let (&root, song) = all
        .iter()
        .find(|(_, r)| {
            r.name == "aligner.align"
                && r.fields.get("aligner.lines").map(String::as_str) == Some("4")
        })
        .expect("an aligner.align span for the song");
    let f = |k: &str| song.fields.get(k).map(String::as_str);
    assert_eq!(f("aligner.path"), Some("song.mp3"));
    assert_eq!(f("aligner.offset_ms"), Some("-8000"));
    assert_eq!(f("aligner.words_aligned"), Some("12"));
    assert!(f("aligner.decode_ms").is_some() && f("aligner.model_ms").is_some());
    assert_eq!(f("otel.kind"), Some("server"));
    assert_eq!(f("otel.status_code"), None);
    // The song span names where its forward passes ran (R11).
    assert_eq!(
        (f("aligner.device"), f("aligner.device_reason")),
        (Some("cpu"), Some("forced_cpu"))
    );

    let under: Vec<&Recorded> = all
        .iter()
        .filter(|(&id, _)| descends_from(&all, id, root))
        .map(|(_, r)| r)
        .collect();
    let count = |name: &str, stage: &str| {
        under
            .iter()
            .filter(|r| r.name == name && r.fields.get("stage").map(String::as_str) == Some(stage))
            .count()
    };
    assert_eq!(under.iter().filter(|r| r.name == "aligner.wait").count(), 1);
    assert_eq!(
        (
            count("aligner.decode", "probe"),
            count("aligner.model", "probe")
        ),
        (3, 1),
        "the probes are decoded one by one and modelled together"
    );
    let probe_model = under
        .iter()
        .find(|r| {
            r.name == "aligner.model" && r.fields.get("stage").map(String::as_str) == Some("probe")
        })
        .unwrap();
    assert_eq!(
        probe_model.fields.get("windows").map(String::as_str),
        Some("3")
    );
    // The line with no alignable words is neither decoded nor modelled.
    assert_eq!(
        (
            count("aligner.decode", "line"),
            count("aligner.model", "line")
        ),
        (3, 3)
    );
    let decode = under.iter().find(|r| r.name == "aligner.decode").unwrap();
    assert!(decode.fields.contains_key("window_ms"));
    let line_model = under
        .iter()
        .find(|r| {
            r.name == "aligner.model" && r.fields.get("stage").map(String::as_str) == Some("line")
        })
        .unwrap();
    assert!(line_model.fields.contains_key("line") && line_model.fields.contains_key("words"));
}

#[test]
fn a_failed_song_marks_its_span_as_an_error() {
    let spans = spans();
    let s = serve("spanfail", silent(), None);
    let body = json!({"path": "gone.mp3", "lines": [
        {"start_ms": 0, "end_ms": 1, "text": "a"}, {"start_ms": 1, "end_ms": 2, "text": "b"},
        {"start_ms": 2, "end_ms": 3, "text": "c"}, {"start_ms": 3, "end_ms": 4, "text": "d"},
        {"start_ms": 4, "end_ms": 5, "text": "e"},
    ]});
    assert_eq!(post(s.addr, &body).0, 404);
    let all = spans.lock().unwrap().clone();
    let song = all
        .values()
        .find(|r| {
            r.name == "aligner.align"
                && r.fields.get("aligner.lines").map(String::as_str) == Some("5")
        })
        .expect("an aligner.align span for the missing file");
    assert_eq!(
        song.fields.get("otel.status_code").map(String::as_str),
        Some("error")
    );
    assert_eq!(
        song.fields.get("otel.status_message").map(String::as_str),
        Some("file not found: gone.mp3")
    );
}
