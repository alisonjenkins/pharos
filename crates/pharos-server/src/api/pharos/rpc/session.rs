//! `Session`: a signed-in user's root capability.

use std::rc::Rc;

use actix_web::web;
use pharos_api::pharos_capnp::session as wire;
use pharos_api::{server, Connection, Denial};
use pharos_core::{AuthError, User};

use super::library::Library;
use super::ConnectionCtx;
use crate::services::auth::{revoke_token, RevokeCause};
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
