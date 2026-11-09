//! `Session`: a signed-in user's root capability.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use actix_web::web;
use pharos_api::pharos_capnp::session as wire;
use pharos_api::{server, Connection, Denial};
use pharos_core::{
    AuthError, DomainResult, SecretString, TokenId, TokenScope, TokenStore, User, UserId,
    UserPrefStore,
};

use super::bootstrap::{qc_denial, token_denial};
use super::group::Group;
use super::library::Library;
use super::playback::Playback;
use super::prefs::{Prefs, DIAGNOSTICS_OPT_IN};
use super::socket::MessageBucket;
use super::telemetry::{Telemetry, TELEMETRY_BATCHES_PER_SECOND, TELEMETRY_BATCH_BURST};
use super::user_data::UserData;
use super::ConnectionCtx;
use crate::auth::LoginRefusal;
use crate::services::auth::{
    change_own_password, revoke_token, user_for_token, ChangePasswordError, RevokeCause,
    TokenRefusal,
};
use crate::state::AppState;

/// How long a session trusts the diagnostics opt-in it last read. A batch
/// then costs no store read, and an opt-out made on another device still
/// stops the next batch within this long (one made on this session at once).
pub(super) const OPT_IN_CACHE_TTL: Duration = Duration::from_secs(5);

pub(super) struct Session {
    pub(super) state: web::Data<AppState>,
    ctx: Rc<ConnectionCtx>,
    conn: Rc<Connection>,
    user: User,
    /// The device token this session was opened with; `logout` revokes it.
    pub(super) token: String,
    /// Paces this session's `Telemetry.submit` batches.
    pub(super) telemetry_bucket: RefCell<MessageBucket>,
    /// The user's `diagnostics.opt_in` and when it was read.
    opt_in_cache: RefCell<Option<(Instant, bool)>>,
}

impl Session {
    /// Runs `hook` when this session's connection closes.
    pub(super) fn on_connection_close(&self, hook: impl FnOnce() + 'static) {
        self.ctx.on_close(hook);
    }

    pub(super) fn new(
        state: web::Data<AppState>,
        ctx: Rc<ConnectionCtx>,
        conn: Rc<Connection>,
        user: User,
        token: String,
    ) -> Self {
        Self {
            state,
            ctx,
            conn,
            user,
            token,
            telemetry_bucket: RefCell::new(MessageBucket::with_rate(
                Instant::now(),
                TELEMETRY_BATCH_BURST,
                TELEMETRY_BATCHES_PER_SECOND,
            )),
            opt_in_cache: RefCell::new(None),
        }
    }

    /// `Some` once `logout` ended this session's token on the connection:
    /// every call (and, as they land, every capability getter's calls)
    /// answers it instead of serving the user.
    pub(super) fn ended(&self) -> Option<Denial> {
        self.ctx.has_ended(&self.token).then_some(Denial::Revoked {
            reason: "logged out",
        })
    }

    /// The user this session was signed in as, whether or not its token is
    /// still live.
    pub(super) fn owner(&self) -> UserId {
        self.user.id
    }

    pub(super) fn conn(&self) -> Rc<Connection> {
        Rc::clone(&self.conn)
    }

    /// Whether the client reached the server over a non-private address:
    /// `ip_is_remote`, as for a Jellyfin request, on the address the
    /// WebSocket upgrade resolved through `trusted_proxies`.
    pub(super) fn client_is_remote(&self) -> bool {
        crate::api::jellyfin::items::ip_is_remote(&self.ctx.client_ip)
    }

    /// The device this session's token was issued to: the server's own
    /// `pharos-native-…` id from sign-in, never one the client states. It is
    /// the key `/Sessions` lists the device under, as the token's device is
    /// for a `/socket` that sends no `deviceId`. `None`, with a WARN, when the
    /// store errors or no longer knows the token: a session is never listed
    /// under a made-up device, which would supersede every other such one.
    pub(super) async fn device_id(&self) -> Option<pharos_core::ClientDeviceId> {
        match self.state.stores.device_for_token(&self.token).await {
            Ok(Some(device)) => Some(pharos_core::ClientDeviceId::from(device)),
            Ok(None) => {
                tracing::warn!("native session: token has no device");
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "native session: token device lookup failed");
                None
            }
        }
    }

    /// Whether `user` has opted in to sending diagnostics: the pref reads
    /// `true`. Absent or anything else is off.
    pub(super) async fn diagnostics_opted_in(&self, user: UserId) -> DomainResult<bool> {
        let cached = *self.opt_in_cache.borrow();
        if let Some((read_at, opted_in)) = cached {
            if read_at.elapsed() < OPT_IN_CACHE_TTL {
                return Ok(opted_in);
            }
        }
        let opted_in = self
            .state
            .stores
            .get_user_pref(user, DIAGNOSTICS_OPT_IN)
            .await?
            .is_some_and(|v| v == b"true");
        *self.opt_in_cache.borrow_mut() = Some((Instant::now(), opted_in));
        Ok(opted_in)
    }

    /// Drops the cached opt-in so the next batch reads the store.
    pub(super) fn forget_opt_in(&self) {
        *self.opt_in_cache.borrow_mut() = None;
    }

    /// The session's user as the store has them now, so a policy change or
    /// a disabled account applies to the next call, not the next sign-in.
    pub(super) async fn current_user(&self) -> Result<User, Denial> {
        if let Some(denial) = self.ended() {
            return Err(denial);
        }
        user_for_token(&self.state.stores, &self.token)
            .await
            .map_err(|refusal| {
                if let TokenRefusal::Unavailable(detail) = &refusal {
                    tracing::warn!(error = %detail, "native session: token store unavailable");
                }
                token_denial(&refusal)
            })
    }
}

impl server::session::Handler for Session {
    async fn me(
        self: Rc<Self>,
        _params: wire::MeParams,
        results: &mut wire::MeResults,
    ) -> Result<(), capnp::Error> {
        if let Some(denial) = self.ended() {
            denial.write(results.get().init_result().init_error());
            return Ok(());
        }
        // No avatar rather than no answer: the tag only decorates the menu.
        let avatar_tag = crate::avatar::tag(&self.state.stores, self.user.id).await;
        let mut me = results.get().init_result().init_me();
        me.reborrow().init_id().set_uuid(self.user.id.0.as_bytes());
        me.set_name(self.user.name.as_str());
        me.set_is_admin(self.user.policy.admin);
        me.set_avatar_tag(avatar_tag.as_deref().unwrap_or_default());
        Ok(())
    }

    /// Never refused: a getter answers at once so calls can pipeline on it;
    /// each `Library` call checks the session itself.
    async fn library(
        self: Rc<Self>,
        _params: wire::LibraryParams,
        results: &mut wire::LibraryResults,
    ) -> Result<(), capnp::Error> {
        let conn = Rc::clone(&self.conn);
        results
            .get()
            .set_library(server::library::client(Library::new(self), &conn));
        Ok(())
    }

    /// Never refused, like `library`; each `UserData` call checks the
    /// session itself.
    async fn user_data(
        self: Rc<Self>,
        _params: wire::UserDataParams,
        results: &mut wire::UserDataResults,
    ) -> Result<(), capnp::Error> {
        let conn = Rc::clone(&self.conn);
        results
            .get()
            .set_user_data(server::user_data::client(UserData::new(self), &conn));
        Ok(())
    }

    /// Never refused, like `library`; each `submit` checks the session itself.
    async fn telemetry(
        self: Rc<Self>,
        _params: wire::TelemetryParams,
        results: &mut wire::TelemetryResults,
    ) -> Result<(), capnp::Error> {
        let conn = Rc::clone(&self.conn);
        results
            .get()
            .set_telemetry(server::telemetry::client(Telemetry::new(self), &conn));
        Ok(())
    }

    /// Never refused, like `library`; each `Prefs` call checks the
    /// session itself.
    async fn prefs(
        self: Rc<Self>,
        _params: wire::PrefsParams,
        results: &mut wire::PrefsResults,
    ) -> Result<(), capnp::Error> {
        let conn = Rc::clone(&self.conn);
        results
            .get()
            .set_prefs(server::prefs::client(Prefs::new(self), &conn));
        Ok(())
    }

    /// Never refused, like `library`; each `Playback` call checks the
    /// session itself.
    async fn playback(
        self: Rc<Self>,
        _params: wire::PlaybackParams,
        results: &mut wire::PlaybackResults,
    ) -> Result<(), capnp::Error> {
        let conn = Rc::clone(&self.conn);
        results
            .get()
            .set_playback(server::playback::client(Playback::new(self), &conn));
        Ok(())
    }

    /// Never refused, like `library`; each `Group` call checks the session
    /// itself.
    async fn group(
        self: Rc<Self>,
        _params: wire::GroupParams,
        results: &mut wire::GroupResults,
    ) -> Result<(), capnp::Error> {
        let conn = Rc::clone(&self.conn);
        results
            .get()
            .set_group(server::group::client(Group::new(self), &conn));
        Ok(())
    }

    /// Revokes this session's device token and announces the user, so every
    /// connection signed in with it closes (T025). The user's other devices
    /// stay signed in. A token already gone is a successful logout. This
    /// connection stays open, so the reply arrives and the client can sign
    /// in again on it; its sessions with the token fail `revoked`.
    async fn logout(
        self: Rc<Self>,
        _params: wire::LogoutParams,
        results: &mut wire::LogoutResults,
    ) -> Result<(), capnp::Error> {
        let revoked =
            revoke_token(&self.state, self.user.id, &self.token, RevokeCause::Logout).await;
        let mut done = results.get().init_result();
        match revoked {
            Ok(()) | Err(AuthError::InvalidToken) => {
                self.ctx.end_token(&self.token);
                tracing::info!(user = %self.user.name, "native logout");
                done.set_ok(());
            }
            Err(e) => {
                tracing::warn!(user = %self.user.name, error = %e, "native logout: revoke failed");
                Denial::Unavailable {
                    dependency: "token store",
                }
                .write(done.init_error());
            }
        }
        Ok(())
    }

    /// A narrow token for image loaders and media players that cannot send
    /// the device token or a cookie (B542, V437, ADR-0058). It reaches only
    /// GET and HEAD on item images and on everything under `/videos/{id}/`
    /// and `/audio/{id}/` (streams, playlists, subtitles, trickplay,
    /// attachments, lyrics), expires after
    /// `[server.native_api].media_token_ttl_secs`, and is bound to the
    /// device `pharos-native:media`. A revoked or ended session is refused
    /// like any other call, and revoking the session's token (sign-out, device
    /// or session revoke) removes the media token with it.
    async fn media_token(
        self: Rc<Self>,
        _params: wire::MediaTokenParams,
        results: &mut wire::MediaTokenResults,
    ) -> Result<(), capnp::Error> {
        let user = match self.current_user().await {
            Ok(user) => user,
            Err(denial) => {
                let outcome = match denial {
                    Denial::Unavailable { .. } => MediaTokenOutcome::StoreUnavailable,
                    _ => MediaTokenOutcome::Rejected,
                };
                outcome.count();
                tracing::info!(
                    outcome = outcome.label(),
                    denial = ?denial,
                    "native media token refused"
                );
                denial.write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        let ttl_secs = self.ctx.media_token_ttl_secs();
        // Issued as a child of this session's own token, so ending the session
        // by any path ends the media token with it (B543, V438).
        let parent = TokenId(self.state.stores.token_id(&self.token));
        let issued = self
            .state
            .stores
            .issue_child_with_ttl(
                user.id,
                MEDIA_TOKEN_DEVICE_ID,
                i64::from(ttl_secs),
                TokenScope::Media,
                &parent,
            )
            .await;
        let result = results.get().init_result();
        match issued {
            Ok(issued) => {
                MediaTokenOutcome::Ok.count();
                let secret = issued.secret.0.expose();
                // The id prefix names the token in the log; the secret
                // never reaches it.
                tracing::info!(
                    user = %user.name,
                    token_id = %issued.id.0.chars().take(8).collect::<String>(),
                    ttl_secs,
                    "native media token issued"
                );
                let mut out = result.init_token();
                out.set_token(secret);
                // The store's own expiry: cut to the session token's when that ends sooner.
                out.set_expires_at_ms(
                    u64::try_from(issued.expires_at)
                        .unwrap_or(0)
                        .saturating_mul(1000),
                );
            }
            Err(e) => {
                let outcome = match e {
                    AuthError::Backend(_) => MediaTokenOutcome::StoreUnavailable,
                    _ => MediaTokenOutcome::Rejected,
                };
                outcome.count();
                tracing::warn!(
                    user = %user.name,
                    outcome = outcome.label(),
                    error = %e,
                    "native media token refused"
                );
                // An `InvalidToken` here is the session's own token, revoked
                // or expired since `current_user()`: answer it as revoked so
                // the client signs in again instead of retrying a store fault.
                match e {
                    AuthError::InvalidToken => Denial::Revoked {
                        reason: "session ended",
                    },
                    _ => Denial::Unavailable {
                        dependency: "token store",
                    },
                }
                .write(result.init_error());
            }
        }
        Ok(())
    }

    /// A short-lived Full token for the web UI's Manage pages (B920, V880,
    /// ADR-0082), which drive the admin endpoints of the Jellyfin API with a
    /// header: the browser's session cookie is `HttpOnly`, and no cookie
    /// authorizes a write (V191). Admins only. Issued as a child of this
    /// session's token, so ending the session by any path ends it, and it
    /// lives [`MANAGE_TOKEN_TTL_SECS`] at most.
    async fn manage_token(
        self: Rc<Self>,
        _params: wire::ManageTokenParams,
        results: &mut wire::ManageTokenResults,
    ) -> Result<(), capnp::Error> {
        let user = match self.current_user().await {
            Ok(user) => user,
            Err(denial) => {
                let outcome = match denial {
                    Denial::Unavailable { .. } => ManageTokenOutcome::StoreUnavailable,
                    _ => ManageTokenOutcome::Rejected,
                };
                outcome.count();
                tracing::info!(
                    outcome = outcome.label(),
                    denial = ?denial,
                    "native manage token refused"
                );
                denial.write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        if !user.policy.admin {
            ManageTokenOutcome::NotAdmin.count();
            tracing::info!(
                user = %user.name,
                outcome = ManageTokenOutcome::NotAdmin.label(),
                "native manage token refused"
            );
            Denial::NotPermitted {
                what: "manage is for admins",
            }
            .write(results.get().init_result().init_error());
            return Ok(());
        }
        let parent = TokenId(self.state.stores.token_id(&self.token));
        let issued = self
            .state
            .stores
            .issue_child_with_ttl(
                user.id,
                MANAGE_TOKEN_DEVICE_ID,
                MANAGE_TOKEN_TTL_SECS,
                TokenScope::Full,
                &parent,
            )
            .await;
        let result = results.get().init_result();
        match issued {
            Ok(issued) => {
                ManageTokenOutcome::Ok.count();
                tracing::info!(
                    user = %user.name,
                    outcome = ManageTokenOutcome::Ok.label(),
                    token_id = %issued.id.0.chars().take(8).collect::<String>(),
                    ttl_secs = MANAGE_TOKEN_TTL_SECS,
                    "native manage token issued"
                );
                let mut out = result.init_token();
                out.set_token(issued.secret.0.expose());
                out.set_expires_at_ms(
                    u64::try_from(issued.expires_at)
                        .unwrap_or(0)
                        .saturating_mul(1000),
                );
            }
            Err(e) => {
                let outcome = match e {
                    AuthError::Backend(_) => ManageTokenOutcome::StoreUnavailable,
                    _ => ManageTokenOutcome::Rejected,
                };
                outcome.count();
                tracing::warn!(
                    user = %user.name,
                    outcome = outcome.label(),
                    error = %e,
                    "native manage token refused"
                );
                match e {
                    AuthError::InvalidToken => Denial::Revoked {
                        reason: "session ended",
                    },
                    _ => Denial::Unavailable {
                        dependency: "token store",
                    },
                }
                .write(result.init_error());
            }
        }
        Ok(())
    }

    /// The user's own password (V361): see [`change_own_password`]. This
    /// session's token is kept, so the caller stays signed in here; the
    /// user's other sessions close.
    async fn change_password(
        self: Rc<Self>,
        params: wire::ChangePasswordParams,
        results: &mut wire::ChangePasswordResults,
    ) -> Result<(), capnp::Error> {
        let user = match self.current_user().await {
            Ok(user) => user,
            Err(denial) => {
                denial.write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        let params = params.get()?;
        // Not UTF-8: it cannot be the current password, nor one to store.
        let (Ok(current), Ok(new)) = (
            params.get_current()?.to_string(),
            params.get_new()?.to_string(),
        ) else {
            Denial::NotPermitted {
                what: "current password is wrong",
            }
            .write(results.get().init_result().init_error());
            return Ok(());
        };
        let changed = change_own_password(
            &self.state,
            &user,
            &SecretString::new(current),
            &SecretString::new(new),
            self.ctx.client_ip,
            &self.token,
        )
        .await;
        let mut done = results.get().init_result();
        match changed {
            Ok(_) => done.set_ok(()),
            Err(e) => write_change_password_error(&e, done.init_error()),
        }
        Ok(())
    }

    /// Approves a Quick Connect code shown on another device (V428), which
    /// then signs in as this user; an unknown or expired code answers notFound.
    async fn quick_connect_authorize(
        self: Rc<Self>,
        params: wire::QuickConnectAuthorizeParams,
        results: &mut wire::QuickConnectAuthorizeResults,
    ) -> Result<(), capnp::Error> {
        let user = match self.current_user().await {
            Ok(user) => user,
            Err(denial) => {
                denial.write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        let code = match params.get()?.get_code().ok().and_then(|r| r.to_str().ok()) {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => {
                let done = results.get().init_result();
                let mut error = done.init_error();
                error.set_reason("a quick connect code is required");
                let mut invalid = error.get_kind().init_invalid();
                invalid.set_field("code");
                invalid.set_value("");
                return Ok(());
            }
        };
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let sent = self
            .state
            .quick_connect
            .tx
            .send(crate::quick_connect::QcMsg::Authorize {
                code,
                by: user.id,
                reply: reply_tx,
            })
            .await;
        if sent.is_err() {
            Denial::Unavailable {
                dependency: "quick connect",
            }
            .write(results.get().init_result().init_error());
            return Ok(());
        }
        let res = match reply_rx.await {
            Ok(res) => res,
            Err(_) => {
                Denial::Unavailable {
                    dependency: "quick connect",
                }
                .write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        let mut done = results.get().init_result();
        match res {
            Ok(true) => done.set_ok(()),
            Ok(false) => Denial::NotFound {
                what: "quick connect code",
            }
            .write(done.init_error()),
            Err(refusal) => qc_denial(refusal).write(done.init_error()),
        }
        Ok(())
    }

    /// The session user's own avatar (V362): checked and re-encoded by
    /// [`crate::avatar::set`]; answers the new tag.
    async fn set_avatar(
        self: Rc<Self>,
        params: wire::SetAvatarParams,
        results: &mut wire::SetAvatarResults,
    ) -> Result<(), capnp::Error> {
        let user = match self.current_user().await {
            Ok(user) => user,
            Err(denial) => {
                denial.write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        let image = params.get()?.get_image()?;
        let mut result = results.get().init_result();
        if image.len() > NATIVE_MAX_AVATAR_BYTES {
            let mut error = result.init_error();
            error.set_reason("the picture is too large");
            let mut limit = error.get_kind().init_limit();
            limit.set_name("avatar_bytes");
            limit.set_value(image.len() as u64);
            limit.set_max(NATIVE_MAX_AVATAR_BYTES as u64);
            return Ok(());
        }
        match crate::avatar::set(
            &self.state.stores,
            user.id,
            image.to_vec(),
            crate::avatar::raw,
        )
        .await
        {
            Ok(tag) => result.set_tag(tag.as_str()),
            Err(e) => write_avatar_error(&e, result.init_error()),
        }
        Ok(())
    }

    async fn remove_avatar(
        self: Rc<Self>,
        _params: wire::RemoveAvatarParams,
        results: &mut wire::RemoveAvatarResults,
    ) -> Result<(), capnp::Error> {
        let user = match self.current_user().await {
            Ok(user) => user,
            Err(denial) => {
                denial.write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        let mut done = results.get().init_result();
        match crate::avatar::remove(&self.state.stores, user.id).await {
            Ok(_) => done.set_ok(()),
            Err(e) => {
                tracing::warn!(user = %user.name, error = %e, "native avatar remove failed");
                Denial::Unavailable {
                    dependency: "user store",
                }
                .write(done.init_error());
            }
        }
        Ok(())
    }
}

/// The device id a media token is stored under: the session's own device
/// id is not kept past sign-in, so every native media token shares one.
const MEDIA_TOKEN_DEVICE_ID: &str = "pharos-native:media";

/// `pharos_media_token_issued_total`. A dashboard contract: bounded, stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MediaTokenOutcome {
    /// A token was issued.
    Ok,
    /// The token store could not answer.
    StoreUnavailable,
    /// The session was revoked or ended, or the store refused to issue.
    Rejected,
}

impl MediaTokenOutcome {
    #[cfg(test)]
    pub(crate) const ALL: [MediaTokenOutcome; 3] =
        [Self::Ok, Self::StoreUnavailable, Self::Rejected];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::StoreUnavailable => "store_unavailable",
            Self::Rejected => "rejected",
        }
    }

    fn count(self) {
        metrics::counter!("pharos_media_token_issued_total", "outcome" => self.label())
            .increment(1);
    }
}

/// The device id every manage token is stored under, like
/// [`MEDIA_TOKEN_DEVICE_ID`].
const MANAGE_TOKEN_DEVICE_ID: &str = "pharos-native:manage";

/// Whether `device_id` names a token issued as a child of a session (a media
/// or manage token) rather than a device: the device list shows the session's
/// own device, not one row per child (B920).
pub fn is_child_token_device(device_id: &str) -> bool {
    matches!(device_id, MEDIA_TOKEN_DEVICE_ID | MANAGE_TOKEN_DEVICE_ID)
}

/// How long a manage token lives: an hour, and the page asks for another
/// before it ends. Short, because script on the page can read it.
pub const MANAGE_TOKEN_TTL_SECS: i64 = 3_600;

/// `pharos_manage_token_issued_total`. A dashboard contract: bounded, stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManageTokenOutcome {
    /// A token was issued.
    Ok,
    /// The user is not an admin.
    NotAdmin,
    /// The token store could not answer.
    StoreUnavailable,
    /// The session was revoked or ended, or the store refused to issue.
    Rejected,
}

impl ManageTokenOutcome {
    #[cfg(test)]
    pub(crate) const ALL: [ManageTokenOutcome; 4] = [
        Self::Ok,
        Self::NotAdmin,
        Self::StoreUnavailable,
        Self::Rejected,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NotAdmin => "not_admin",
            Self::StoreUnavailable => "store_unavailable",
            Self::Rejected => "rejected",
        }
    }

    fn count(self) {
        metrics::counter!("pharos_manage_token_issued_total", "outcome" => self.label())
            .increment(1);
    }
}

/// Largest picture `setAvatar` takes. A WebSocket message is at most 1 MiB,
/// and the web UI scales a photo down before it sends it.
pub const NATIVE_MAX_AVATAR_BYTES: usize = 768 * 1024;

/// A refused `setAvatar`: a picture the server would not take is `invalid`
/// (field `image`), with the reason fit to show; failures of ours are
/// `unavailable`.
fn write_avatar_error(
    error: &crate::avatar::AvatarError,
    mut out: pharos_api::pharos_capnp::error::Builder<'_>,
) {
    use crate::avatar::AvatarError;
    match error {
        AvatarError::Empty
        | AvatarError::TooLarge { .. }
        | AvatarError::Unsupported
        | AvatarError::Decode(_) => {
            out.set_reason(match error {
                AvatarError::Empty => "the picture is empty",
                AvatarError::TooLarge { .. } => "the picture is too large",
                AvatarError::Unsupported => "not a PNG, JPEG, WebP or GIF picture",
                _ => "the picture could not be read",
            });
            let mut invalid = out.get_kind().init_invalid();
            invalid.set_field("image");
            invalid.set_value("");
        }
        AvatarError::Busy => Denial::Unavailable {
            dependency: "image decoder",
        }
        .write(out),
        AvatarError::Encode(_) | AvatarError::Store(_) => Denial::Unavailable {
            dependency: "user store",
        }
        .write(out),
    }
}

/// A refused `changePassword` as the client sees it; backend error text
/// stays in the log.
fn write_change_password_error(
    error: &ChangePasswordError,
    mut out: pharos_api::pharos_capnp::error::Builder<'_>,
) {
    let denial = match error {
        ChangePasswordError::EmptyNew => {
            out.set_reason("a new password is required");
            let mut invalid = out.get_kind().init_invalid();
            invalid.set_field("new");
            invalid.set_value("");
            return;
        }
        // Stored already: say so, so the client does not retry with the
        // old password. A retry with the new one revokes again.
        ChangePasswordError::Revoke(_) => {
            out.set_reason(pharos_api::PASSWORD_CHANGED_REVOKE_FAILED);
            out.get_kind().set_unavailable("token store");
            return;
        }
        ChangePasswordError::Refused(LoginRefusal::Invalid | LoginRefusal::LockedOut(_)) => {
            Denial::NotPermitted {
                what: "current password is wrong",
            }
        }
        // V174: the wait is machine-readable, in seconds.
        ChangePasswordError::Refused(LoginRefusal::Throttled(retry)) => Denial::Throttled {
            name: "login_retry_after_secs",
            retry_after_secs: retry.secs,
        },
        ChangePasswordError::Refused(LoginRefusal::Backend(_)) | ChangePasswordError::Store(_) => {
            Denial::Unavailable {
                dependency: "user store",
            }
        }
    };
    denial.write(out);
}

#[cfg(test)]
mod tests {
    use super::{ManageTokenOutcome, MediaTokenOutcome};

    /// The outcome labels are a dashboard contract: fixed and distinct.
    #[test]
    fn media_token_outcome_labels_are_stable_and_distinct() {
        let labels: Vec<_> = MediaTokenOutcome::ALL.iter().map(|o| o.label()).collect();
        assert_eq!(labels, ["ok", "store_unavailable", "rejected"]);
    }

    #[test]
    fn manage_token_outcome_labels_are_stable_and_distinct() {
        let labels: Vec<_> = ManageTokenOutcome::ALL.iter().map(|o| o.label()).collect();
        assert_eq!(labels, ["ok", "not_admin", "store_unavailable", "rejected"]);
    }
}
