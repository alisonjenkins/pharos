//! `Events.subscribe` from the client side (017 US2, T047, ADR-0094): the
//! sink that reads each pushed batch, and the cursor that lets a new
//! connection resume where the last one stopped.

use std::cell::Cell;
use std::rc::Rc;

use pharos_api::pharos_capnp::{event, event_sink, item_id, session, subscribe_result};
use pharos_api::{server, Connection};

use crate::api::ItemRef;
use crate::{CallInfo, ClientError};

/// One pushed event, in plain Rust types.
#[derive(Debug, Clone, PartialEq)]
pub enum PushEvent {
    /// The signed-in user's state on `id` changed, on any device.
    UserData {
        id: ItemRef,
        played: bool,
        favorite: bool,
        position_ms: u64,
        play_count: u32,
    },
    /// The library changed; all three empty means "something changed".
    Library {
        added: Vec<ItemRef>,
        updated: Vec<ItemRef>,
        removed: Vec<ItemRef>,
    },
    /// A server task (a library scan) moved; `progress` is 0.0 to 1.0.
    /// `state` is `running`, `done`, `partial`, `failed` or `cancelled`.
    Task {
        id: String,
        progress: f32,
        state: String,
        /// What a finished scan did; all zero while it runs, for other
        /// tasks, and from a server older than schemaVersion 40.
        counts: TaskCounts,
    },
    /// Events were missed: everything cached may be stale.
    Resync,
}

/// What a finished library scan did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TaskCounts {
    pub added: u32,
    pub changed: u32,
    pub removed: u32,
    /// Folders the scan could not read.
    pub failed_roots: u32,
}

/// The last `seq` this client received, shared across connections so a
/// resubscribe asks only for what it missed. 0 is "nothing yet".
#[derive(Debug, Clone, Default)]
pub struct EventCursor(Rc<Cell<u64>>);

impl EventCursor {
    pub fn seq(&self) -> u64 {
        self.0.get()
    }

    fn advance(&self, seq: u64) {
        if seq > self.0.get() {
            self.0.set(seq);
        }
    }

    /// A resync carries the head the server resumes from, which may be below
    /// a cursor from another server run.
    fn reset(&self, seq: u64) {
        self.0.set(seq);
    }
}

/// A live subscription. Dropping it unsubscribes.
pub struct EventSubscription {
    _cap: capnp::capability::Client,
}

/// Subscribes `session` to `topics` (empty is all of them), resuming after
/// `cursor`. `on_events` runs once per pushed batch, in order, after the
/// cursor has moved past it.
pub async fn subscribe(
    session: &session::Client,
    call: &CallInfo,
    topics: &[&str],
    cursor: &EventCursor,
    on_events: impl Fn(Vec<PushEvent>) + 'static,
) -> Result<EventSubscription, ClientError> {
    let events = session.events_request().send().pipeline.get_events();
    let mut req = events.subscribe_request();
    {
        let mut p = req.get();
        call.write(p.reborrow().init_ctx());
        let sink = Sink {
            cursor: cursor.clone(),
            on_events,
        };
        p.set_sink(server::event_sink::client(sink, &Connection::new()));
        let mut list = p
            .reborrow()
            .init_topics(u32::try_from(topics.len()).unwrap_or(u32::MAX));
        for (i, topic) in (0u32..).zip(topics) {
            list.set(i, *topic);
        }
        p.set_after_seq(cursor.seq());
    }
    let reply = req.send().promise.await?;
    match reply
        .get()?
        .get_result()?
        .which()
        .map_err(capnp::Error::from)?
    {
        subscribe_result::Subscription(cap) => Ok(EventSubscription { _cap: cap?.client }),
        subscribe_result::Error(e) => Err(ClientError::from_api(e?)),
    }
}

struct Sink<F> {
    cursor: EventCursor,
    on_events: F,
}

impl<F: Fn(Vec<PushEvent>) + 'static> server::event_sink::Handler for Sink<F> {
    async fn push(
        self: Rc<Self>,
        params: event_sink::PushParams,
        results: &mut event_sink::PushResults,
    ) -> Result<(), capnp::Error> {
        let list = params.get()?.get_events()?;
        let mut batch = Vec::with_capacity(list.len() as usize);
        for e in list {
            let seq = e.get_seq();
            let event = read_event(e)?;
            if event == PushEvent::Resync {
                self.cursor.reset(seq);
            } else {
                self.cursor.advance(seq);
            }
            batch.push(event);
        }
        (self.on_events)(batch);
        results.get().init_result().set_ok(());
        Ok(())
    }
}

fn read_ref(id: item_id::Reader<'_>) -> Result<ItemRef, capnp::Error> {
    Ok(match id.which()? {
        item_id::Real(n) => ItemRef::Real(n),
        item_id::Synth(n) => ItemRef::Synth(n),
    })
}

fn read_refs(
    list: capnp::struct_list::Reader<'_, item_id::Owned>,
) -> Result<Vec<ItemRef>, capnp::Error> {
    list.iter().map(read_ref).collect()
}

fn read_event(e: event::Reader<'_>) -> Result<PushEvent, capnp::Error> {
    Ok(match e.which()? {
        event::UserData(u) => {
            let u = u?;
            let s = u.get_state()?;
            PushEvent::UserData {
                id: read_ref(u.get_id()?)?,
                played: s.get_played(),
                favorite: s.get_favorite(),
                position_ms: s.get_position_ms(),
                play_count: s.get_play_count(),
            }
        }
        event::LibraryChanged(l) => {
            let l = l?;
            PushEvent::Library {
                added: read_refs(l.get_added()?)?,
                updated: read_refs(l.get_updated()?)?,
                removed: read_refs(l.get_removed()?)?,
            }
        }
        event::Task(t) => {
            let t = t?;
            PushEvent::Task {
                id: t.get_id()?.to_string()?,
                progress: t.get_progress(),
                state: t.get_state()?.to_string()?,
                counts: TaskCounts {
                    added: t.get_added(),
                    changed: t.get_changed(),
                    removed: t.get_removed(),
                    failed_roots: t.get_failed_roots(),
                },
            }
        }
        event::Resync(()) => PushEvent::Resync,
    })
}

#[cfg(test)]
mod tests {
    use super::EventCursor;

    #[test]
    fn the_cursor_only_moves_forward_except_on_a_resync() {
        let c = EventCursor::default();
        c.advance(10);
        c.advance(7);
        assert_eq!(c.seq(), 10);
        // A resync from a newer run carries a far higher head; one from the
        // same run's window may carry a lower one after a client restart.
        c.reset(4);
        assert_eq!(c.seq(), 4);
        let shared = c.clone();
        shared.advance(5);
        assert_eq!(c.seq(), 5, "clones share one cursor");
    }
}
