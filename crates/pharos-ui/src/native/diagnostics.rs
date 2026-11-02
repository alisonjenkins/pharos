//! The gate on client diagnostics: whether this browser has opted in to
//! sending them, and the bounded hold for events raised before the session
//! opens. Platform-neutral so the gate runs under host tests; the browser
//! platform reads the choice from localStorage and does the submit.

// Only the browser platform submits; the host build compiles this for tests.
#![allow(dead_code)]

use pharos_client::api::ClientEvent;

/// The `localStorage` key holding this browser's diagnostics opt-in.
pub const OPT_IN_KEY: &str = "pharos.diagnostics.opt_in";
/// Events raised before the session opens; any past this are dropped.
pub const MAX_PENDING: usize = 32;

/// The stored opt-in. Only `"true"` opts in; absent, `"false"` and anything
/// else leave diagnostics off.
pub fn opt_in_from(raw: Option<&str>) -> bool {
    raw == Some("true")
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
    fn opt_in_reads_only_the_literal_true() {
        assert!(!opt_in_from(None));
        assert!(!opt_in_from(Some("false")));
        assert!(!opt_in_from(Some("garbage")));
        assert!(!opt_in_from(Some("")));
        assert!(opt_in_from(Some("true")));
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

    #[test]
    fn hold_before_install_is_bounded() {
        let mut sink: Sink<u32> = Sink::default();
        for _ in 0..MAX_PENDING + 5 {
            assert!(sink.report(true, event()).is_none());
        }
        assert_eq!(sink.pending_len(), MAX_PENDING);
    }
}
