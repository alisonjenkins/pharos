//! `Session`: a signed-in user's root capability.

use std::rc::Rc;

use actix_web::web;
use pharos_api::pharos_capnp::session as wire;
use pharos_api::{server, Connection, Denial};
use pharos_core::{AuthError, User};

use super::bootstrap::token_denial;
use super::library::Library;
use super::playback::Playback;
use super::user_data::UserData;
use super::ConnectionCtx;
use crate::services::auth::{revoke_token, user_for_token, RevokeCause, TokenRefusal};
use crate::state::AppState;

pub(super) struct Session {
    pub(super) state: web::Data<AppState>,
    ctx: Rc<ConnectionCtx>,
    conn: Rc<Connection>,
    user: User,
    /// The device token this session was opened with; `logout` revokes it.
    pub(super) token: String,
}

impl Session {
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

    pub(super) fn conn(&self) -> Rc<Connection> {
        Rc::clone(&self.conn)
    }

    /// Whether the client reached the server over a non-private address,
    /// the same test a Jellyfin request gets.
    pub(super) fn client_is_remote(&self) -> bool {
        crate::api::jellyfin::items::ip_is_remote(&self.ctx.client_ip)
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
        let mut me = results.get().init_result().init_me();
        me.reborrow().init_id().set_uuid(self.user.id.0.as_bytes());
        me.set_name(self.user.name.as_str());
        me.set_is_admin(self.user.policy.admin);
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
}
