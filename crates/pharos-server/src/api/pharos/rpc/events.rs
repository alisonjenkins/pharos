//! `Events`: live push of a user's stream to an `EventSink` (017 US2,
//! T041–T043, T026a, ADR-0094). The stream itself is
//! [`crate::services::events`]; this is the wire half and the per-
//! subscription pump.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::Arc;

use pharos_api::pharos_capnp::{done, event, event_sink, events as wire};
use pharos_api::{server, Denial};
use pharos_core::{MediaId, UserId};
use tokio::sync::broadcast;
use tokio::task::AbortHandle;

use super::library::set_item_id;
use super::session::Session;
use crate::services::events::{
    count, topic_label, Catchup, Event, Outcome, Payload, Topic, EVENT_LAG_SECONDS, SUBSCRIPTIONS,
};

/// Subscriptions one connection may hold at once (FR-010). The UI needs one;
/// the slack covers a reconnect that subscribes before the old one is
/// released.
pub const MAX_SUBSCRIPTIONS_PER_CONNECTION: u32 = 8;

/// Events one `push` carries at most. A folder marked played can announce
/// hundreds of episodes at once; this keeps one message well under the
/// client's traversal limit.
const MAX_BATCH: usize = 256;

/// `pharos_rpc_capabilities_total{type,transition}` (T045): a `Subscription`
/// issued and released. Issued minus released is how many are alive.
pub const CAPABILITIES_TOTAL: &str = "pharos_rpc_capabilities_total";

/// The subscriptions open on one connection, shared by its sessions: each
/// pump's abort handle and the token its session was opened with.
#[derive(Default)]
pub struct Subscriptions {
    next: Cell<u64>,
    pumps: RefCell<HashMap<u64, (String, AbortHandle)>>,
}

impl Subscriptions {
    fn len(&self) -> usize {
        self.pumps.borrow().len()
    }

    fn reserve(&self) -> u64 {
        let id = self.next.get();
        self.next.set(id.wrapping_add(1));
        id
    }

    fn insert(&self, id: u64, token: &str, pump: AbortHandle) {
        self.pumps.borrow_mut().insert(id, (token.to_owned(), pump));
    }

    fn remove(&self, id: u64) {
        self.pumps.borrow_mut().remove(&id);
    }

    /// Stops every subscription of sessions opened with `token`, after
    /// `Session.logout` ended it on this connection.
    pub(crate) fn stop_token(&self, token: &str) {
        let stopping: Vec<AbortHandle> = self
            .pumps
            .borrow()
            .values()
            .filter(|(t, _)| t == token)
            .map(|(_, pump)| pump.clone())
            .collect();
        if !stopping.is_empty() {
            tracing::info!(
                subscriptions = stopping.len(),
                "native events: session logged out; stopping its subscriptions"
            );
        }
        for pump in stopping {
            pump.abort();
        }
    }

    /// Stops every subscription, as the connection closes.
    pub(crate) fn stop_all(&self) {
        let stopping: Vec<AbortHandle> = self
            .pumps
            .borrow()
            .values()
            .map(|(_, pump)| pump.clone())
            .collect();
        for pump in stopping {
            pump.abort();
        }
    }
}

pub(super) struct Events {
    session: Rc<Session>,
}

impl Events {
    pub(super) fn new(session: Rc<Session>) -> Self {
        Self { session }
    }
}

/// The topics a subscriber asked for: empty means all of them.
fn read_topics(list: capnp::text_list::Reader<'_>) -> Result<HashSet<Topic>, Denial> {
    if list.is_empty() {
        return Ok(Topic::ALL.into_iter().collect());
    }
    let mut topics = HashSet::new();
    for name in list.iter() {
        let name = name
            .ok()
            .and_then(|t| t.to_str().ok())
            .ok_or(Denial::Invalid { field: "topics" })?;
        let Some(topic) = Topic::from_wire(name) else {
            tracing::info!(topic = %name, "native events: unknown topic refused");
            return Err(Denial::Invalid { field: "topics" });
        };
        topics.insert(topic);
    }
    Ok(topics)
}

impl server::events::Handler for Events {
    async fn subscribe(
        self: Rc<Self>,
        params: wire::SubscribeParams,
        results: &mut wire::SubscribeResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let sink = params.get_sink()?;
        let after = params.get_after_seq();
        let topics = read_topics(params.get_topics()?);
        let made = async {
            let user = self.session.current_user().await?;
            let topics = topics?;
            let held = u32::try_from(self.session.subscriptions().len()).unwrap_or(u32::MAX);
            if held >= MAX_SUBSCRIPTIONS_PER_CONNECTION {
                tracing::warn!(
                    user = %user.name,
                    held,
                    max = MAX_SUBSCRIPTIONS_PER_CONNECTION,
                    "native events: subscription refused at the per-connection cap"
                );
                return Err(Denial::Limit {
                    name: "subscriptions",
                    value: u64::from(held),
                    max: u64::from(MAX_SUBSCRIPTIONS_PER_CONNECTION),
                });
            }
            let log = &self.session.state.events;
            log.start(&self.session.state.bus);
            let (catchup, rx) = log.subscribe(user.id, after);
            let id = self.session.subscriptions().reserve();
            let held = Held::new(Rc::clone(&self.session), id, topics.clone());
            tracing::info!(
                user = %user.name,
                after_seq = after,
                topics = ?held.topic_labels(),
                start = catchup_label(&catchup),
                "native events: subscribed"
            );
            let pump = actix_web::rt::spawn(pump(
                Pump {
                    session: Rc::clone(&self.session),
                    user: user.id,
                    topics,
                    sink,
                    rx,
                    last: after,
                },
                catchup,
                held,
            ));
            self.session
                .subscriptions()
                .insert(id, &self.session.token, pump.abort_handle());
            Ok(Subscription { pump })
        }
        .await;
        let mut out = results.get().init_result();
        match made {
            Ok(subscription) => out.set_subscription(server::subscription::client(
                subscription,
                &self.session.conn(),
            )),
            Err(denial) => denial.write(out.init_error()),
        }
        Ok(())
    }
}

fn catchup_label(c: &Catchup) -> &'static str {
    match c {
        Catchup::Live { .. } => "live",
        Catchup::Replay(_) => "replay",
        Catchup::Resync { .. } => "resync",
    }
}

/// A live subscription. Dropping it, which the client does to unsubscribe,
/// stops its pump.
struct Subscription {
    pump: tokio::task::JoinHandle<()>,
}

impl server::subscription::Handler for Subscription {}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

/// What a running pump holds against the connection's cap and the gauges;
/// released however the pump ends, aborted included.
struct Held {
    session: Rc<Session>,
    id: u64,
    topics: HashSet<Topic>,
}

impl Held {
    fn new(session: Rc<Session>, id: u64, topics: HashSet<Topic>) -> Self {
        for topic in &topics {
            metrics::gauge!(SUBSCRIPTIONS, "topic" => topic.label()).increment(1.0);
        }
        metrics::counter!(CAPABILITIES_TOTAL, "type" => "subscription", "transition" => "issued")
            .increment(1);
        Self {
            session,
            id,
            topics,
        }
    }

    fn topic_labels(&self) -> Vec<&'static str> {
        let mut labels: Vec<_> = self.topics.iter().map(|t| t.label()).collect();
        labels.sort_unstable();
        labels
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.session.subscriptions().remove(self.id);
        for topic in &self.topics {
            metrics::gauge!(SUBSCRIPTIONS, "topic" => topic.label()).decrement(1.0);
        }
        metrics::counter!(CAPABILITIES_TOTAL, "type" => "subscription", "transition" => "released")
            .increment(1);
    }
}

/// Why a pump stopped, for its log line.
enum Stop {
    SinkGone(String),
    LoggedOut,
    LogClosed,
}

/// One subscription's delivery state.
struct Pump {
    session: Rc<Session>,
    user: UserId,
    topics: HashSet<Topic>,
    sink: event_sink::Client,
    rx: broadcast::Receiver<Event>,
    /// The highest `seq` the client has, or is told to resume from.
    last: u64,
}

/// What the pump sends next.
enum Next {
    /// The events, and the highest `seq` they cover.
    Events(Vec<Event>, u64),
    Resync(u64),
}

impl Pump {
    /// Refills `pending` from the window after `last`, for a pump that lagged
    /// its live channel; `None` unless the window no longer reaches back.
    fn catch_up(&self, pending: &mut VecDeque<Event>) -> Option<u64> {
        match self.session.state.events.catch_up(self.user, self.last) {
            Catchup::Live { .. } => {
                pending.clear();
                None
            }
            Catchup::Replay(events) => {
                *pending = events.into();
                None
            }
            Catchup::Resync { head } => Some(head),
        }
    }

    fn wants_topic(&self, event: &Event) -> bool {
        event
            .payload
            .topic()
            .map_or(true, |t| self.topics.contains(&t))
    }

    fn take(&self, pending: &mut VecDeque<Event>) -> (Vec<Event>, u64) {
        take_batch(pending, self.last, |e| self.wants_topic(e))
    }

    /// The ids of `ids` that `user` may see; none without a user, or when
    /// the access check fails.
    async fn visible(&self, user: Option<&pharos_core::User>, ids: &[MediaId]) -> Vec<MediaId> {
        let Some(user) = user else {
            return Vec::new();
        };
        let mut ids = ids.to_vec();
        match crate::services::access::retain_visible(
            &self.session.state,
            user,
            &mut ids,
            |id| *id,
            "events",
        )
        .await
        {
            Ok(()) => ids,
            Err(_) => Vec::new(),
        }
    }

    /// Drops what this user may not see: library ids outside their access
    /// (the change itself still arrives, as a bare "something changed") and,
    /// for a user whose policy hides anything, scan progress, whose ids name
    /// libraries.
    async fn scoped(&self, batch: Vec<Event>) -> Result<Vec<Event>, Stop> {
        let needs_scope = batch.iter().any(|e| {
            matches!(&*e.payload, Payload::Task { .. })
                || matches!(&*e.payload, Payload::Library { added, removed } if !added.is_empty() || !removed.is_empty())
        });
        if !needs_scope {
            return Ok(batch);
        }
        // Without the policy (store down), assume it hides everything.
        let user = match self.session.current_user().await {
            Ok(user) => Some(user),
            Err(Denial::Unavailable { .. }) => None,
            Err(_) => return Err(Stop::LoggedOut),
        };
        if user
            .as_ref()
            .is_some_and(|u| crate::services::library::hides_nothing(&u.policy))
        {
            return Ok(batch);
        }
        let mut scoped = Vec::with_capacity(batch.len());
        for event in batch {
            match &*event.payload {
                Payload::Task { .. } => {}
                Payload::Library { added, removed } => {
                    let payload = Payload::Library {
                        added: self.visible(user.as_ref(), added).await,
                        removed: self.visible(user.as_ref(), removed).await,
                    };
                    scoped.push(Event {
                        payload: Arc::new(payload),
                        ..event
                    });
                }
                _ => scoped.push(event),
            }
        }
        Ok(scoped)
    }
}

/// The next push's events from `pending`, oldest first: each `seq` once and
/// above `last` (a catch-up can overlap what the receiver still holds), those
/// `wanted`, at most [`MAX_BATCH`]. Also the highest `seq` taken, kept or
/// filtered, which the cursor moves to.
fn take_batch(
    pending: &mut VecDeque<Event>,
    last: u64,
    wanted: impl Fn(&Event) -> bool,
) -> (Vec<Event>, u64) {
    let mut reached = last;
    let mut batch = Vec::new();
    while batch.len() < MAX_BATCH {
        let Some(event) = pending.pop_front() else {
            break;
        };
        if event.seq <= reached {
            continue;
        }
        reached = event.seq;
        if wanted(&event) {
            batch.push(event);
        }
    }
    (batch, reached)
}

/// Sends `catchup`, then every live event on the subscription's topics, to
/// its sink in `seq` order, at most [`MAX_BATCH`] per `push` and one `push`
/// in flight at a time.
async fn pump(mut p: Pump, catchup: Catchup, _held: Held) {
    let mut pending: VecDeque<Event> = VecDeque::new();
    let mut resync = None;
    match catchup {
        Catchup::Live { head } => p.last = head,
        Catchup::Replay(events) => pending = events.into(),
        Catchup::Resync { head } => resync = Some(head),
    }
    let stop = loop {
        if resync.is_none() && pending.is_empty() {
            match p.rx.recv().await {
                Ok(event) => pending.push_back(event),
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::info!(
                        missed,
                        after_seq = p.last,
                        "native events: subscriber lagged; catching up from the window"
                    );
                    resync = p.catch_up(&mut pending);
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break Stop::LogClosed,
            }
        }
        if resync.is_none() {
            while pending.len() < MAX_BATCH {
                match p.rx.try_recv() {
                    Ok(event) => pending.push_back(event),
                    Err(broadcast::error::TryRecvError::Lagged(missed)) => {
                        tracing::info!(
                            missed,
                            after_seq = p.last,
                            "native events: subscriber lagged; catching up from the window"
                        );
                        resync = p.catch_up(&mut pending);
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
        let next = match resync.take() {
            Some(head) => {
                pending.clear();
                // The client refetches up to `head`; a cursor above it (another
                // run's) must not mask the live events that follow.
                p.last = head;
                Next::Resync(head)
            }
            None => {
                let (batch, reached) = p.take(&mut pending);
                Next::Events(batch, reached)
            }
        };
        let (batch, reached) = match next {
            Next::Resync(head) => (
                vec![Event {
                    seq: head,
                    at: std::time::Instant::now(),
                    payload: Arc::new(Payload::Resync),
                }],
                head,
            ),
            // Nothing for this subscriber, but the cursor still moves, so a
            // later lag catches up from here rather than resyncing.
            Next::Events(batch, reached) if batch.is_empty() => {
                p.last = reached;
                continue;
            }
            Next::Events(batch, reached) => match p.scoped(batch).await {
                Ok(batch) if batch.is_empty() => {
                    p.last = reached;
                    continue;
                }
                Ok(batch) => (batch, reached),
                Err(stop) => break stop,
            },
        };
        if p.session.ended().is_some() {
            break Stop::LoggedOut;
        }
        match push(&p.sink, &batch).await {
            Ok(accepted) => {
                let outcome = if accepted {
                    Outcome::Delivered
                } else {
                    Outcome::Rejected
                };
                for event in &batch {
                    let topic = topic_label(&event.payload);
                    count(topic, outcome, 1);
                    metrics::histogram!(EVENT_LAG_SECONDS, "topic" => topic)
                        .record(event.at.elapsed().as_secs_f64());
                }
                p.last = p.last.max(reached);
            }
            Err(e) => {
                for event in &batch {
                    count(topic_label(&event.payload), Outcome::Dropped, 1);
                }
                break Stop::SinkGone(e.to_string());
            }
        }
    };
    let last = p.last;
    match stop {
        Stop::SinkGone(error) => {
            tracing::info!(last_seq = last, error = %error, "native events: sink gone; subscription ended");
        }
        Stop::LoggedOut => {
            tracing::info!(
                last_seq = last,
                "native events: session logged out; subscription ended"
            );
        }
        Stop::LogClosed => {
            tracing::info!(
                last_seq = last,
                "native events: event log closed; subscription ended"
            );
        }
    }
}

/// Sends one batch; `Ok(false)` when the sink answered it with an error.
async fn push(sink: &event_sink::Client, batch: &[Event]) -> Result<bool, capnp::Error> {
    let mut req = sink.push_request();
    req.get().init_ctx();
    let mut list = req
        .get()
        .init_events(u32::try_from(batch.len()).unwrap_or(u32::MAX));
    for (i, event) in (0u32..).zip(batch) {
        write_event(list.reborrow().get(i), event);
    }
    let reply = req.send().promise.await?;
    Ok(match reply.get()?.get_result()?.which()? {
        done::Ok(()) => true,
        done::Error(e) => {
            let reason = e
                .and_then(|e| e.get_reason())
                .ok()
                .and_then(|r| r.to_str().ok().map(str::to_owned))
                .unwrap_or_default();
            tracing::info!(reason = %reason, "native events: sink refused a batch");
            false
        }
    })
}

fn write_event(mut out: event::Builder<'_>, event: &Event) {
    out.set_seq(event.seq);
    match &*event.payload {
        Payload::UserData { id, state } => {
            let mut changed = out.init_user_data();
            set_item_id(changed.reborrow().init_id(), *id);
            let mut s = changed.init_state();
            s.set_played(state.played);
            s.set_favorite(state.favorite);
            s.set_position_ms(state.position_ms);
            s.set_play_count(state.play_count);
        }
        Payload::Library { added, removed } => {
            let mut changed = out.init_library_changed();
            let mut list = changed
                .reborrow()
                .init_added(u32::try_from(added.len()).unwrap_or(u32::MAX));
            for (i, id) in (0u32..).zip(added) {
                list.reborrow().get(i).set_real(*id);
            }
            changed.reborrow().init_updated(0);
            let mut list = changed.init_removed(u32::try_from(removed.len()).unwrap_or(u32::MAX));
            for (i, id) in (0u32..).zip(removed) {
                list.reborrow().get(i).set_real(*id);
            }
        }
        Payload::Task {
            id,
            progress,
            state,
            result,
        } => {
            let mut task = out.init_task();
            task.set_id(id.as_str());
            task.set_progress(*progress);
            task.set_state(*state);
            if let Some(tally) = result {
                let wire = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
                task.set_added(wire(tally.added));
                task.set_changed(wire(tally.changed));
                task.set_removed(wire(tally.removed));
                task.set_failed_roots(wire(tally.failed_roots));
            }
        }
        Payload::Resync => out.set_resync(()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ev(seq: u64, payload: Payload) -> Event {
        Event {
            seq,
            at: std::time::Instant::now(),
            payload: Arc::new(payload),
        }
    }

    fn lib(seq: u64) -> Event {
        ev(
            seq,
            Payload::Library {
                added: vec![],
                removed: vec![],
            },
        )
    }

    #[test]
    fn a_catch_up_overlapping_the_receiver_is_sent_once_in_order() {
        // The window's replay, then what the receiver still held.
        let mut pending: VecDeque<Event> = [5, 6, 7, 6, 7, 8].map(lib).into();
        let (batch, reached) = take_batch(&mut pending, 4, |_| true);
        assert_eq!(
            batch.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [5, 6, 7, 8]
        );
        assert_eq!(reached, 8);
    }

    #[test]
    fn a_batch_is_capped_and_the_rest_waits() {
        let mut pending: VecDeque<Event> = (1..=600).map(lib).collect();
        let (batch, reached) = take_batch(&mut pending, 0, |_| true);
        assert_eq!(batch.len(), MAX_BATCH);
        assert_eq!(reached, 256);
        assert_eq!(pending.front().unwrap().seq, 257);
    }

    #[test]
    fn filtered_events_still_move_the_cursor() {
        let mut pending: VecDeque<Event> = (10..=12).map(lib).collect();
        let (batch, reached) = take_batch(&mut pending, 9, |_| false);
        assert!(batch.is_empty());
        assert_eq!(reached, 12);
    }
}
