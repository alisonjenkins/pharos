//! `Events`: live push of a user's stream to an `EventSink` (017 US2,
//! T041–T043, T026a, ADR-0094). The stream itself is
//! [`crate::services::events`]; this is the wire half and the per-
//! subscription pump.

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;

use pharos_api::pharos_capnp::{done, event, event_sink, events as wire};
use pharos_api::{server, Denial};
use pharos_core::UserId;
use tokio::sync::broadcast;

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

/// How many subscriptions a connection holds; shared by its sessions.
#[derive(Debug, Default)]
pub struct SubscriptionCount(Cell<u32>);

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
            let held = self.session.subscriptions().0.get();
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
            let held = Held::new(Rc::clone(&self.session), topics.clone());
            tracing::info!(
                user = %user.name,
                after_seq = after,
                topics = ?held.topic_labels(),
                start = catchup_label(&catchup),
                "native events: subscribed"
            );
            let pump = actix_web::rt::spawn(pump(
                Rc::clone(&self.session),
                user.id,
                topics,
                sink,
                catchup,
                rx,
                held,
            ));
            let on_close = pump.abort_handle();
            self.session.on_connection_close(move || on_close.abort());
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
    topics: HashSet<Topic>,
}

impl Held {
    fn new(session: Rc<Session>, topics: HashSet<Topic>) -> Self {
        let count = &session.subscriptions().0;
        count.set(count.get().saturating_add(1));
        for topic in &topics {
            metrics::gauge!(SUBSCRIPTIONS, "topic" => topic.label()).increment(1.0);
        }
        metrics::counter!(CAPABILITIES_TOTAL, "type" => "subscription", "transition" => "issued")
            .increment(1);
        Self { session, topics }
    }

    fn topic_labels(&self) -> Vec<&'static str> {
        let mut labels: Vec<_> = self.topics.iter().map(|t| t.label()).collect();
        labels.sort_unstable();
        labels
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let count = &self.session.subscriptions().0;
        count.set(count.get().saturating_sub(1));
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

/// Sends `catchup`, then every live event for `user` on `topics`, to `sink`
/// in `seq` order, one `push` in flight at a time.
async fn pump(
    session: Rc<Session>,
    user: UserId,
    topics: HashSet<Topic>,
    sink: event_sink::Client,
    catchup: Catchup,
    mut rx: broadcast::Receiver<(UserId, Event)>,
    _held: Held,
) {
    let mut last = 0u64;
    let mut pending: Vec<Event> = Vec::new();
    let mut resync_at = None;
    match catchup {
        Catchup::Live { head } => last = head,
        Catchup::Replay(events) => pending = events,
        Catchup::Resync { head } => resync_at = Some(head),
    }
    let stop = loop {
        if let Some(head) = resync_at.take() {
            pending.clear();
            pending.push(Event {
                seq: head,
                at: std::time::Instant::now(),
                payload: std::sync::Arc::new(Payload::Resync),
            });
        }
        if pending.is_empty() {
            match rx.recv().await {
                Ok((owner, event)) => {
                    if owner == user {
                        pending.push(event);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::info!(
                        missed,
                        after_seq = last,
                        "native events: subscriber lagged; catching up from the window"
                    );
                    match session.state.events.catch_up(user, last) {
                        Catchup::Live { .. } => {}
                        Catchup::Replay(events) => pending = events,
                        Catchup::Resync { head } => resync_at = Some(head),
                    }
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break Stop::LogClosed,
            }
        }
        while pending.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok((owner, event)) => {
                    if owner == user {
                        pending.push(event);
                    }
                }
                Err(broadcast::error::TryRecvError::Lagged(_)) => {
                    // The window has everything after `last`; the receiver
                    // reports the lag again on the next `recv`.
                    break;
                }
                Err(_) => break,
            }
        }
        let batch: Vec<Event> = std::mem::take(&mut pending)
            .into_iter()
            .filter(|e| e.seq > last)
            .filter(|e| e.payload.topic().map_or(true, |t| topics.contains(&t)))
            .collect();
        if batch.is_empty() {
            continue;
        }
        if session.ended().is_some() {
            break Stop::LoggedOut;
        }
        match push(&sink, &batch).await {
            Ok(accepted) => {
                let outcome = if accepted {
                    Outcome::Delivered
                } else {
                    Outcome::Rejected
                };
                for event in &batch {
                    let topic = topic_label(&event.payload);
                    count(topic, outcome, 1);
                    if matches!(*event.payload, Payload::Resync) {
                        count(topic, Outcome::Resync, 1);
                    }
                    metrics::histogram!(EVENT_LAG_SECONDS, "topic" => topic)
                        .record(event.at.elapsed().as_secs_f64());
                }
                last = batch.iter().map(|e| e.seq).max().unwrap_or(last).max(last);
            }
            Err(e) => {
                for event in &batch {
                    count(topic_label(&event.payload), Outcome::Dropped, 1);
                }
                break Stop::SinkGone(e.to_string());
            }
        }
    };
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
        } => {
            let mut task = out.init_task();
            task.set_id(id.as_str());
            task.set_progress(*progress);
            task.set_state(*state);
        }
        Payload::Resync => out.set_resync(()),
    }
}
