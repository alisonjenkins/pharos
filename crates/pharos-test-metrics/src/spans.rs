//! Test support: record the names of the tracing spans a code path opens.
//!
//! [`capture`] installs one process-wide subscriber (once) and attributes each
//! span to the thread that created it, so parallel tests do not see each
//! other's spans. A thread-scoped `set_default` subscriber is not enough: a
//! callsite first hit by a parallel test with no subscriber caches
//! "never interested" and silently drops this thread's spans.
//!
//! Spans must be created on the thread that called [`capture`], i.e. run the
//! code under test on a current-thread runtime. If another global subscriber
//! was installed first, nothing is captured and the assertion says so.

use std::cell::RefCell;
use std::sync::{Arc, Mutex, Once, PoisonError};

use tracing::span::Attributes;
use tracing::{Id, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

type Buffer = Arc<Mutex<Vec<String>>>;

thread_local! {
    static ACTIVE: RefCell<Option<Buffer>> = const { RefCell::new(None) };
}

/// Span names in creation order.
#[derive(Clone, Debug, Default)]
pub struct SpanCapture {
    names: Buffer,
}

impl SpanCapture {
    pub fn names(&self) -> Vec<String> {
        self.names
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.names().iter().any(|n| n == name)
    }
}

struct Recorder;

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Recorder {
    fn on_new_span(&self, attrs: &Attributes<'_>, _id: &Id, _ctx: Context<'_, S>) {
        ACTIVE.with(|active| {
            if let Some(buf) = active.borrow().as_ref() {
                buf.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(attrs.metadata().name().to_string());
            }
        });
    }
}

/// Stops capturing on this thread when dropped.
pub struct CaptureGuard;

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        ACTIVE.with(|active| *active.borrow_mut() = None);
    }
}

/// Start recording the spans created on this thread.
pub fn capture() -> (SpanCapture, CaptureGuard) {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ =
            tracing::subscriber::set_global_default(tracing_subscriber::registry().with(Recorder));
    });
    let capture = SpanCapture::default();
    ACTIVE.with(|active| *active.borrow_mut() = Some(capture.names.clone()));
    (capture, CaptureGuard)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn records_span_names_in_creation_order() {
        let (spans, _guard) = capture();
        {
            let _outer = tracing::info_span!("outer").entered();
            let _inner = tracing::info_span!("inner").entered();
        }
        assert_eq!(spans.names(), vec!["outer", "inner"]);
        assert!(spans.contains("inner"));
        assert!(!spans.contains("missing"));
    }

    #[test]
    fn a_thread_only_sees_its_own_spans() {
        let (spans, _guard) = capture();
        std::thread::spawn(|| {
            let (other, _g) = capture();
            let _s = tracing::info_span!("elsewhere").entered();
            assert!(other.contains("elsewhere"));
        })
        .join()
        .unwrap();
        let _s = tracing::info_span!("here").entered();
        assert!(spans.contains("here"));
        assert!(!spans.contains("elsewhere"));
    }
}
