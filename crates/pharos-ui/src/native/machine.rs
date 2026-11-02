//! The native UI's connection state as a pure reducer: the browser glue
//! turns socket, HTTP and timer results into [`Event`]s and the app renders
//! [`Machine::phase`]. Kept free of IO so every transition is tested on the
//! host.
//!
//! Each connection attempt gets an epoch. Entering [`Phase::Connecting`]
//! starts a new one; events from an older attempt (a socket that closes
//! after a sign-in replaced it) are dropped.

/// What the app shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Opening `/pharos/rpc` and resuming the session cookie.
    Connecting,
    /// `[server.native_api]` is off: `/pharos/session` does not exist.
    NotEnabled,
    /// The socket would not open, or the session could not resume, for a
    /// reason the user can act on or retry.
    Unreachable {
        detail: String,
    },
    SignIn {
        error: Option<String>,
        busy: bool,
    },
    /// Signed in; screens may call the API.
    Ready,
}

/// `GET /pharos/session` after the socket would not open. Browsers hide a
/// refused upgrade's HTTP status, so this asks the server directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    Status(u16),
    /// The request itself failed (network down, server gone).
    Failed(String),
}

/// `POST /pharos/session`'s answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInReply {
    Status {
        status: u16,
        /// `Retry-After` in seconds, on a 429.
        retry_after: Option<u64>,
    },
    Failed(String),
}

/// `DELETE /pharos/session`'s answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignOutReply {
    Status(u16),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The socket opened and `resume` signed it in.
    Resumed {
        epoch: u32,
    },
    /// The socket opened but the server knows no session for this browser.
    ResumeRefused {
        epoch: u32,
    },
    /// The socket opened but `resume` failed for another reason.
    ResumeFailed {
        epoch: u32,
        error: String,
    },
    /// The socket never opened; `probe` says why.
    NotOpened {
        epoch: u32,
        probe: Probe,
    },
    /// An open socket closed, with its close frame's reason.
    Closed {
        epoch: u32,
        reason: String,
    },
    /// A call answered `revoked` (FR: a revoked session shows sign-in).
    NeedsSignIn,
    SignInSubmitted,
    SignInAnswered(SignInReply),
    SignOutSubmitted,
    SignOutAnswered(SignOutReply),
    Retry,
}

/// The close reason the server sends when the session's token is revoked.
const REVOKED: &str = "revoked";

/// First reconnect delay after an open socket drops; doubles per attempt.
const BACKOFF_BASE_MS: u32 = 500;
/// Cap on the reconnect delay, so a restarted server is found within 30 s.
const BACKOFF_MAX_MS: u32 = 30_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    phase: Phase,
    epoch: u32,
    /// Reconnects since the last successful resume.
    attempt: u32,
    signing_out: bool,
    /// Connecting again after a signed-in socket dropped: the screens stay
    /// up, so a playing video is not torn down by a network blip.
    reconnecting: bool,
    /// A non-blocking message for the signed-in shell, e.g. a failed
    /// sign-out.
    notice: Option<String>,
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

impl Machine {
    pub fn new() -> Self {
        Self {
            phase: Phase::Connecting,
            epoch: 0,
            attempt: 0,
            signing_out: false,
            reconnecting: false,
            notice: None,
        }
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    /// The current connection attempt; events carry it back.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub fn signing_out(&self) -> bool {
        self.signing_out
    }

    pub fn reconnecting(&self) -> bool {
        self.reconnecting
    }

    /// Whether the signed-in screens show: ready, or reconnecting after a
    /// drop.
    pub fn shows_screens(&self) -> bool {
        match self.phase {
            Phase::Ready => true,
            Phase::Connecting => self.reconnecting,
            _ => false,
        }
    }

    /// How long to wait before this attempt connects: none for the first,
    /// then 0.5 s doubling to 30 s.
    pub fn delay_ms(&self) -> u32 {
        match self.attempt {
            0 => 0,
            n => BACKOFF_BASE_MS
                .saturating_mul(1u32.checked_shl(n.saturating_sub(1)).unwrap_or(u32::MAX))
                .min(BACKOFF_MAX_MS),
        }
    }

    pub fn apply(&mut self, event: Event) {
        let current = |epoch: u32| epoch == self.epoch;
        match event {
            Event::Resumed { epoch } if current(epoch) => {
                self.attempt = 0;
                self.phase = Phase::Ready;
            }
            Event::ResumeRefused { epoch } if current(epoch) => self.sign_in(None),
            Event::ResumeFailed { epoch, error } if current(epoch) => {
                self.phase = Phase::Unreachable {
                    detail: format!("The server would not resume the session: {error}"),
                };
            }
            Event::NotOpened { epoch, probe } if current(epoch) => {
                self.phase = not_opened(&probe);
            }
            Event::Closed { epoch, reason } if current(epoch) => {
                if reason == REVOKED {
                    let error = (!self.signing_out)
                        .then(|| "Your session ended. Sign in again.".to_owned());
                    self.sign_in(error);
                } else {
                    let was_signed_in = self.shows_screens();
                    self.attempt = self.attempt.saturating_add(1);
                    self.connect();
                    self.reconnecting = was_signed_in;
                }
            }
            Event::NeedsSignIn if self.phase == Phase::Ready => {
                self.sign_in(Some("Your session ended. Sign in again.".into()));
            }
            Event::SignInSubmitted => {
                if let Phase::SignIn { busy, error } = &mut self.phase {
                    *busy = true;
                    *error = None;
                }
            }
            Event::SignInAnswered(reply) => {
                if matches!(self.phase, Phase::SignIn { .. }) {
                    match sign_in_error(&reply) {
                        None => {
                            self.attempt = 0;
                            self.connect();
                        }
                        Some(error) => self.sign_in(Some(error)),
                    }
                }
            }
            Event::SignOutSubmitted if self.phase == Phase::Ready => {
                self.signing_out = true;
                self.notice = None;
            }
            Event::SignOutAnswered(reply) => {
                self.signing_out = false;
                match sign_out_error(&reply) {
                    None => self.sign_in(None),
                    Some(error) => self.notice = Some(error),
                }
            }
            Event::Retry if matches!(self.phase, Phase::Unreachable { .. } | Phase::NotEnabled) => {
                self.attempt = 0;
                self.connect();
            }
            // A stale attempt's event, or one the current phase ignores.
            _ => {}
        }
        if self.phase != Phase::Connecting {
            self.reconnecting = false;
        }
    }

    fn connect(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.notice = None;
        self.phase = Phase::Connecting;
    }

    fn sign_in(&mut self, error: Option<String>) {
        self.signing_out = false;
        self.notice = None;
        self.phase = Phase::SignIn { error, busy: false };
    }
}

fn not_opened(probe: &Probe) -> Phase {
    match probe {
        Probe::Status(404) => Phase::NotEnabled,
        // The routes exist, so the upgrade itself was refused.
        Probe::Status(405) => Phase::Unreachable {
            detail: "The native API is on, but this page could not open it. Check that \
                     [server.native_api].allowed_origins lists this page's address, and that \
                     the page is served over https: browsers drop the sign-in cookie on plain \
                     http."
                .into(),
        },
        Probe::Status(status) => Phase::Unreachable {
            detail: format!("The server answered HTTP {status} when asked about sign-in."),
        },
        Probe::Failed(error) => Phase::Unreachable {
            detail: format!("Could not reach the server: {error}"),
        },
    }
}

/// The sign-in form's error for a reply, or none when it succeeded.
fn sign_in_error(reply: &SignInReply) -> Option<String> {
    match reply {
        SignInReply::Status { status: 204, .. } => None,
        SignInReply::Status { status: 401, .. } => Some("Wrong name or password.".into()),
        SignInReply::Status {
            status: 429,
            retry_after,
        } => Some(match retry_after {
            Some(secs) => format!("Too many failed attempts. Try again in {secs} s."),
            None => "Too many failed attempts. Try again later.".into(),
        }),
        SignInReply::Status { status: 403, .. } => Some(
            "This server does not accept sign-in from this page's address \
             ([server.native_api].allowed_origins)."
                .into(),
        ),
        SignInReply::Status { status, .. } => {
            Some(format!("Sign-in is unavailable right now (HTTP {status})."))
        }
        SignInReply::Failed(error) => Some(format!("Could not reach the server: {error}")),
    }
}

/// The shell's notice for a failed sign-out. A 503 keeps the cookie so the
/// browser can retry, so the user stays signed in.
fn sign_out_error(reply: &SignOutReply) -> Option<String> {
    match reply {
        SignOutReply::Status(204) => None,
        SignOutReply::Status(status) => {
            Some(format!("Could not sign out (HTTP {status}). Try again."))
        }
        SignOutReply::Failed(error) => Some(format!("Could not sign out: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> Machine {
        let mut m = Machine::new();
        m.apply(Event::Resumed { epoch: 0 });
        m
    }

    fn signing_in() -> Machine {
        let mut m = Machine::new();
        m.apply(Event::ResumeRefused { epoch: 0 });
        m
    }

    fn sign_in_error_of(m: &Machine) -> Option<&str> {
        match m.phase() {
            Phase::SignIn { error, .. } => error.as_deref(),
            other => panic!("not signing in: {other:?}"),
        }
    }

    #[test]
    fn a_resumed_session_is_ready_and_a_refused_one_signs_in() {
        assert_eq!(ready().phase(), &Phase::Ready);
        assert_eq!(
            signing_in().phase(),
            &Phase::SignIn {
                error: None,
                busy: false
            }
        );
    }

    #[test]
    fn a_socket_that_never_opens_is_explained_by_the_probe() {
        let after = |probe| {
            let mut m = Machine::new();
            m.apply(Event::NotOpened { epoch: 0, probe });
            m.phase().clone()
        };
        assert_eq!(after(Probe::Status(404)), Phase::NotEnabled);
        let Phase::Unreachable { detail } = after(Probe::Status(405)) else {
            panic!("405 is enabled but unreachable");
        };
        assert!(
            detail.contains("allowed_origins") && detail.contains("https"),
            "{detail}"
        );
        assert!(matches!(
            after(Probe::Failed("offline".into())),
            Phase::Unreachable { detail } if detail.contains("offline")
        ));
    }

    #[test]
    fn a_revoked_close_signs_in_and_any_other_reconnects_with_backoff() {
        let mut m = ready();
        m.apply(Event::Closed {
            epoch: 0,
            reason: "idle_timeout".into(),
        });
        assert_eq!(m.phase(), &Phase::Connecting);
        assert_eq!((m.epoch(), m.delay_ms()), (1, 500));
        m.apply(Event::Closed {
            epoch: 1,
            reason: String::new(),
        });
        assert_eq!(m.delay_ms(), 1000);
        m.apply(Event::Resumed { epoch: 2 });
        assert_eq!((m.phase(), m.delay_ms()), (&Phase::Ready, 0));

        m.apply(Event::Closed {
            epoch: 2,
            reason: "revoked".into(),
        });
        assert_eq!(
            sign_in_error_of(&m),
            Some("Your session ended. Sign in again.")
        );
    }

    #[test]
    fn a_dropped_socket_keeps_the_screens_until_the_reconnect_settles() {
        let mut m = ready();
        m.apply(Event::Closed {
            epoch: 0,
            reason: "idle_timeout".into(),
        });
        assert!(m.reconnecting() && m.shows_screens());
        m.apply(Event::Closed {
            epoch: 1,
            reason: String::new(),
        });
        assert!(m.shows_screens(), "a second drop is still a reconnect");
        m.apply(Event::Resumed { epoch: 2 });
        assert!(!m.reconnecting() && m.shows_screens());

        m.apply(Event::Closed {
            epoch: 2,
            reason: String::new(),
        });
        m.apply(Event::NotOpened {
            epoch: 3,
            probe: Probe::Failed("offline".into()),
        });
        assert!(!m.reconnecting() && !m.shows_screens());

        assert!(
            !Machine::new().shows_screens(),
            "a first connect is not a reconnect"
        );
    }

    #[test]
    fn backoff_stops_growing_at_thirty_seconds() {
        let mut m = ready();
        for epoch in 0..40 {
            m.apply(Event::Closed {
                epoch,
                reason: String::new(),
            });
        }
        assert_eq!(m.delay_ms(), 30_000);
    }

    /// The socket opened before sign-in is replaced by a new one; its close
    /// (the server's auth deadline) must not disturb the new attempt.
    #[test]
    fn events_from_a_replaced_connection_are_ignored() {
        let mut m = signing_in();
        m.apply(Event::SignInSubmitted);
        m.apply(Event::SignInAnswered(SignInReply::Status {
            status: 204,
            retry_after: None,
        }));
        assert_eq!((m.phase(), m.epoch()), (&Phase::Connecting, 1));
        m.apply(Event::Closed {
            epoch: 0,
            reason: "auth_deadline".into(),
        });
        m.apply(Event::ResumeRefused { epoch: 0 });
        assert_eq!((m.phase(), m.epoch()), (&Phase::Connecting, 1));
        m.apply(Event::Resumed { epoch: 1 });
        assert_eq!(m.phase(), &Phase::Ready);
    }

    #[test]
    fn sign_in_failures_say_what_to_do() {
        let reply = |status, retry_after| {
            let mut m = signing_in();
            m.apply(Event::SignInSubmitted);
            assert_eq!(
                m.phase(),
                &Phase::SignIn {
                    error: None,
                    busy: true
                }
            );
            m.apply(Event::SignInAnswered(SignInReply::Status {
                status,
                retry_after,
            }));
            sign_in_error_of(&m).unwrap_or_default().to_owned()
        };
        assert_eq!(reply(401, None), "Wrong name or password.");
        assert!(reply(429, Some(30)).contains("30 s"));
        assert!(reply(429, None).contains("later"));
        assert!(reply(403, None).contains("allowed_origins"));
        assert!(reply(503, None).contains("503"));
    }

    #[test]
    fn a_call_refused_as_revoked_signs_in() {
        let mut m = ready();
        m.apply(Event::NeedsSignIn);
        assert!(sign_in_error_of(&m).is_some());
    }

    #[test]
    fn sign_out_signs_in_quietly_or_keeps_the_session_on_failure() {
        let mut m = ready();
        m.apply(Event::SignOutSubmitted);
        // The server closes the socket as revoked before the reply lands.
        m.apply(Event::Closed {
            epoch: 0,
            reason: "revoked".into(),
        });
        m.apply(Event::SignOutAnswered(SignOutReply::Status(204)));
        assert_eq!(sign_in_error_of(&m), None);

        let mut m = ready();
        m.apply(Event::SignOutSubmitted);
        m.apply(Event::SignOutAnswered(SignOutReply::Status(503)));
        assert_eq!(m.phase(), &Phase::Ready);
        assert!(m.notice().is_some_and(|n| n.contains("503")));
        assert!(!m.signing_out());
    }

    #[test]
    fn retry_reconnects_from_a_status_page_only() {
        let mut m = Machine::new();
        m.apply(Event::NotOpened {
            epoch: 0,
            probe: Probe::Status(404),
        });
        m.apply(Event::Retry);
        assert_eq!((m.phase(), m.epoch()), (&Phase::Connecting, 1));
        let mut m = ready();
        m.apply(Event::Retry);
        assert_eq!(m.phase(), &Phase::Ready);
    }
}
