//! `Bootstrap`: what a connection can do before it has a session.

use std::rc::Rc;

use actix_web::web;
use pharos_api::pharos_capnp::{self, bootstrap as wire};
use pharos_api::{server, Connection, Denial};

use pharos_core::{SecretString, User};

use super::session::Session;
use super::ConnectionCtx;
use crate::auth::LoginRefusal;
use crate::services::auth::{
    password_login, user_for_token, LoginTransport, PasswordLoginError, TokenRefusal,
};
use crate::state::AppState;

pub(super) struct Bootstrap {
    state: web::Data<AppState>,
    ctx: Rc<ConnectionCtx>,
    conn: Rc<Connection>,
}

impl Bootstrap {
    pub(super) fn new(
        state: web::Data<AppState>,
        ctx: Rc<ConnectionCtx>,
        conn: Rc<Connection>,
    ) -> Self {
        Self { state, ctx, conn }
    }

    /// Counts `user` against the per-user connection cap (FR-010).
    fn admit_user(&self, user: &User) -> Result<(), Refusal> {
        let ctx = &self.ctx;
        ctx.limits
            .sign_in(&mut ctx.slot.borrow_mut(), user.id, ctx.max_per_user)
            .map_err(|held| Refusal::TooManyConnections {
                held,
                max: ctx.max_per_user,
            })
    }

    /// The session `resume` and `login` hand out, signed in with `token`.
    /// The token is re-checked while the connection lives (T025a) and is the
    /// one `Session.logout` revokes.
    fn open_session(&self, user: User, token: String) -> pharos_capnp::session::Client {
        self.ctx.remember_token(token.clone(), user.id);
        self.conn.set_user(&user.name);
        let session = Session::new(self.state.clone(), Rc::clone(&self.ctx), user, token);
        server::session::client(session, &self.conn)
    }
}

/// A refused `login` as the client sees it. A wrong password and an unknown
/// user are one answer (B234); a backend's error text stays in the log.
fn login_denial(error: &PasswordLoginError) -> Denial {
    match error {
        PasswordLoginError::Refused(LoginRefusal::Invalid) => Denial::NotPermitted {
            what: "invalid credentials",
        },
        // V174: the wait is machine-readable, in seconds.
        PasswordLoginError::Refused(LoginRefusal::Throttled(retry)) => Denial::Throttled {
            name: "login_retry_after_secs",
            retry_after_secs: retry.secs,
        },
        PasswordLoginError::Refused(LoginRefusal::Backend(_)) => Denial::Unavailable {
            dependency: "user store",
        },
        PasswordLoginError::Issue(_) => Denial::Unavailable {
            dependency: "token store",
        },
        PasswordLoginError::Capped { held, max } => Refusal::TooManyConnections {
            held: *held,
            max: *max,
        }
        .denial(),
    }
}

/// Why `resume` refused, as it reaches the client in `ResumeResult.status`.
#[derive(Clone)]
enum Refusal {
    /// No device token and no session cookie.
    NoCredential,
    /// The token is not valid (or not UTF-8).
    Token(TokenRefusal),
    /// The user already holds `max_connections_per_user` connections.
    TooManyConnections { held: u32, max: u32 },
}

impl Refusal {
    /// What the refused session answers, in `status` and on every call. A
    /// fixed reason only: a store outage's own error text stays in the
    /// server's log and never reaches the client.
    fn denial(&self) -> Denial {
        match self {
            Refusal::NoCredential => Denial::Revoked {
                reason: "no device token and no pharos_session cookie",
            },
            // B216: ours and transient; the client keeps its token and
            // resumes again.
            Refusal::Token(TokenRefusal::Unavailable(_)) => Denial::Unavailable {
                dependency: "token store",
            },
            Refusal::Token(other) => Denial::Revoked {
                reason: other.label(),
            },
            Refusal::TooManyConnections { held, max } => Denial::Limit {
                name: "connections_per_user",
                value: u64::from(*held),
                max: u64::from(*max),
            },
        }
    }
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
    /// cookie) on every call, so a revoked token cannot resume. A session
    /// already open is ended by the connection's re-check of the tokens it
    /// resumed with (T025a, `[server.native_api].revalidate_secs`).
    /// Always answers with a session: a refused one fails every call with the
    /// same error as `status` (research R1).
    async fn resume(
        self: Rc<Self>,
        params: wire::ResumeParams,
        results: &mut wire::ResumeResults,
    ) -> Result<(), capnp::Error> {
        let explicit = params.get()?.get_device_token()?;
        let token = match explicit.to_str() {
            Ok("") => self.ctx.cookie_token.clone().ok_or(Refusal::NoCredential),
            Ok(t) => Ok(t.to_owned()),
            Err(_) => Err(Refusal::Token(TokenRefusal::InvalidToken)),
        };
        let user = match &token {
            Ok(token) => user_for_token(&self.state.stores, token)
                .await
                .map_err(Refusal::Token),
            Err(refusal) => Err(refusal.clone()),
        };
        // `user` is only `Ok` when `token` is.
        let opened = user
            .and_then(|user| self.admit_user(&user).map(|()| user))
            .and_then(|user| token.map(|token| (user, token)));
        let mut result = results.get().init_result();
        match opened {
            Ok((user, token)) => {
                result.set_session(self.open_session(user, token));
                result.init_status().set_ok(());
            }
            Err(refusal) => {
                if let Refusal::Token(TokenRefusal::Unavailable(detail)) = &refusal {
                    tracing::warn!(
                        connection_id = self.conn.id(),
                        error = %detail,
                        "native API resume: token store unavailable"
                    );
                }
                let denial = Rc::new(refusal.denial());
                denial.write(result.reborrow().init_status().init_error());
                result.set_session(server::session::client(
                    server::session::Refused(Rc::clone(&self.conn), denial),
                    &self.conn,
                ));
            }
        }
        Ok(())
    }

    /// Checks the password through the login throttle (V174) and answers
    /// with a session and the device token a native app keeps for `resume`.
    /// The device id is the server's (`pharos-native-…`), never the
    /// client's, so it cannot pose as an API key (B235). The per-user
    /// connection cap is checked before any token is issued.
    async fn login(
        self: Rc<Self>,
        params: wire::LoginParams,
        results: &mut wire::LoginResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let (name, password) = (params.get_name()?, params.get_password()?);
        let credentials = name
            .to_string()
            .and_then(|n| Ok((n, password.to_string()?)));
        let result = results.get().init_result();
        // Not UTF-8: no account has that name or password.
        let Ok((name, password)) = credentials else {
            login_denial(&PasswordLoginError::Refused(LoginRefusal::Invalid))
                .write(result.init_error());
            return Ok(());
        };
        let password = SecretString::new(password);
        let device_id = format!("pharos-native-{}", uuid::Uuid::new_v4().simple());
        let login = password_login(
            &self.state,
            LoginTransport::Rpc,
            &name,
            &password,
            self.ctx.client_ip,
            &device_id,
            |user| match self.admit_user(user) {
                Err(Refusal::TooManyConnections { held, max }) => Err((held, max)),
                _ => Ok(()),
            },
        )
        .await;
        match login {
            Ok((user, token)) => {
                let token = token.0.expose().to_owned();
                let mut ok = result.init_ok();
                ok.set_device_token(token.as_str());
                ok.set_session(self.open_session(user, token));
            }
            Err(e) => login_denial(&e).write(result.init_error()),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// B216, and the backend's error text never reaches the client.
    #[test]
    fn an_outage_is_unavailable_without_the_backends_error_text() {
        let refusal = Refusal::Token(TokenRefusal::Unavailable(
            "pool timed out talking to db-3.internal:5432".into(),
        ));
        assert_eq!(
            refusal.denial(),
            Denial::Unavailable {
                dependency: "token store"
            }
        );
    }

    #[test]
    fn a_bad_or_missing_credential_is_revoked_and_a_cap_is_a_limit() {
        assert!(matches!(
            Refusal::NoCredential.denial(),
            Denial::Revoked { .. }
        ));
        for t in [TokenRefusal::InvalidToken, TokenRefusal::UserRevoked] {
            assert!(matches!(Refusal::Token(t).denial(), Denial::Revoked { .. }));
        }
        assert_eq!(
            Refusal::TooManyConnections { held: 16, max: 16 }.denial(),
            Denial::Limit {
                name: "connections_per_user",
                value: 16,
                max: 16
            }
        );
    }
}
