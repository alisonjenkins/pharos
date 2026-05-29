//! Auth boundary for the WS handler.
//!
//! The sync crate authenticates the opening `Hello` frame's token but
//! must not depend on `pharos-server`'s `AppState`. `TokenResolver`
//! is the thin trait the server impls (over `pharos_core::TokenStore`)
//! and registers as actix `web::Data<Arc<dyn TokenResolver>>`.
//!
//! Dyn-safe via `Pin<Box<dyn Future>>` returns — same pattern as
//! `pharos_transcode::FfmpegBackend`. Stable Rust, no `async-trait`
//! macro dep.

use pharos_core::{SecretString, UserId};
use std::future::Future;
use std::pin::Pin;

pub trait TokenResolver: Send + Sync + 'static {
    fn resolve<'a>(
        &'a self,
        token: &'a SecretString,
    ) -> Pin<Box<dyn Future<Output = Option<UserId>> + Send + 'a>>;
}
