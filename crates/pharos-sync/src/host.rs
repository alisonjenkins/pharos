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
use crate::registry::{GroupRegistry, RegistryError};
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

    /// The group `user` may join, or `None` when it does not exist or the user
    /// may not see it: the two answer alike so a hidden group cannot be
    /// probed. A group that is not live here but persisted, or live on another
    /// replica, is hydrated or routed only after the user's access is checked
    /// (V582).
    fn joinable_group<'a>(
        &'a self,
        user: UserId,
        registry: &'a GroupRegistry,
        group_id: crate::messages::GroupId,
    ) -> Pin<Box<dyn Future<Output = Result<Option<GroupHandle>, RegistryError>> + Send + 'a>>;
}
