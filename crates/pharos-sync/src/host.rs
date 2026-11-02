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

use crate::group::GroupHandle;
use pharos_core::{SecretString, UserId};
use std::future::Future;
use std::pin::Pin;

pub trait TokenResolver: Send + Sync + 'static {
    fn resolve<'a>(
        &'a self,
        token: &'a SecretString,
    ) -> Pin<Box<dyn Future<Output = Option<UserId>> + Send + 'a>>;

    /// Whether `user` may create a group, asked before `CreateAndJoin`.
    fn may_create(&self, user: UserId) -> Pin<Box<dyn Future<Output = bool> + Send + '_>>;

    /// Whether `user` may join `group`, asked before a `Join` admits them. A
    /// refusal reads to the client as an unknown group.
    fn may_join<'a>(
        &'a self,
        user: UserId,
        group: &'a GroupHandle,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>;
}
