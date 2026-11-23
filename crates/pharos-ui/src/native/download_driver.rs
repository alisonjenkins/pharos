//! Runs the download queue (ADR 0148) on every app that plays video: reads
//! the saved [`Queue`] and the store's records once a second, asks
//! [`next_action`] what to do, and does it. It negotiates each stream when
//! the download reaches the front of the queue, holds that play session's
//! grant while the download runs, and drops it as soon as the download
//! stops, so a queued, paused or finished download holds no session.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;

use dioxus::core::spawn_forever;
use dioxus::prelude::*;
use pharos_client::api::{Detail, NativeGrant, PharosApi, TrackPicks};
use pharos_client::ClientError;

use super::app::{call, Ctx};
use super::download_queue::{
    admit, next_action, start_gate, still_startable, Action, Halt, Queue, QueueEntry, Tries,
    QUEUE_KEY,
};
use super::kept_video::{KeepVideoRequest, KeptState, KeptVideo, VideoStore};
use super::offline_video;
use super::platform;
use super::ports::video_store;

/// How often the driver looks at the queue.
const TICK_MS: u32 = 1000;

thread_local! {
    static TRIES: RefCell<HashMap<String, Tries>> = RefCell::new(HashMap::new());
    /// The grant of the download running now, by item.
    static GRANTS: RefCell<HashMap<String, NativeGrant>> = RefCell::new(HashMap::new());
    /// A start is negotiating: the next tick waits for it.
    static STARTING: Cell<bool> = const { Cell::new(false) };
    static STARTED: Cell<bool> = const { Cell::new(false) };
}

/// Bumped on every change to the saved queue, so a page showing it draws
/// again at once rather than at its next poll.
pub static QUEUE_CHANGED: GlobalSignal<u32> = Signal::global(|| 0);

fn now_ms() -> u64 {
    platform::now_ms().max(0.0) as u64
}

/// The saved queue.
pub fn load() -> Queue {
    match platform::stored_theme(QUEUE_KEY) {
        Ok(text) => Queue::parse(text.as_deref()),
        Err(e) => {
            platform::warn(&format!("pharos: video queue unreadable error={e}"));
            Queue::default()
        }
    }
}

fn save(queue: &Queue) {
    let saved = serde_json::to_string(queue)
        .map_err(|e| e.to_string())
        .and_then(|t| platform::store_theme(QUEUE_KEY, &t));
    if let Err(e) = saved {
        platform::warn(&format!("pharos: video queue not saved error={e}"));
    }
    *QUEUE_CHANGED.write() += 1;
}

/// The driver's tries, for the page's rows.
pub fn tries() -> HashMap<String, Tries> {
    TRIES.with(|t| t.borrow().clone())
}

fn release(item: &str, why: &str) {
    if GRANTS.with(|g| g.borrow_mut().remove(item)).is_some() {
        platform::info(&format!(
            "pharos: video queue grant released item={item} reason={why}"
        ));
    }
}

/// Puts `detail` (at `version`, `quality_bps`) on the queue, after the
/// downloads cap and the free-space floor are checked against everything
/// kept and queued. The error is the line to show.
pub(super) async fn enqueue(
    ctx: Ctx,
    detail: &Detail,
    version: &str,
    quality_bps: u64,
    store: &dyn VideoStore,
) -> Result<(), String> {
    let Some(user) = offline_video::last_user() else {
        return Err("Sign in to download this video.".into());
    };
    // The chosen file's tracks are its own.
    let own;
    let detail = if version.is_empty() {
        detail
    } else {
        let Some(api) = ctx.api.peek().clone() else {
            return Err("Connect to the server to download this version.".into());
        };
        own = api
            .detail_version(&call("title", "download"), detail.item.id, version)
            .await
            .map_err(|e| format!("Could not read this version: {e}"))?;
        &own
    };
    let item = &detail.item;
    let entry = QueueEntry {
        user: user.clone(),
        item: item.id.to_string(),
        version: version.to_owned(),
        title: offline_video::video_title(item),
        kind: if item.kind == pharos_client::api::ItemKind::Episode {
            "episode"
        } else {
            "movie"
        }
        .to_owned(),
        runtime_ms: item.runtime_ms,
        subtitles: offline_video::kept_subtitles(&detail.subtitle_tracks),
        quality_bps,
        paused: false,
        failed: None,
    };
    let mut queue = load();
    let records = store.videos(&user);
    admit(
        &queue,
        &records,
        &entry,
        store.used_bytes(&user),
        store.cap_bytes(),
        store.free_bytes(),
    )
    .map_err(|refusal| {
        platform::warn(&format!(
            "pharos: video queue refused item={} reason={refusal}",
            entry.item
        ));
        refusal.to_string()
    })?;
    let place = queue.entries.iter().filter(|e| e.user == user).count() + 1;
    platform::info(&format!(
        "pharos: video queue added item={} quality_bps={quality_bps} place={place} estimate={}",
        entry.item,
        entry.estimate()
    ));
    TRIES.with(|t| t.borrow_mut().remove(&entry.item));
    queue.push(entry);
    save(&queue);
    Ok(())
}

/// The viewer's Pause: a running download stops where it is, a queued one
/// is passed over until Resume.
pub fn pause(store: &dyn VideoStore, user: &str, item: &str) -> Result<(), String> {
    let running = store
        .videos(user)
        .iter()
        .any(|r| r.item == item && matches!(r.state, KeptState::Downloading { .. }));
    if running {
        store.pause(user, item)?;
        release(item, "paused");
    }
    let mut queue = load();
    queue.set_paused(user, item, true);
    save(&queue);
    platform::info(&format!(
        "pharos: video queue paused item={item} was_running={running}"
    ));
    Ok(())
}

/// The viewer's Resume, Retry or Try now: the download is due at once.
pub fn resume(user: &str, item: &str) {
    let mut queue = load();
    queue.set_paused(user, item, false);
    if let Some(e) = queue
        .entries
        .iter_mut()
        .find(|e| e.user == user && e.item == item)
    {
        e.failed = None;
    }
    save(&queue);
    TRIES.with(|t| t.borrow_mut().remove(item));
    platform::info(&format!("pharos: video queue resumed item={item}"));
}

/// The viewer's Retry of a download that failed for good: due at once, and
/// started even though its record carries no halt.
pub(super) fn retry(ctx: Ctx, user: &str, item: &str) {
    resume(user, item);
    let (user, item) = (user.to_owned(), item.to_owned());
    spawn_forever(async move {
        if let Some(store) = video_store() {
            start(ctx, &*store, &user, &item, "retry").await;
        }
    });
}

/// Moves a queued download one place.
pub fn shift(user: &str, item: &str, up: bool) {
    let mut queue = load();
    if queue.shift(user, item, up) {
        save(&queue);
        platform::info(&format!(
            "pharos: video queue moved item={item} direction={}",
            if up { "up" } else { "down" }
        ));
    }
}

/// After the viewer's Remove took the files and the record: the queue
/// entry, the grant and the tries go too.
pub fn forget(user: &str, item: &str) {
    release(item, "removed");
    let mut queue = load();
    if queue.remove(user, item) {
        save(&queue);
    }
    TRIES.with(|t| t.borrow_mut().remove(item));
    platform::info(&format!("pharos: video queue removed item={item}"));
}

/// Starts the driver once for the app's life. Call it where [`Ctx`] is
/// provided.
pub fn use_download_driver() {
    let ctx = use_context::<Ctx>();
    use_hook(move || {
        if STARTED.replace(true) || !platform::SLEEP_IS_REAL {
            return;
        }
        spawn_forever(async move {
            loop {
                platform::sleep_ms(TICK_MS).await;
                tick(ctx).await;
            }
        });
    });
}

async fn tick(ctx: Ctx) {
    let (Some(store), Some(user)) = (video_store(), offline_video::last_user()) else {
        return;
    };
    if STARTING.get() {
        return;
    }
    let records = store.videos(&user);
    let mut queue = load();
    if queue.reconcile(&user, &records) {
        save(&queue);
    }
    note_progress(&records);
    for r in &records {
        if !matches!(r.state, KeptState::Downloading { .. }) {
            release(&r.item, "not_downloading");
        }
    }
    let action = next_action(&queue, &records, &tries(), &user, now_ms());
    match action {
        Action::Idle | Action::Wait { .. } => {}
        Action::Restart { item } => {
            // Its stream changed under a new session: the files cannot be
            // kept, so the record goes and the next tick starts it afresh.
            platform::warn(&format!(
                "pharos: video queue action=restart item={item} reason=stream_changed"
            ));
            store.remove(&user, &item);
        }
        Action::Start { item } => {
            let why = records
                .iter()
                .find(|r| r.item == item)
                .and_then(|r| r.halt)
                .map_or("next", Halt::label);
            start(ctx, &*store, &user, &item, why).await;
        }
    }
}

/// Resets the tries of each download that stored more since the last tick.
fn note_progress(records: &[KeptVideo]) {
    TRIES.with(|t| {
        let mut t = t.borrow_mut();
        for r in records {
            if let KeptState::Downloading { bytes, .. } = r.state {
                if let Some(tries) = t.get_mut(&r.item) {
                    tries.progressed(bytes);
                }
            }
        }
    });
}

fn note_try(item: &str, answered: bool) -> Tries {
    TRIES.with(|t| {
        let mut t = t.borrow_mut();
        let tries = t.entry(item.to_owned()).or_default();
        tries.failed(now_ms(), answered);
        *tries
    })
}

fn fail_for_good(user: &str, item: &str, reason: String) {
    platform::warn(&format!(
        "pharos: video queue failed item={item} reason={reason}"
    ));
    let mut queue = load();
    if let Some(e) = queue
        .entries
        .iter_mut()
        .find(|e| e.user == user && e.item == item)
    {
        e.failed = Some(reason);
        save(&queue);
    }
}

/// Negotiates `item`'s stream and hands it to the store. A failure is a
/// try (counted as answered when the server answered it) or, when no try
/// can mend it, a failure the row shows until Retry.
async fn start(ctx: Ctx, store: &dyn VideoStore, user: &str, item: &str, why: &str) {
    let Some(entry) = load().find(user, item).cloned() else {
        return;
    };
    // The tick's gates hold for a viewer's Retry too: with one open the
    // item stays due, and the tick starts it once both are.
    if let Err(gate) = start_gate(STARTING.get(), &store.videos(user), user) {
        platform::info(&format!(
            "pharos: video queue action=start verdict=deferred item={item} reason={gate} trigger={why}"
        ));
        return;
    }
    let resumed_halt = store
        .videos(user)
        .iter()
        .find(|r| r.item == item)
        .and_then(|r| r.halt)
        .filter(|h| matches!(h, Halt::SessionEnded | Halt::Unreachable));
    STARTING.set(true);
    let outcome = negotiate(ctx, &entry).await;
    STARTING.set(false);
    // The viewer may have removed or paused it while the negotiation was
    // out: its grant is dropped, nothing is started.
    if let (Ok(_), Err(gone)) = (&outcome, still_startable(&load(), user, item)) {
        platform::info(&format!(
            "pharos: video queue action=start verdict=dropped item={item} reason={gone}"
        ));
        return;
    }
    let grant = settle(store, user, item, why, resumed_halt, outcome);
    if let Some(grant) = grant {
        GRANTS.with(|g| g.borrow_mut().insert(item.to_owned(), grant));
    }
    // The try the outcome counted (and when the next is) is on the page.
    *QUEUE_CHANGED.write() += 1;
}

/// What a negotiation's outcome does: starts the run, or counts the try that
/// failed. The grant of a started run, for the caller to keep.
fn settle<G>(
    store: &dyn VideoStore,
    user: &str,
    item: &str,
    why: &str,
    resumed_halt: Option<Halt>,
    outcome: Result<(KeepVideoRequest, G), Negotiation>,
) -> Option<G> {
    match outcome {
        Ok((request, grant)) => match store.start(request) {
            Ok(()) => {
                // A store halt is a try the server took part in: it answered
                // the negotiation that started that run. It is counted once,
                // when a new run replaces it: counted at every attempt, a
                // halt kept across offline retries reached the give-up count
                // with no server answer at all.
                let attempt = resumed_halt.map_or(0, |halt| {
                    let t = note_try(item, true);
                    platform::info(&format!(
                        "pharos: video queue action=count_halt item={item} halt={halt:?} tries={} answered={}",
                        t.tries, t.answered
                    ));
                    t.tries
                });
                platform::info(&format!(
                    "pharos: video queue action=start item={item} reason={why} attempt={attempt}"
                ));
                return Some(grant);
            }
            Err(refusal) => fail_for_good(user, item, refusal.to_string()),
        },
        Err(Negotiation::Offline(reason)) => {
            let t = note_try(item, false);
            platform::info(&format!(
                "pharos: video queue action=wait item={item} reason=offline detail={reason} tries={}",
                t.tries
            ));
        }
        Err(Negotiation::Busy(reason)) => {
            let t = note_try(item, true);
            platform::warn(&format!(
                "pharos: video queue action=wait item={item} reason=server_refused detail={reason} answered={}",
                t.answered
            ));
            if t.given_up() {
                fail_for_good(
                    user,
                    item,
                    format!("gave up after {} tries: {reason}", t.tries),
                );
            }
        }
        Err(Negotiation::Never(reason)) => fail_for_good(user, item, reason),
    }
    None
}

enum Negotiation {
    /// The server could not be reached.
    Offline(String),
    /// The server answered with an error a later try may not repeat.
    Busy(String),
    /// No try can mend it (the item is gone, the stream is not HLS).
    Never(String),
}

async fn negotiate(
    ctx: Ctx,
    e: &QueueEntry,
) -> Result<(KeepVideoRequest, NativeGrant), Negotiation> {
    let Some(api) = ctx.api.peek().clone() else {
        return Err(Negotiation::Offline("not connected".into()));
    };
    let id = e
        .item
        .parse()
        .map_err(|_| Negotiation::Never(format!("{} is not an item", e.item)))?;
    let caps = offline_video::download_caps(platform::capabilities(), e.quality_bps);
    let (decision, grant) = api
        .play(
            &call("downloads", "queue"),
            id,
            &e.version,
            &caps,
            TrackPicks::default(),
        )
        .await
        .map_err(|err| match &err {
            ClientError::Rpc(_) => Negotiation::Offline(err.to_string()),
            ClientError::Api { kind, .. }
                if matches!(*kind, "notFound" | "notPermitted" | "invalid") =>
            {
                Negotiation::Never(format!("The server would not start a stream: {err}"))
            }
            ClientError::Api { .. } => Negotiation::Busy(err.to_string()),
        })?;
    if !decision.is_hls() {
        return Err(Negotiation::Never(format!(
            "The server chose {} for this video, which cannot be downloaded.",
            decision.method
        )));
    }
    Ok((
        KeepVideoRequest {
            item: e.item.clone(),
            version: e.version.clone(),
            user: e.user.clone(),
            title: e.title.clone(),
            kind: e.kind.clone(),
            runtime_ms: e.runtime_ms,
            master_url: decision.stream_url.clone(),
            audio: decision.audio,
            subtitles: e.subtitles.clone(),
            quality_bps: e.quality_bps,
        },
        grant,
    ))
}

/// The store, for a page handler.
pub fn store() -> Option<Arc<dyn VideoStore>> {
    video_store()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::native::kept_video::KeepVideoRefusal;

    #[derive(Default)]
    struct Store {
        started: std::sync::Mutex<Vec<String>>,
    }

    impl VideoStore for Store {
        fn videos(&self, _: &str) -> Vec<KeptVideo> {
            Vec::new()
        }
        fn start(&self, request: KeepVideoRequest) -> Result<(), KeepVideoRefusal> {
            self.started.lock().unwrap().push(request.item);
            Ok(())
        }
        fn remove(&self, _: &str, _: &str) {}
        fn save_position(&self, _: &str, _: &str, _: u64) {}
        fn cap_bytes(&self) -> u64 {
            u64::MAX
        }
        fn used_bytes(&self, _: &str) -> u64 {
            0
        }
        fn free_bytes(&self) -> u64 {
            u64::MAX
        }
    }

    fn request(item: &str) -> KeepVideoRequest {
        KeepVideoRequest {
            item: item.to_owned(),
            version: "v".to_owned(),
            user: "u".to_owned(),
            title: String::new(),
            kind: String::new(),
            runtime_ms: 0,
            master_url: String::new(),
            audio: None,
            subtitles: Vec::new(),
            quality_bps: 0,
        }
    }

    fn tries_of(item: &str) -> Tries {
        TRIES.with(|t| t.borrow().get(item).copied().unwrap_or_default())
    }

    /// A halt kept in the store across attempts that never reached the
    /// server is not an answer each time: only the run that replaces it
    /// counts it, once.
    #[test]
    fn a_kept_halt_is_counted_once_when_its_run_is_replaced() {
        let store = Store::default();
        let item = "halt-count";
        for _ in 0..10 {
            let _ = settle::<()>(
                &store,
                "u",
                item,
                "tick",
                Some(Halt::SessionEnded),
                Err(Negotiation::Offline("not connected".into())),
            );
        }
        let t = tries_of(item);
        assert_eq!(t.answered, 0, "unanswered attempts counted as answered");
        assert!(!t.given_up(), "offline retries of a halt gave the item up");
        let grant = settle(
            &store,
            "u",
            item,
            "tick",
            Some(Halt::SessionEnded),
            Ok((request(item), 7)),
        );
        assert_eq!(grant, Some(7));
        assert_eq!(*store.started.lock().unwrap(), [item]);
        assert_eq!(tries_of(item).answered, 1, "the halt was not counted once");
    }
}
