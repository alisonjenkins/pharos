//! Counts store round trips from the `#[tracing::instrument]` span every
//! `pharos_store_sqlx` method opens. Install with
//! `tracing::subscriber::set_default(tracing_subscriber::registry().with(log.clone()))`
//! on a current-thread test so the guard sees the whole request.
#![allow(dead_code, clippy::unwrap_used)]

use std::sync::{Arc, Mutex};
use tracing_subscriber::{layer::Context, Layer};

#[derive(Clone, Default)]
pub struct StoreCallLog(Arc<Mutex<Vec<String>>>, Arc<Mutex<Vec<(String, u64)>>>);

impl StoreCallLog {
    /// The method names called since the last `take`, in order.
    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }

    /// Each call that recorded a `count` field (how many ids it bound), as
    /// `(method, count)`, since the last `take_counts`.
    pub fn take_counts(&self) -> Vec<(String, u64)> {
        std::mem::take(&mut *self.1.lock().unwrap())
    }
}

struct CountField(Option<u64>);

impl tracing::field::Visit for CountField {
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        if field.name() == "count" {
            self.0 = Some(value);
        }
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        if field.name() == "count" {
            self.0 = u64::try_from(value).ok();
        }
    }

    fn record_debug(&mut self, _field: &tracing::field::Field, _value: &dyn std::fmt::Debug) {}
}

impl<S: tracing::Subscriber> Layer<S> for StoreCallLog {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: Context<'_, S>,
    ) {
        let meta = attrs.metadata();
        if meta.target().starts_with("pharos_store_sqlx") {
            self.0.lock().unwrap().push(meta.name().to_string());
            let mut count = CountField(None);
            attrs.record(&mut count);
            if let Some(n) = count.0 {
                self.1.lock().unwrap().push((meta.name().to_string(), n));
            }
        }
    }
}
