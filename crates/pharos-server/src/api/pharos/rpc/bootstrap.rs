//! `Bootstrap`: what a connection can do before it has a session.

use std::rc::Rc;

use actix_web::web;
use pharos_api::pharos_capnp::{self, bootstrap as wire};
use pharos_api::{server, Connection};

use super::session::Session;
use crate::services::auth::{user_for_token, TokenRefusal};
use crate::state::AppState;

pub(super) struct Bootstrap {
    state: web::Data<AppState>,
    cookie_token: Option<String>,
    conn: Rc<Connection>,
}

impl Bootstrap {
    pub(super) fn new(
        state: web::Data<AppState>,
        cookie_token: Option<String>,
        conn: Rc<Connection>,
    ) -> Self {
        Self {
            state,
            cookie_token,
            conn,
        }
    }
}

/// Why `resume` refused, as it reaches the client in `ResumeResult.status`.
enum Refusal {
    /// No device token and no session cookie.
    NoCredential,
    /// The token is not valid (or not UTF-8).
    Token(TokenRefusal),
}

impl server::bootstrap::Handler for Bootstrap {
    async fn server_info(
        self: Rc<Self>,
        _params: wire::ServerInfoParams,
        results: &mut wire::ServerInfoResults,
    ) -> Result<(), capnp::Error> {
        let mut info = results.get().init_result().init_info();
        info.set_name(self.state.server_name.as_str());
        info.set_version(self.state.version);
        info.set_schema_version(pharos_capnp::SCHEMA_VERSION);
        // 016 FR-005a: not offered until anonymous telemetry exists.
        info.set_anonymous_errors(false);
        Ok(())
    }

    /// Resolves the device token (or, when it is empty, the upgrade's
    /// cookie) on every call, so a revoked token stops working at once.
    /// Always answers with a session: a refused one fails every call with
    /// `revoked`, and `status` says why (research R1).
    async fn resume(
        self: Rc<Self>,
        params: wire::ResumeParams,
        results: &mut wire::ResumeResults,
    ) -> Result<(), capnp::Error> {
        let explicit = params.get()?.get_device_token()?;
        let token = match explicit.to_str() {
            Ok("") => self.cookie_token.clone().ok_or(Refusal::NoCredential),
            Ok(t) => Ok(t.to_owned()),
            Err(_) => Err(Refusal::Token(TokenRefusal::InvalidToken)),
        };
        let user = match token {
            Ok(token) => user_for_token(&self.state.stores, &token)
                .await
                .map_err(Refusal::Token),
            Err(refusal) => Err(refusal),
        };
        let mut result = results.get().init_result();
        match user {
            Ok(user) => {
                self.conn.set_user(&user.name);
                let session = Session::new(self.state.clone(), user);
                result.set_session(server::session::client(session, &self.conn));
                result.init_status().set_ok(());
            }
            Err(refusal) => {
                result.set_session(server::session::client(
                    server::session::Revoked(Rc::clone(&self.conn)),
                    &self.conn,
                ));
                let mut err = result.init_status().init_error();
                match refusal {
                    Refusal::NoCredential => {
                        err.set_reason("no device token and no pharos_session cookie");
                        err.get_kind().set_revoked(());
                    }
                    Refusal::Token(TokenRefusal::Unavailable(detail)) => {
                        // B216: ours and transient; the client keeps its token
                        // and resumes again.
                        err.set_reason(format!("auth store unavailable: {detail}").as_str());
                        err.get_kind().set_unavailable("token store");
                    }
                    Refusal::Token(other) => {
                        err.set_reason(other.label());
                        err.get_kind().set_revoked(());
                    }
                }
            }
        }
        Ok(())
    }
}
