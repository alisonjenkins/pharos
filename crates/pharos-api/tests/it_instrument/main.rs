//! V173 / 017 §Observability: the generated `Instrumented<S>` wrappers.
//! One binary for the group (CLAUDE.md); each test installs its subscriber
//! and recorder thread-locally, so none touches process-global state.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod guard;
mod instrument;
mod labels;
mod redact;
mod refused;
mod registration;
mod resume;
mod schema_copy;
mod support;
mod wire;
