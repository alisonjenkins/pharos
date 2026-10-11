//! Counts store round trips from the `#[tracing::instrument]` span every
//! `pharos_store_sqlx` method opens. Install with
//! `tracing::subscriber::set_default(tracing_subscriber::registry().with(log.clone()))`
//! on a current-thread test so the guard sees the whole request.
#![allow(dead_code, clippy::unwrap_used)]

use std::sync::{Arc, Mutex};
use tracing_subscriber::{layer::Context, Layer};

#[derive(Clone, Default)]
pub struct StoreCallLog(Arc<Mutex<Vec<String>>>);

impl StoreCallLog {
    /// The method names called since the last `take`, in order.
    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
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
        }
    }
}
