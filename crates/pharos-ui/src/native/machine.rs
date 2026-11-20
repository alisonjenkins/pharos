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
    /// Signed out in this process, but the device could not forget the
    /// sign-in (its token file stayed), for the reason given.
    NotSaved(String),
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
    /// As `NeedsSignIn`, for a call sent in `session` ([`Machine::session`]
    /// when it was sent): it ends only that session, never one signed in
    /// since.
    SessionNeedsSignIn {
        session: u32,
    },
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

/// What the host runs when a signed-in session ends (the app leaves Ready
/// for sign-in), whichever event caused it. Compares equal to any other, so
/// it does not take part in the machine's equality.
#[derive(Clone, Copy, Default)]
struct SessionEnd(Option<fn()>);

impl std::fmt::Debug for SessionEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "SessionEnd(set)"
        } else {
            "SessionEnd(none)"
        })
    }
}

impl PartialEq for SessionEnd {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for SessionEnd {}

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
    /// A session has been Ready and has not ended since: set on Ready,
    /// cleared only when the app shows sign-in. Kept apart from the phase
    /// and `reconnecting`, which a failed open or a retry clears, so a
    /// revoked close after such a detour still ends the session.
    had_session: bool,
    /// A non-blocking message for the signed-in shell, e.g. a failed
    /// sign-out.
    notice: Option<String>,
    /// Why the last attempt failed, while connecting again after a failure
    /// that may clear by itself (server down, network gone).
    waiting: Option<String>,
    /// Counts the sessions published: bumped on every resume that makes the
    /// app Ready, a reconnect's included, so a refusal names the one its
    /// call was sent in.
    session: u32,
    /// Counts the sign-ins started (submitted or answered): a sign-out's
    /// reply carries the count from when it was sent, and one that a newer
    /// sign-in overtook is ignored ([`Machine::sign_out_reply_is_current`]).
    sign_ins: u32,
    on_session_end: SessionEnd,
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
            had_session: false,
            notice: None,
            waiting: None,
            session: 0,
            sign_ins: 0,
            on_session_end: SessionEnd(None),
        }
    }

    /// Runs `hook` whenever a signed-in session ends: the app leaves Ready
    /// for sign-in, by a rejected request, a revoked socket or a sign-out.
    /// The one place that happens, so no caller can skip it.
    #[must_use]
    pub fn with_session_end(mut self, hook: fn()) -> Self {
        self.on_session_end = SessionEnd(Some(hook));
        self
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    /// The current connection attempt; events carry it back.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// The session signed in now (or last): a call captures it when it is
    /// sent, so its refusal ends only that session.
    pub fn session(&self) -> u32 {
        self.session
    }

    /// The sign-ins started so far: a sign-out captures it when it is sent.
    pub fn sign_ins(&self) -> u32 {
        self.sign_ins
    }

    /// Whether a sign-out's reply, sent when [`Machine::sign_ins`] was
    /// `sent_at`, still answers for the session it ended: no sign-in has
    /// started since. One a newer sign-in overtook (B signed in after A's
    /// revoked socket showed sign-in) must not end B's session.
    pub fn sign_out_reply_is_current(&self, sent_at: u32) -> bool {
        self.sign_ins == sent_at
    }

    /// Whether attempt `epoch`'s resume would make the app Ready: it is the
    /// current attempt and the app is still connecting. A sign-out (or any
    /// other move to a screen) that landed while the attempt was in flight
    /// refuses it, so the attempt publishes no session.
    pub fn accepts_resume(&self, epoch: u32) -> bool {
        epoch == self.epoch && self.phase == Phase::Connecting
    }

    /// Whether a session has been Ready since the last sign-in screen: a
    /// refusal then ends it through the session-end hook.
    pub fn had_session(&self) -> bool {
        self.had_session
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub fn signing_out(&self) -> bool {
        self.signing_out
    }

    /// Why the server could not be reached, while the app keeps trying.
    pub fn waiting(&self) -> Option<&str> {
        self.waiting.as_deref()
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
        // A session ends whenever the app shows sign-in after it was
        // Ready, whatever failed opens or retries came in between.
        let was_ready = self.had_session;
        self.step(event);
        if was_ready && matches!(self.phase, Phase::SignIn { .. }) {
            if let Some(hook) = self.on_session_end.0 {
                hook();
            }
        }
    }

    fn step(&mut self, event: Event) {
        let current = |epoch: u32| epoch == self.epoch;
        // An attempt's open result counts only while it is still connecting:
        // a sign-out (or any screen) that landed meanwhile ended it, without
        // a new epoch.
        let open = |epoch: u32| epoch == self.epoch && self.phase == Phase::Connecting;
        match event {
            Event::Resumed { epoch } if self.accepts_resume(epoch) => {
                self.attempt = 0;
                self.waiting = None;
                self.phase = Phase::Ready;
                // Every published session gets its own refusal id, a
                // reconnect's too: it may carry another token than the one
                // a pending call was sent with, and that call's refusal must
                // not end it. `had_session` alone spans the outage.
                self.session = self.session.wrapping_add(1);
                self.had_session = true;
            }
            Event::ResumeRefused { epoch } if open(epoch) => self.sign_in(None),
            // The server is up but could not answer (its database may be
            // down): treat it like an outage.
            Event::ResumeFailed { epoch, error } if open(epoch) => {
                self.wait_and_retry(format!("The server would not resume the session: {error}"));
            }
            Event::NotOpened { epoch, probe } if open(epoch) => {
                if may_clear(&probe) {
                    self.wait_and_retry(probe_detail(&probe));
                } else {
                    self.phase = not_opened(&probe);
                }
            }
            // A session the sign-in screen already ended has no socket to
            // reconnect, and its warning stays.
            Event::Closed { epoch, reason }
                if current(epoch) && !matches!(self.phase, Phase::SignIn { .. }) =>
            {
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
            // The current session's refusal ends it while it lasts, Ready or
            // reconnecting; a stale one never does.
            Event::SessionNeedsSignIn { session }
                if self.had_session && session == self.session =>
            {
                self.sign_in(Some("Your session ended. Sign in again.".into()));
            }
            Event::SignInSubmitted => {
                if matches!(self.phase, Phase::SignIn { .. }) {
                    self.sign_ins = self.sign_ins.wrapping_add(1);
                }
                if let Phase::SignIn { busy, error } = &mut self.phase {
                    *busy = true;
                    *error = None;
                }
            }
            Event::SignInAnswered(reply) => {
                if matches!(self.phase, Phase::SignIn { .. }) {
                    self.sign_ins = self.sign_ins.wrapping_add(1);
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
            Event::SignOutAnswered(SignOutReply::NotSaved(why)) => {
                // Signed out here all the same; the sign-in screen says the
                // device kept the sign-in, so the user can sign out again.
                self.sign_in(Some(format!(
                    "Signed out, but this device could not forget the sign-in ({why}). \
                     It may come back when the app restarts: sign in and sign out again to retry."
                )));
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
            self.waiting = None;
        }
    }

    /// Connects again after the backoff, keeping the screens if they were
    /// up: the server may be restarting or the network back soon, and a TV
    /// may have nobody watching to press "Try again".
    fn wait_and_retry(&mut self, why: String) {
        let was_reconnecting = self.reconnecting;
        self.attempt = self.attempt.saturating_add(1);
        self.connect();
        self.reconnecting = was_reconnecting;
        self.waiting = Some(why);
    }

    fn connect(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.notice = None;
        self.phase = Phase::Connecting;
    }

    fn sign_in(&mut self, error: Option<String>) {
        self.signing_out = false;
        self.had_session = false;
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
        probe => Phase::Unreachable {
            detail: probe_detail(probe),
        },
    }
}

/// Whether a failed open may clear by itself: the server unreachable, or
/// answering with an overload or server error.
fn may_clear(probe: &Probe) -> bool {
    match probe {
        Probe::Failed(_) => true,
        Probe::Status(status) => *status >= 500 || matches!(status, 408 | 429),
    }
}

fn probe_detail(probe: &Probe) -> String {
    match probe {
        Probe::Status(status) => {
            format!("The server answered HTTP {status} when asked about sign-in.")
        }
        Probe::Failed(error) if error.contains(NOT_VALID_YET) => format!(
            "Could not reach the server: {error}. The server's certificate is dated after \
             this device's clock, so check the device's date and time."
        ),
        Probe::Failed(error) => format!("Could not reach the server: {error}"),
    }
}

/// How rustls words a certificate whose notBefore is after the local clock;
/// a TV that lost power can boot with its clock years behind.
const NOT_VALID_YET: &str = "certificate not valid yet";

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
        SignOutReply::NotSaved(_) => None,
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
            after(Probe::Status(403)),
            Phase::Unreachable { detail } if detail.contains("403")
        ));
    }

    #[test]
    fn a_server_that_is_down_is_retried_with_backoff_and_the_reason_shown() {
        for probe in [
            Probe::Failed("offline".into()),
            Probe::Status(503),
            Probe::Status(429),
        ] {
            let mut m = Machine::new();
            m.apply(Event::NotOpened {
                epoch: 0,
                probe: probe.clone(),
            });
            assert_eq!(m.phase(), &Phase::Connecting, "{probe:?}");
            assert_eq!((m.epoch(), m.delay_ms()), (1, 500), "{probe:?}");
            assert!(m.waiting().is_some(), "{probe:?}");
            assert!(!m.shows_screens(), "a first connect is not a reconnect");
        }

        let mut m = Machine::new();
        for epoch in 0..3 {
            m.apply(Event::NotOpened {
                epoch,
                probe: Probe::Failed("offline".into()),
            });
        }
        assert_eq!(m.delay_ms(), 2000);
        assert!(m.waiting().is_some_and(|why| why.contains("offline")));
        m.apply(Event::ResumeFailed {
            epoch: 3,
            error: "database unavailable".into(),
        });
        assert_eq!((m.phase(), m.delay_ms()), (&Phase::Connecting, 4000));
        assert!(m.waiting().is_some_and(|why| why.contains("database")));
        m.apply(Event::ResumeRefused { epoch: 4 });
        assert_eq!(m.waiting(), None);
        assert!(matches!(m.phase(), Phase::SignIn { .. }));
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

    static SESSIONS_ENDED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn count_end() {
        SESSIONS_ENDED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    static DETOUR_ENDED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn count_detour_end() {
        DETOUR_ENDED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// A drop, a failed open that does not clear by itself (405), a retry,
    /// then a revoked close: the session the user was signed into ends and
    /// the hook runs, though the detour cleared the phase and the
    /// reconnecting flag. A second sign-in screen runs it no more.
    #[test]
    fn a_revoked_close_after_a_failed_open_and_a_retry_ends_the_session() {
        use std::sync::atomic::Ordering;
        let mut m = Machine::new().with_session_end(count_detour_end);
        m.apply(Event::Resumed { epoch: 0 });
        m.apply(Event::Closed {
            epoch: 0,
            reason: "idle_timeout".into(),
        });
        m.apply(Event::NotOpened {
            epoch: 1,
            probe: Probe::Status(405),
        });
        assert!(
            matches!(m.phase, Phase::Unreachable { .. }),
            "{:?}",
            m.phase
        );
        m.apply(Event::Retry);
        let epoch = m.epoch();
        m.apply(Event::Closed {
            epoch,
            reason: REVOKED.into(),
        });
        assert!(matches!(m.phase, Phase::SignIn { .. }), "{:?}", m.phase);
        assert_eq!(
            DETOUR_ENDED.load(Ordering::SeqCst),
            1,
            "the signed-in session ended without its hook"
        );
        m.apply(Event::SignInAnswered(SignInReply::Status {
            status: 401,
            retry_after: None,
        }));
        assert_eq!(DETOUR_ENDED.load(Ordering::SeqCst), 1, "the hook ran twice");
    }

    /// The session-end hook runs when a session revoked after a dropped
    /// socket ends (the screens were still up), and never for an ordinary
    /// outage that reconnects. Run as one test: the count is a static.
    #[test]
    fn a_revoked_close_after_a_drop_ends_the_session_but_an_outage_does_not() {
        use std::sync::atomic::Ordering;
        let count = || SESSIONS_ENDED.load(Ordering::SeqCst);
        let start = count();
        let mut m = Machine::new().with_session_end(count_end);
        m.apply(Event::Resumed { epoch: 0 });
        // An outage: drop, the server down for a while, then back.
        m.apply(Event::Closed {
            epoch: 0,
            reason: "idle_timeout".into(),
        });
        m.apply(Event::NotOpened {
            epoch: 1,
            probe: Probe::Failed("offline".into()),
        });
        assert!(m.shows_screens());
        m.apply(Event::Resumed { epoch: 2 });
        assert_eq!(count(), start, "an outage ended the session");
        // A drop, then the reconnect finds the session revoked.
        m.apply(Event::Closed {
            epoch: 2,
            reason: "idle_timeout".into(),
        });
        assert!(m.reconnecting());
        m.apply(Event::Closed {
            epoch: 3,
            reason: REVOKED.into(),
        });
        assert!(matches!(m.phase, Phase::SignIn { .. }), "{:?}", m.phase);
        assert_eq!(count(), start + 1, "the revoked session's hook did not run");
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
        assert!(
            m.reconnecting() && m.shows_screens(),
            "a server that is down for a while keeps the screens up"
        );
        m.apply(Event::NotOpened {
            epoch: 4,
            probe: Probe::Status(405),
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

    /// A refusal for a call an earlier session sent does not end the one
    /// signed in now; the current session's own refusal still does, and
    /// runs the session-end hook.
    #[test]
    fn a_refusal_from_an_earlier_session_leaves_the_current_one() {
        thread_local! {
            static ENDED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        }
        fn hook() {
            ENDED.with(|n| n.set(n.get() + 1));
        }
        let mut m = Machine::new().with_session_end(hook);
        m.apply(Event::Resumed { epoch: 0 });
        let first = m.session();
        // Signed out and in again while the first session's call was out.
        m.apply(Event::SignOutSubmitted);
        m.apply(Event::SignOutAnswered(SignOutReply::Status(204)));
        m.apply(Event::SignInAnswered(SignInReply::Status {
            status: 204,
            retry_after: None,
        }));
        let epoch = m.epoch();
        m.apply(Event::Resumed { epoch });
        assert_eq!(m.phase(), &Phase::Ready);
        assert_ne!(m.session(), first);
        ENDED.with(|n| n.set(0));
        m.apply(Event::SessionNeedsSignIn { session: first });
        assert_eq!(
            m.phase(),
            &Phase::Ready,
            "an old call ended the new session"
        );
        assert_eq!(ENDED.with(std::cell::Cell::get), 0);
        // The current session's own refusal ends it.
        let now = m.session();
        m.apply(Event::SessionNeedsSignIn { session: now });
        assert!(sign_in_error_of(&m).is_some());
        assert_eq!(ENDED.with(std::cell::Cell::get), 1);
    }

    /// CR 4638660965: a refusal from the current session lands while the
    /// socket is reconnecting (not Ready): it still ends that session and
    /// runs the hook. A stale one still does not.
    #[test]
    fn a_current_refusal_during_a_reconnect_ends_the_session() {
        thread_local! {
            static ENDED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        }
        fn hook() {
            ENDED.with(|n| n.set(n.get() + 1));
        }
        let mut m = Machine::new().with_session_end(hook);
        m.apply(Event::Resumed { epoch: 0 });
        let now = m.session();
        m.apply(Event::Closed {
            epoch: 0,
            reason: "gone".into(),
        });
        assert_eq!(m.phase(), &Phase::Connecting);
        m.apply(Event::SessionNeedsSignIn {
            session: now.wrapping_sub(1),
        });
        assert_eq!(m.phase(), &Phase::Connecting, "a stale refusal acted");
        m.apply(Event::SessionNeedsSignIn { session: now });
        assert!(sign_in_error_of(&m).is_some(), "the refusal was dropped");
        assert_eq!(ENDED.with(std::cell::Cell::get), 1);
    }

    /// Review 5987743326 (machine.rs:301): a socket close at the current
    /// epoch after a sign-out that could not forget the token does not
    /// reconnect or replace the sign-in screen's warning.
    #[test]
    fn a_close_after_the_sign_in_screen_is_ignored() {
        let mut m = ready();
        m.apply(Event::SignOutSubmitted);
        m.apply(Event::SignOutAnswered(SignOutReply::NotSaved(
            "disk".into(),
        )));
        let warning = sign_in_error_of(&m).map(str::to_owned);
        assert!(warning.is_some());
        let epoch = m.epoch();
        m.apply(Event::Closed {
            epoch,
            reason: "gone".into(),
        });
        assert_eq!(
            sign_in_error_of(&m),
            warning.as_deref(),
            "the close reconnected"
        );
        m.apply(Event::Closed {
            epoch,
            reason: REVOKED.into(),
        });
        assert_eq!(
            sign_in_error_of(&m),
            warning.as_deref(),
            "the warning was replaced"
        );
    }

    /// A reconnect may publish another token (B) than the one a pending
    /// call was sent with (A): A's delayed refusal leaves B's session and
    /// its token alone, and B's own refusal still ends it.
    #[test]
    fn a_refusal_sent_before_a_reconnect_leaves_the_replacement_session() {
        thread_local! {
            static ENDED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        }
        fn hook() {
            ENDED.with(|n| n.set(n.get() + 1));
        }
        let mut m = Machine::new().with_session_end(hook);
        m.apply(Event::Resumed { epoch: 0 });
        let a = m.session();
        m.apply(Event::Closed {
            epoch: 0,
            reason: "gone".into(),
        });
        assert!(m.had_session(), "the outage forgot the session");
        let epoch = m.epoch();
        m.apply(Event::Resumed { epoch });
        let b = m.session();
        assert_ne!(b, a, "the replacement reused the refusal id");
        m.apply(Event::SessionNeedsSignIn { session: a });
        assert_eq!(m.phase(), &Phase::Ready, "A's refusal ended B's session");
        assert_eq!(
            ENDED.with(std::cell::Cell::get),
            0,
            "B's token was cleaned up"
        );
        m.apply(Event::SessionNeedsSignIn { session: b });
        assert!(sign_in_error_of(&m).is_some());
        assert_eq!(ENDED.with(std::cell::Cell::get), 1);
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

    /// A sign-out that lands while a reconnect is in flight wins: that
    /// attempt's resume is refused, and the app stays on sign-in.
    #[test]
    fn a_resume_after_a_sign_out_won_is_refused() {
        let mut m = ready();
        m.apply(Event::SignOutSubmitted);
        m.apply(Event::Closed {
            epoch: 0,
            reason: "network".into(),
        });
        let attempt = m.epoch();
        assert!(m.accepts_resume(attempt));
        m.apply(Event::SignOutAnswered(SignOutReply::Status(204)));
        assert!(
            !m.accepts_resume(attempt),
            "a won sign-out still accepts the attempt"
        );
        m.apply(Event::Resumed { epoch: attempt });
        assert_eq!(
            sign_in_error_of(&m),
            None,
            "the attempt resumed over the sign-out"
        );
    }

    /// A sign-out the device could not save signs out all the same, and
    /// the sign-in screen says the device kept the sign-in and how to
    /// retry; the session-end hook runs.
    #[test]
    fn a_sign_out_not_saved_signs_out_and_says_so() {
        let mut m = ready();
        m.apply(Event::SignOutSubmitted);
        m.apply(Event::SignOutAnswered(SignOutReply::NotSaved(
            "permission denied".into(),
        )));
        let error = sign_in_error_of(&m).unwrap_or_default();
        assert!(
            error.contains("could not forget the sign-in (permission denied)")
                && error.contains("sign out again"),
            "{error}"
        );
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

    #[test]
    fn a_certificate_from_the_future_points_at_the_clock() {
        let mut m = Machine::new();
        m.apply(Event::NotOpened {
            epoch: 0,
            probe: Probe::Failed(
                "connect wss://example/pharos/rpc: IO error: invalid peer certificate: \
                 certificate not valid yet: verification time 1749513733 (UNIX)"
                    .into(),
            ),
        });
        assert_eq!(m.phase(), &Phase::Connecting, "it still retries");
        assert!(
            m.waiting()
                .is_some_and(|why| why.contains("date and time") && why.contains("not valid yet")),
            "{:?}",
            m.waiting()
        );
    }

    #[test]
    fn other_failures_do_not_blame_the_clock() {
        let detail = probe_detail(&Probe::Failed("offline".into()));
        assert!(!detail.contains("date and time"), "{detail}");
    }
}
