//! The gate on client diagnostics: whether this user has opted in to
//! sending them, and the bounded hold for events raised before the session
//! opens. Platform-neutral so the gate runs under host tests. The session's
//! state (the opt-in as read from the server's `Prefs`, and the gate) lives
//! here too, so the browser and the TV app share one gate; each platform
//! spawns the submit on its own executor.

// Only the browser and the TV app submit; the host build compiles this for tests.
#![allow(dead_code)]

use std::cell::RefCell;

use pharos_client::api::{ClientEvent, NativeApi};
use pharos_client::CallInfo;

/// The `Prefs` key holding this user's diagnostics opt-in.
pub const OPT_IN_PREF: &str = "diagnostics.opt_in";
/// The `localStorage` key older builds kept the opt-in under. It is removed
/// and never read: a stored `true` must not opt the user in on the server.
pub const LEGACY_OPT_IN_KEY: &str = "pharos.diagnostics.opt_in";
/// Events raised before the session opens; any past this are dropped.
pub const MAX_PENDING: usize = 32;

/// The stored opt-in. Only `true` opts in; absent, `false` and anything
/// else leave diagnostics off.
pub fn opt_in_from(raw: Option<&[u8]>) -> bool {
    raw == Some(b"true".as_slice())
}

/// The value the toggle shows once a save resolves: the value the server
/// stored on success, the value it held before the attempt on failure.
pub fn shown_after_save(saved: Result<bool, String>, server_before: bool) -> bool {
    match saved {
        Ok(stored) => stored,
        Err(_) => server_before,
    }
}

/// Holds events raised before the session opens and decides, per call,
/// whether an event is submitted now. The opt-in is passed in at each call,
/// so turning it off stops the next submit.
pub struct Sink<A> {
    api: Option<A>,
    pending: Vec<ClientEvent>,
}

impl<A> Default for Sink<A> {
    fn default() -> Self {
        Self {
            api: None,
            pending: Vec::new(),
        }
    }
}

impl<A: Clone> Sink<A> {
    /// Starts submitting through `api`. Returns the held events to send now;
    /// when opted out they are dropped instead.
    pub fn install(&mut self, api: A, opted_in: bool) -> Vec<ClientEvent> {
        self.api = Some(api);
        let held = std::mem::take(&mut self.pending);
        if opted_in {
            held
        } else {
            Vec::new()
        }
    }

    /// The submit to make now for `event`, if any. Opted out, the event is
    /// dropped and not held. Before the session opens it is held, up to
    /// [`MAX_PENDING`].
    pub fn report(&mut self, opted_in: bool, event: ClientEvent) -> Option<(A, ClientEvent)> {
        if !opted_in {
            return None;
        }
        match &self.api {
            Some(api) => Some((api.clone(), event)),
            None => {
                if self.pending.len() < MAX_PENDING {
                    self.pending.push(event);
                }
                None
            }
        }
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

/// The signed-in session's diagnostics: the gate, and the user's opt-in as
/// last read from or written to `Prefs`. Off until read, and again after
/// sign-out.
#[derive(Default)]
struct Telemetry {
    sink: Sink<NativeApi>,
    failure_logged: bool,
    opted_in: bool,
    /// Bumped by each [`forget_opt_in`]: a read or an install begun under an
    /// earlier session must not land in this one.
    session: u64,
    /// Bumped as each save of the opt-in starts: a read or an earlier save
    /// that lands after a later save began must not undo it.
    saves: u64,
}

thread_local! {
    static TELEMETRY: RefCell<Telemetry> = RefCell::new(Telemetry::default());
}

/// Whether the signed-in user has opted in to sending diagnostics, as
/// last read from the server. Off before sign-in and until the read lands.
pub fn opted_in() -> bool {
    TELEMETRY.with(|t| t.borrow().opted_in)
}

fn remember_opt_in(on: bool) {
    TELEMETRY.with(|t| t.borrow_mut().opted_in = on);
}

/// Forgets the session: the opt-in, the connection events went through
/// and any events held for it, so nothing is sent after sign-out until the
/// next user's choice is read, and nothing raised before reaches that user's
/// session. A read or an install still under way is discarded when it lands.
pub fn forget_opt_in() {
    TELEMETRY.with(|t| {
        let mut t = t.borrow_mut();
        let session = t.session.wrapping_add(1);
        *t = Telemetry {
            session,
            ..Telemetry::default()
        };
    });
}

/// The session a read or an install is begun under; see [`forget_opt_in`].
pub fn session() -> u64 {
    TELEMETRY.with(|t| t.borrow().session)
}

fn prefs_call(action: &str) -> CallInfo {
    CallInfo {
        ui_route: "telemetry".into(),
        ui_action: action.into(),
        ..Default::default()
    }
}

/// Reads the user's opt-in from `Prefs` and remembers it.
pub async fn load_opted_in(api: &NativeApi) -> Result<bool, String> {
    load_opted_in_since(api, session()).await
}

/// [`load_opted_in`] for a read begun under session `begun` (from
/// [`session`]), taken where `api` was: a task that only starts after a
/// sign-out must not remember the previous user's opt-in for the next.
/// An unreadable opt-in counts as off from then on, not only for the held
/// events: a reconnect keeps the last session's choice otherwise.
pub async fn load_opted_in_since(api: &NativeApi, begun: u64) -> Result<bool, String> {
    let ticket = Ticket {
        session: begun,
        saves: saves(),
    };
    match api.pref(&prefs_call("opt_in_read"), OPT_IN_PREF).await {
        Ok(raw) => remember_if_current(ticket, opt_in_from(raw.as_deref())),
        Err(e) => {
            // Off whatever happened since: a failed read never turns it on.
            let _ = remember_if_current(ticket, false);
            Err(e.to_string())
        }
    }
}

/// When a read or a save of the opt-in began: its result is remembered only
/// if no sign-out and no later save came since.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Ticket {
    session: u64,
    saves: u64,
}

fn saves() -> u64 {
    TELEMETRY.with(|t| t.borrow().saves)
}

/// Remembers `on` if `ticket` is still the latest: no sign-out and no save
/// begun since.
fn remember_if_current(ticket: Ticket, on: bool) -> Result<bool, String> {
    let now = Ticket {
        session: session(),
        saves: saves(),
    };
    if now.session != ticket.session {
        return Err("signed out while the opt-in was read".to_owned());
    }
    if now.saves != ticket.saves {
        return Err("the opt-in was saved while it was read".to_owned());
    }
    remember_opt_in(on);
    Ok(on)
}

/// Writes the user's opt-in to `Prefs`; remembered only once the server
/// has stored it.
pub async fn set_opted_in(api: &NativeApi, on: bool) -> Result<(), String> {
    let ticket = begin_save();
    let value: &[u8] = if on { b"true" } else { b"false" };
    api.set_pref(&prefs_call("opt_in_write"), OPT_IN_PREF, value)
        .await
        .map_err(|e| e.to_string())?;
    remember_if_current(ticket, on)
        .map(|_| ())
        .map_err(|_| "signed out or saved again while the opt-in was saved".to_owned())
}

/// Starts a save: reads and saves begun before it no longer land.
fn begin_save() -> Ticket {
    TELEMETRY.with(|t| {
        let mut t = t.borrow_mut();
        t.saves = t.saves.wrapping_add(1);
        Ticket {
            session: t.session,
            saves: t.saves,
        }
    })
}

/// Starts submitting through `api`: the held events to send now, none when
/// opted out. An install begun under an earlier session (`begun`, from
/// [`session`]) does nothing.
pub fn install(api: NativeApi, opted_in: bool, begun: u64) -> Vec<ClientEvent> {
    TELEMETRY.with(|t| {
        let mut t = t.borrow_mut();
        if t.session != begun {
            return Vec::new();
        }
        t.sink.install(api, opted_in)
    })
}

/// The submit to make now for `event`, if any (see [`Sink::report`]).
pub fn report(event: ClientEvent) -> Option<(NativeApi, ClientEvent)> {
    let opted = opted_in();
    TELEMETRY.with(|t| t.borrow_mut().sink.report(opted, event))
}

/// Submits `events`, unless the opt-in was turned off since they were
/// raised. A failure is returned once per session: a dead socket fails
/// every later batch the same way.
pub async fn submit(api: &NativeApi, events: &[ClientEvent]) -> Option<String> {
    if !opted_in() {
        return None;
    }
    let e = api
        .submit_events(&prefs_call("submit"), events)
        .await
        .err()?;
    let first = TELEMETRY.with(|t| !std::mem::replace(&mut t.borrow_mut().failure_logged, true));
    first.then(|| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pharos_client::api::{ClientEventKind, ClientLevel};

    fn event() -> ClientEvent {
        ClientEvent {
            kind: ClientEventKind::PlaybackError,
            level: ClientLevel::Error,
            route: "/player".to_owned(),
            fields: Vec::new(),
        }
    }

    #[test]
    fn sign_out_forgets_the_opt_in_and_the_held_events_and_starts_a_new_session() {
        remember_opt_in(true);
        assert!(report(event()).is_none(), "no connection yet: held");
        let held = || TELEMETRY.with(|t| t.borrow().sink.pending_len());
        assert_eq!(held(), 1);
        let before = session();
        forget_opt_in();
        assert!(!opted_in());
        assert_eq!(held(), 0, "a held event survived sign-out");
        assert_ne!(
            session(),
            before,
            "a read begun before sign-out would still land"
        );
    }

    #[test]
    fn opt_in_reads_only_the_literal_true() {
        assert!(!opt_in_from(None), "absent must be off");
        assert!(!opt_in_from(Some(b"false")));
        assert!(!opt_in_from(Some(b"garbage")));
        assert!(!opt_in_from(Some(b"")));
        assert!(!opt_in_from(Some(b"True")));
        assert!(opt_in_from(Some(b"true")));
    }

    #[test]
    fn toggle_shows_what_the_server_holds_once_a_save_resolves() {
        assert!(shown_after_save(Ok(true), false), "a stored on must show");
        assert!(!shown_after_save(Ok(false), true), "a stored off must show");
        assert!(
            !shown_after_save(Err("offline".to_owned()), false),
            "a failed save of on must show the server's off"
        );
        assert!(
            shown_after_save(Err("offline".to_owned()), true),
            "a failed save of off must show the server's on"
        );
    }

    #[test]
    fn opted_out_submits_nothing_and_holds_nothing() {
        let mut sink: Sink<u32> = Sink::default();
        assert!(sink.report(false, event()).is_none());
        assert_eq!(sink.pending_len(), 0, "held an event while opted out");
        assert!(sink.install(7, false).is_empty());
        assert!(
            sink.report(false, event()).is_none(),
            "submitted while opted out after install"
        );
        assert_eq!(sink.pending_len(), 0, "held an event after install");
    }

    #[test]
    fn opted_out_drops_events_held_before_install() {
        let mut sink: Sink<u32> = Sink::default();
        assert!(sink.report(true, event()).is_none());
        assert_eq!(sink.pending_len(), 1);
        assert!(
            sink.install(7, false).is_empty(),
            "sent held events after opt-out"
        );
        assert_eq!(sink.pending_len(), 0);
    }

    #[test]
    fn opted_in_holds_until_install_then_submits_each_event_once() {
        let mut sink: Sink<u32> = Sink::default();
        assert!(sink.report(true, event()).is_none());
        assert_eq!(sink.pending_len(), 1);
        assert_eq!(sink.install(7, true).len(), 1);
        assert_eq!(sink.pending_len(), 0);
        let submitted = sink.report(true, event());
        assert!(
            matches!(submitted, Some((7, _))),
            "expected one submit through api 7"
        );
    }

    /// An opt-in read begun before a sign-out, even one whose task only
    /// starts after it, is not remembered for the next user.
    #[test]
    fn an_opt_in_read_from_before_sign_out_is_not_remembered() {
        forget_opt_in();
        let begun = Ticket {
            session: session(),
            saves: saves(),
        };
        forget_opt_in();
        assert!(remember_if_current(begun, true).is_err());
        assert!(!opted_in());
        let now = Ticket {
            session: session(),
            saves: saves(),
        };
        assert_eq!(remember_if_current(now, true), Ok(true));
        assert!(opted_in());
        forget_opt_in();
    }

    /// A read begun before a save of the opt-out lands after it and is
    /// dropped, so the saved choice stands; so is an earlier save landing
    /// after a later one.
    #[test]
    fn a_read_or_save_from_before_a_later_save_does_not_undo_it() {
        forget_opt_in();
        let read = Ticket {
            session: session(),
            saves: saves(),
        };
        let first = begin_save();
        let second = begin_save();
        assert_eq!(remember_if_current(second, false), Ok(false));
        assert!(remember_if_current(read, true).is_err());
        assert!(remember_if_current(first, true).is_err());
        assert!(!opted_in());
        forget_opt_in();
    }

    #[test]
    fn hold_before_install_is_bounded() {
        let mut sink: Sink<u32> = Sink::default();
        for _ in 0..MAX_PENDING + 5 {
            assert!(sink.report(true, event()).is_none());
        }
        assert_eq!(sink.pending_len(), MAX_PENDING);
    }
}
