//! Views for downloaded videos (spec 048): the Download control on a title
//! page, the Downloads page and the offline start's list. The pure
//! components take data in and send events out, so they render under
//! `dioxus-ssr`; the stateful wrappers below them talk to the registered
//! [`VideoStore`].

use dioxus::core::spawn_forever;
use dioxus::prelude::*;
use pharos_client::api::{Detail, ItemRef};

use super::app::{AccountMe, Ctx};
use super::download_driver;
use super::download_queue::{self, Halt, RowState};
use super::kept_video::{
    cap_from_option, cap_options, cap_selected, quality_label, KeptState, KeptVideo, VideoStore,
    DEFAULT_QUALITY_BPS, QUALITY_CHOICES_BPS,
};
use super::offline_ui::format_bytes;
use super::offline_video;
use super::offline_view::poll_every;
use super::platform;
use super::ports::{video_store, Slot};
use super::route::Route;
use super::settings::Choices;
use super::views::Nav;

/// Bumped when the video store is published after the app mounted. Every
/// component that reads the store through [`shown_video_store`] subscribes
/// to it, so one that rendered with no store renders again once there is
/// one. The phone registers its store from two places, and the second caller
/// can return before the first has published.
static VIDEO_PUBLISHED: GlobalSignal<u32> = Signal::global(|| 0);

/// The registered video store, read by a component: it renders again when
/// a store is published after it rendered without one.
pub fn shown_video_store() -> Option<std::sync::Arc<dyn super::kept_video::VideoStore>> {
    let _ = VIDEO_PUBLISHED.read();
    video_store()
}

/// Run once by the app's root: when no video store is registered at mount,
/// waits (in a task, never on the UI thread) for one to be published and
/// then re-renders every reader of [`shown_video_store`].
pub fn use_video_store_watch() {
    use_publish_watch(super::ports::video_slot(), &VIDEO_PUBLISHED, "video_store");
}

fn use_publish_watch<T: ?Sized + Send + Sync + 'static>(
    slot: &'static Slot<T>,
    seen: &'static GlobalSignal<u32>,
    what: &'static str,
) {
    use_hook(move || {
        if slot.get().is_some() {
            return;
        }
        platform::info(&format!(
            "pharos: port {what} verdict=awaiting_publish reason=not_registered_at_mount"
        ));
        spawn(async move {
            let _ = slot.published().await;
            platform::info(&format!(
                "pharos: port {what} verdict=published_after_mount readers=rerendered"
            ));
            *seen.write() += 1;
        });
    });
}

/// How often a screen re-reads the store while it is on screen, so a
/// download's progress shows. The port has no change feed.
const POLL_MS: u32 = 1500;

/// The id of the quality choice on a title page.
pub const QUALITY_SELECT: &str = "pharos-video-quality";
/// The id of the Download button on a title page.
pub const DOWNLOAD_BUTTON: &str = "pharos-video-download";

/// "Downloading 40%": files stored of files planned.
pub fn percent(done: u32, total: u32) -> u32 {
    if total == 0 {
        return 0;
    }
    let p = u64::from(done).saturating_mul(100) / u64::from(total);
    u32::try_from(p.min(100)).unwrap_or(100)
}

/// What a download says about itself in words.
pub fn state_line(v: &KeptVideo) -> String {
    match &v.state {
        KeptState::Downloading { done, total, .. } => {
            format!("Downloading {}%", percent(*done, *total))
        }
        KeptState::Ready => format!("On this device \u{b7} {}", format_bytes(v.bytes())),
        KeptState::Failed { .. } if v.halt == Some(Halt::Paused) => "Paused".to_owned(),
        // ADR 0148: the queue carries it on by itself.
        KeptState::Failed { .. } if v.halt.is_some() => "Waiting to carry on".to_owned(),
        KeptState::Failed { reason } => format!("Failed: {reason}"),
    }
}

/// The Download control of a title page: the quality and Download while
/// nothing is stored, progress while it is fetched, the size once it is on
/// the device, and the reason with Retry when it stopped short.
#[component]
pub fn VideoKeepControl(
    video: Option<KeptVideo>,
    quality: u64,
    busy: bool,
    message: Option<String>,
    on_quality: EventHandler<u64>,
    on_download: EventHandler<()>,
    on_remove: EventHandler<()>,
    /// The queue's word for a download not started yet ("Queued · 3rd"), and
    /// whether it failed for good: then Retry is offered, in place of
    /// Download.
    #[props(default)]
    queued: Option<QueuedKeep>,
) -> Element {
    // Only a failure no later try mends offers Retry (ADR 0148).
    let state = video
        .as_ref()
        .map(|v| {
            (
                state_line(v),
                matches!(v.state, KeptState::Failed { .. }) && v.halt.is_none(),
            )
        })
        .or_else(|| queued.map(|q| (q.line, q.failed)));
    let idle = !matches!(&state, Some((_, false)));
    let label = match (&state, busy) {
        (_, true) => "Starting\u{2026}",
        (Some((_, true)), _) => "Retry",
        _ => "Download",
    };
    rsx! {
        div { class: "pharos-video-keep",
            if idle {
                select {
                    id: QUALITY_SELECT,
                    aria_label: "Download quality",
                    disabled: busy,
                    onchange: move |e| {
                        if let Ok(bps) = e.value().parse::<u64>() {
                            on_quality.call(bps);
                        }
                    },
                    for bps in QUALITY_CHOICES_BPS {
                        option { value: "{bps}", selected: bps == quality, "{quality_label(bps)}" }
                    }
                }
                button {
                    id: DOWNLOAD_BUTTON,
                    class: "pharos-button pharos-button-secondary",
                    r#type: "button",
                    disabled: busy,
                    aria_busy: busy.then_some("true"),
                    onclick: move |_| on_download.call(()),
                    "{label}"
                }
            }
            if let Some((line, _)) = state.clone() {
                span { class: "pharos-muted", role: "status", id: "pharos-video-state", "{line}" }
                button {
                    class: "pharos-button pharos-button-secondary",
                    r#type: "button",
                    onclick: move |_| on_remove.call(()),
                    "Remove"
                }
            }
            if let Some(message) = message {
                p { class: "pharos-reader-locked", role: "alert", "{message}" }
            }
        }
    }
}

/// The downloaded videos, each with its quality, size and state, a Play
/// button once ready and a Remove button. `used` and `cap` head the list.
#[component]
pub fn VideoList(
    videos: Vec<KeptVideo>,
    used: u64,
    cap: u64,
    on_play: EventHandler<String>,
    on_remove: EventHandler<String>,
    #[props(default)] problem: Option<String>,
    /// Downloads on the device whose record cannot be read: they block new
    /// downloads until removed. The row shows only when there are some.
    #[props(default)]
    unreadable: usize,
    /// Folders a removal could not delete: still on the device and in the
    /// cap. They keep the row (and Remove them) up until they are gone.
    #[props(default)]
    leftover: usize,
    #[props(default)] on_discard: EventHandler<()>,
    /// The signed-in user's videos' own bytes, when `used` (the device's
    /// shared total) also counts what they share the cap with: the phone's
    /// music, other users' downloads, leftover folders. `None`: `used` is
    /// the videos' alone.
    #[props(default)]
    videos_used: Option<u64>,
    /// Whether `used` was read. False while no owner is authorized: the
    /// shared figure is then not shown, never as zero.
    #[props(default = true)]
    used_known: bool,
    /// A discard is running: its button is disabled until it ends.
    #[props(default)]
    discarding: bool,
) -> Element {
    rsx! {
        // Each figure says what it counts: the user's videos' own bytes, and,
        // when the cap is shared, the device's total with everything else.
        match videos_used.filter(|v| *v != used) {
            _ if !used_known => rsx! {
                p { class: "pharos-muted", id: "pharos-video-total",
                    "Space used on this device shows once your account is confirmed. The space allowance is {format_bytes(cap)}."
                }
            },
            Some(videos) => rsx! {
                p { class: "pharos-muted", id: "pharos-video-total",
                    "{format_bytes(videos)} used by your downloaded videos. {format_bytes(used)} of {format_bytes(cap)} used on this device, with music and other downloads."
                }
            },
            None => rsx! {
                p { class: "pharos-muted", id: "pharos-video-total",
                    "{format_bytes(used)} of {format_bytes(cap)} used by downloaded videos."
                }
            },
        }
        if let Some(text) = problem {
            p { class: "pharos-notice", role: "alert", "{text}" }
        }
        if unreadable > 0 || leftover > 0 {
            div { class: "pharos-download-head", id: "pharos-video-unreadable",
                if unreadable > 0 {
                    span { class: "pharos-muted",
                        if unreadable == 1 {
                            "1 download couldn't be read."
                        } else {
                            "{unreadable} downloads couldn't be read."
                        }
                    }
                }
                if leftover > 0 {
                    span { class: "pharos-muted",
                        if leftover == 1 {
                            "1 download couldn't be removed."
                        } else {
                            "{leftover} downloads couldn't be removed."
                        }
                    }
                }
                button {
                    class: "pharos-button pharos-button-danger is-small",
                    r#type: "button",
                    disabled: discarding,
                    onclick: move |_| on_discard.call(()),
                    if discarding { "Removing…" } else { "Remove them" }
                }
            }
        }
        if videos.is_empty() {
            p { class: "pharos-empty", id: "pharos-video-empty",
                "No videos are downloaded. Use Download on a film or episode."
            }
        }
        ul { class: "pharos-download-list", id: "pharos-video-list",
            for v in videos {
                li { key: "{v.item}", class: "pharos-download", "data-kind": "video",
                    div { class: "pharos-download-head",
                        div { class: "pharos-download-name",
                            span { "{v.title}" }
                            span { class: "pharos-muted",
                                "{quality_label(v.quality_bps)} \u{b7} {format_bytes(v.bytes())}"
                            }
                        }
                        span { class: "pharos-offline-chip", role: "status", "{state_line(&v)}" }
                        if v.ready() {
                            button {
                                class: "pharos-button pharos-button-secondary is-small",
                                r#type: "button",
                                aria_label: "Play {v.title}",
                                onclick: {
                                    let item = v.item.clone();
                                    move |_| on_play.call(item.clone())
                                },
                                "Play"
                            }
                        }
                        button {
                            class: "pharos-button pharos-button-danger is-small",
                            r#type: "button",
                            aria_label: "Remove {v.title} from this device",
                            onclick: {
                                let item = v.item.clone();
                                move |_| on_remove.call(item.clone())
                            },
                            "Remove"
                        }
                    }
                }
            }
        }
    }
}

/// The user whose downloads this screen may list, remove and play: the
/// authorized offline owner ([`offline_video::last_user`]), never the
/// account result itself. A replaced session's `me` can still hold the
/// previous user while the identity is fenced; it decides nothing here.
/// The account is read only so a sign-in renders the screen again.
fn use_user() -> Option<String> {
    let _ = try_use_context::<AccountMe>().map(|a| a.answer());
    offline_video::last_user()
}

/// Removes `item` for `rendered` (the owner its row was drawn for) only
/// while that is still the authorized owner ([`offline_video::last_user`],
/// read at the click): a sign-out or another user's sign-in since the row
/// was drawn leaves the download alone. Whether it removed.
fn remove_if_owner(store: &dyn VideoStore, rendered: Option<&str>, item: &str) -> bool {
    let now = offline_video::last_user();
    let Some(user) = rendered.filter(|u| now.as_deref() == Some(*u)) else {
        platform::info(&format!(
            "pharos: video remove verdict=skipped reason=owner_changed item={item} rendered_for_set={} owner_set={}",
            rendered.is_some(),
            now.is_some()
        ));
        return false;
    };
    platform::info(&format!("pharos: video removed item={item}"));
    store.remove(user, item);
    true
}

/// What a screen of downloads shows from the store: the user's videos and
/// how many folders could not be read. Polled as one value, so either one
/// changing reaches the screen (a sweep can drop an unreadable folder while
/// the list stays the same).
#[derive(Debug, Clone, Default, PartialEq)]
struct Listing {
    /// Whose videos these are: the user the listing was read for.
    owner: Option<String>,
    videos: Vec<KeptVideo>,
    unreadable: usize,
    /// Folders a removal left: see [`VideoStore::leftover_count`].
    leftover: usize,
    /// The user's usage, polled with the rows: music finishing a download
    /// changes it with no video row changing, and the page must show it.
    usage: Option<super::kept_video::Usage>,
    /// The store's cap, polled so a change made elsewhere (the cache coming
    /// up) renders the picker and the heading again, together.
    cap: u64,
}

/// The listing to show for `user` now: nothing without a store or a user,
/// except the unreadable count, which is the store's.
fn listing(store: Option<&dyn VideoStore>, user: Option<&str>) -> Listing {
    let Some(store) = store else {
        return Listing::default();
    };
    Listing {
        owner: user.map(str::to_owned),
        videos: user.map(|u| store.videos(u)).unwrap_or_default(),
        unreadable: store.unreadable_count(),
        leftover: store.leftover_count(),
        usage: user.map(|u| store.usage(u)),
        cap: store.cap_bytes(),
    }
}

/// The rows a screen shows for `user`: the polled listing's while it was
/// read for that user, else a fresh read for them. Never one user's rows
/// next to another user's Remove.
fn rows_for(
    shown: &Listing,
    user: Option<&str>,
    fresh: impl FnOnce() -> Listing,
) -> Vec<KeptVideo> {
    if shown.owner.as_deref() == user {
        return shown.videos.clone();
    }
    platform::info(&format!(
        "pharos: video listing verdict=reread reason=owner_changed from_set={} to_set={}",
        shown.owner.is_some(),
        user.is_some()
    ));
    fresh().videos
}

fn listing_now(store: &dyn VideoStore, user: Option<&str>) -> Listing {
    listing(Some(store), user)
}

/// The listing a poll sets, when it differs from the one shown.
fn refreshed(shown: &Listing, now: Listing) -> Option<Listing> {
    (*shown != now).then_some(now)
}

/// What a poll reads: the listing of the authorized offline owner as it
/// is now ([`offline_video::last_user`]), not of the user the last render
/// saw. A fence that props and signals do not show (the owner becoming
/// nobody) changes the listing's owner, and the poll sets it: the screen
/// renders again for whoever is authorized now.
fn polled(store: Option<&dyn VideoStore>) -> Listing {
    listing(store, offline_video::last_user().as_deref())
}

/// The user's listing, re-read every [`POLL_MS`] while the component lives.
fn use_listing() -> Signal<Listing> {
    let read = move || polled(video_store().as_deref());
    let mut shown = use_signal(read);
    use_future(move || async move {
        poll_every(
            platform::SLEEP_IS_REAL,
            || platform::sleep_ms(POLL_MS),
            || {
                let (was, was_left) = {
                    let s = shown.peek();
                    (s.unreadable, s.leftover)
                };
                let next = refreshed(&shown.peek(), read());
                if let Some(now) = next {
                    if now.owner != shown.peek().owner {
                        platform::info(&format!(
                            "pharos: video listing owner verdict=changed authorized_user={}",
                            now.owner.is_some()
                        ));
                    }
                    if now.unreadable != was {
                        platform::info(&format!(
                            "pharos: video listing unreadable verdict=changed from={was} to={}",
                            now.unreadable
                        ));
                    }
                    if now.leftover != was_left {
                        platform::info(&format!(
                            "pharos: video listing leftover verdict=changed from={was_left} to={}",
                            now.leftover
                        ));
                    }
                    shown.set(now);
                }
                true
            },
        )
        .await;
    });
    shown
}

/// The Download control of a film's or episode's title page. Nothing
/// without a registered store.
#[component]
pub fn VideoKeep(detail: Detail, version: String) -> Element {
    let Some(store) = shown_video_store() else {
        return rsx! {};
    };
    let ctx = use_context::<Ctx>();
    let user = use_user();
    let listing = use_listing();
    let id = detail.item.id;
    let item = id.to_string();
    let mut quality = use_signal(|| DEFAULT_QUALITY_BPS);
    let mut busy = use_signal(|| false);
    let mut message = use_signal(|| None::<String>);
    let video = rows_for(&listing.read(), user.as_deref(), || {
        listing_now(&*store, user.as_deref())
    })
    .into_iter()
    .find(|v| v.item == item);
    let remove_user = user.clone();
    let download_owner = user.clone();
    let remove_item = item.clone();
    let remove_store = store.clone();
    let _ = download_driver::QUEUE_CHANGED.read();
    let queued = match (&video, user.as_deref()) {
        (None, Some(u)) => queued_status(&download_driver::load(), u, &item),
        _ => None,
    };
    let retry_queued = queued.as_ref().is_some_and(|q| q.failed);
    let retry_item = item.clone();
    rsx! {
        VideoKeepControl {
            video,
            queued,
            quality: quality(),
            busy: busy(),
            message: message(),
            on_quality: move |bps| quality.set(bps),
            on_download: move |()| {
                let (detail, version, store) = (detail.clone(), version.clone(), store.clone());
                let bps = *quality.peek();
                let rendered = download_owner.clone();
                // A queue entry that failed for good is carried on by the
                // queue's own Retry, not queued a second time.
                if retry_queued {
                    if let Some(u) = rendered.as_deref() {
                        download_driver::retry(ctx, u, &retry_item);
                    }
                    return;
                }
                busy.set(true);
                message.set(None);
                spawn(async move {
                    match download_driver::enqueue(
                        ctx,
                        &detail,
                        &version,
                        bps,
                        &*store,
                        rendered.as_deref(),
                    )
                    .await
                    {
                        Ok(()) => {}
                        Err(line) => {
                            platform::warn(&format!(
                                "pharos: video keep refused item={id} quality_bps={bps} reason={line}"
                            ));
                            message.set(Some(line));
                        }
                    }
                    busy.set(false);
                });
            },
            on_remove: move |()| {
                if remove_if_owner(&*remove_store, remove_user.as_deref(), &remove_item) {
                    if let Some(u) = remove_user.as_deref() {
                        download_driver::forget(u, &remove_item);
                    }
                }
                message.set(None);
            },
        }
    }
}

/// The ordinal of a place in line: 1st, 2nd, 3rd, 4th, 11th, 21st.
pub fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// What the title page says of a queued download with no record yet, and
/// whether it failed for good (Retry is offered; a queued or paused one
/// offers neither Download nor Retry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedKeep {
    pub line: String,
    pub failed: bool,
}

/// What the title page says of `item` while it is queued with no record
/// yet: its place, paused, or why it could not start.
fn queued_status(queue: &download_queue::Queue, user: &str, item: &str) -> Option<QueuedKeep> {
    let mine: Vec<_> = queue.entries.iter().filter(|e| e.user == user).collect();
    let at = mine.iter().position(|e| e.item == item)?;
    let e = mine[at];
    let failed = e.failed.is_some();
    let line = match (&e.failed, e.paused) {
        (Some(reason), _) => format!("Failed: {reason}"),
        (None, true) => "Paused".to_owned(),
        (None, false) => format!("Queued \u{b7} {}", ordinal(at + 1)),
    };
    Some(QueuedKeep { line, failed })
}

/// A queue row's state in words.
fn row_line(state: &RowState, estimate: u64) -> String {
    match state {
        RowState::Downloading { percent, bytes } => format!(
            "Downloading {percent}% \u{b7} {} of {}",
            format_bytes(*bytes),
            format_bytes(estimate)
        ),
        RowState::Waiting {
            halt,
            bytes,
            in_secs,
        } => {
            let kept = if *bytes > 0 {
                format!(" \u{b7} {} kept", format_bytes(*bytes))
            } else {
                String::new()
            };
            format!("{}{kept}", download_queue::waiting_line(*halt, *in_secs))
        }
        RowState::Queued => "Queued".to_owned(),
        RowState::Paused { percent, bytes } => {
            format!("Paused at {percent}% \u{b7} {} kept", format_bytes(*bytes))
        }
        RowState::Failed { reason } => format!("Failed: {reason}"),
    }
}

fn row_kind(state: &RowState) -> &'static str {
    match state {
        RowState::Downloading { .. } => "downloading",
        RowState::Waiting { .. } => "waiting",
        RowState::Queued => "queued",
        RowState::Paused { .. } => "paused",
        RowState::Failed { .. } => "failed",
    }
}

/// The download queue (ADR 0148): every download not on the device yet, in
/// the order they run, each with what it is doing and what the viewer can
/// do. Nothing when the queue is empty. The Downloads page's one polled
/// `listing`, so the page reads the store once for its two sections.
#[component]
fn VideoQueue(listing: Signal<Listing>) -> Element {
    // A waiting row counts down to its next try: it renders again each
    // second while any row waits, and not otherwise.
    let mut clock = use_signal(|| 0u32);
    let waiting = use_hook(|| std::rc::Rc::new(std::cell::Cell::new(false)));
    let counting = std::rc::Rc::clone(&waiting);
    use_future(move || {
        let waiting = std::rc::Rc::clone(&counting);
        async move {
            poll_every(
                platform::SLEEP_IS_REAL,
                || platform::sleep_ms(COUNTDOWN_MS),
                || {
                    if waiting.get() {
                        *clock.write() += 1;
                    }
                    true
                },
            )
            .await;
        }
    });
    let _ = clock.read();
    let Some(store) = shown_video_store() else {
        return rsx! {};
    };
    let user = use_user();
    let _ = download_driver::QUEUE_CHANGED.read();
    let Some(user) = user else {
        return rsx! {};
    };
    let records = rows_for(&listing.read(), Some(&user), || {
        listing_now(&*store, Some(&user))
    });
    let queue = download_driver::load();
    let now = platform::now_ms().max(0.0) as u64;
    let rows = download_queue::rows(&queue, &records, &download_driver::tries(), &user, now);
    waiting.set(any_waiting(&rows));
    if rows.is_empty() {
        return rsx! {};
    }
    rsx! {
        h3 { id: "video-queue-title", "Queue" }
        ol { class: "pharos-download-list", id: "pharos-video-queue", aria_labelledby: "video-queue-title",
            for row in rows {
                QueueRow { key: "{row.item}", row: row.clone(), user: user.clone(), store: StoreRef(store.clone()) }
            }
        }
    }
}

/// How often a waiting row's countdown is drawn again.
const COUNTDOWN_MS: u32 = 1_000;

/// Whether any row counts down to a try, so the queue draws again each
/// [`COUNTDOWN_MS`].
fn any_waiting(rows: &[download_queue::Row]) -> bool {
    rows.iter()
        .any(|r| matches!(r.state, RowState::Waiting { .. }))
}

/// A store handle for props (compared by pointer).
#[derive(Clone)]
pub struct StoreRef(pub std::sync::Arc<dyn VideoStore>);

impl PartialEq for StoreRef {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

#[component]
fn QueueRow(row: download_queue::Row, user: String, store: StoreRef) -> Element {
    let ctx = use_context::<Ctx>();
    let line = row_line(&row.state, row.estimate);
    let kind = row_kind(&row.state);
    let progress = match row.state {
        RowState::Downloading { percent, .. } | RowState::Paused { percent, .. } => Some(percent),
        RowState::Waiting { bytes, .. } if row.estimate > 0 => {
            Some(u32::try_from((bytes.saturating_mul(100) / row.estimate).min(100)).unwrap_or(100))
        }
        _ => None,
    };
    let (item, title) = (row.item.clone(), row.title.clone());
    let act = move |what: &'static str| {
        let (user, item, store) = (user.clone(), item.clone(), store.0.clone());
        move |_| match what {
            "pause" => {
                if let Err(e) = download_driver::pause(&*store, &user, &item) {
                    platform::warn(&format!(
                        "pharos: video queue pause failed item={item} error={e}"
                    ));
                }
            }
            "resume" => download_driver::resume(&user, &item),
            "retry" => download_driver::retry(ctx, &user, &item),
            "up" => download_driver::shift(&user, &item, true),
            "down" => download_driver::shift(&user, &item, false),
            _ => {
                if remove_if_owner(&*store, Some(&user), &item) {
                    download_driver::forget(&user, &item);
                }
            }
        }
    };
    rsx! {
        li { class: "pharos-download", "data-kind": "video", "data-queue": kind,
            div { class: "pharos-download-head",
                span { class: "pharos-queue-place", aria_label: "Place {row.position}", "{row.position}" }
                div { class: "pharos-download-name",
                    span { "{title}" }
                    span { class: "pharos-muted",
                        "{quality_label(row.quality_bps)} \u{b7} about {format_bytes(row.estimate)}"
                    }
                    span { class: "pharos-queue-state", "data-state": kind, role: "status", "{line}" }
                }
                match &row.state {
                    RowState::Downloading { .. } => rsx! {
                        button { class: "pharos-button pharos-button-secondary is-small", r#type: "button",
                            aria_label: "Pause {title}", onclick: act("pause"), "Pause" }
                    },
                    RowState::Waiting { .. } => rsx! {
                        button { class: "pharos-button pharos-button-secondary is-small", r#type: "button",
                            aria_label: "Try {title} now", onclick: act("resume"), "Try now" }
                        button { class: "pharos-button pharos-button-quiet is-small", r#type: "button",
                            aria_label: "Pause {title}", onclick: act("pause"), "Pause" }
                    },
                    RowState::Paused { .. } => rsx! {
                        button { class: "pharos-button pharos-button-secondary is-small", r#type: "button",
                            aria_label: "Resume {title}", onclick: act("resume"), "Resume" }
                    },
                    RowState::Failed { .. } => rsx! {
                        button { class: "pharos-button pharos-button-secondary is-small", r#type: "button",
                            aria_label: "Retry {title}", onclick: act("retry"), "Retry" }
                    },
                    RowState::Queued => rsx! {},
                }
                if row.can_move_up {
                    button { class: "pharos-button pharos-button-quiet is-small", r#type: "button",
                        aria_label: "Move {title} up", onclick: act("up"), "\u{2191}" }
                }
                if row.can_move_down {
                    button { class: "pharos-button pharos-button-quiet is-small", r#type: "button",
                        aria_label: "Move {title} down", onclick: act("down"), "\u{2193}" }
                }
                button { class: "pharos-button pharos-button-danger is-small", r#type: "button",
                    aria_label: "Remove {title} from the queue", onclick: act("remove"), "Remove" }
            }
            if let Some(p) = progress {
                div { class: "pharos-queue-bar", "data-state": kind, role: "presentation",
                    i { style: "width: {p}%" }
                }
            }
        }
    }
}

/// The Downloads page's videos. Nothing without a registered store.
#[component]
pub fn VideoDownloads(
    /// Show the space allowance picker.
    #[props(default = true)]
    settings: bool,
) -> Element {
    let Some(store) = shown_video_store() else {
        return rsx! {};
    };
    let user = use_user();
    let listing = use_listing();
    let nav = use_context::<Nav>();
    // Both figures from one read: the polled one while it was read for this
    // user, else a fresh one.
    let usage = {
        let shown = listing.read();
        if shown.owner == user {
            shown.usage
        } else {
            user.as_deref().map(|u| store.usage(u))
        }
    };
    let used = usage.map_or(0, |u| u.used);
    let videos_used = usage.map(|u| u.video);
    let remove_user = user.clone();
    let remove_store = store.clone();
    // App-wide, not this view's: a discard outlives the page that started
    // it, and a page mounted again shows the one still running.
    let discard = DISCARD.read().clone();
    // Read at each render. The listing polls it with the videos, so a change
    // a sweep makes renders again even when the list is the same; a discard
    // sets its result, which renders again too.
    let unreadable = store.unreadable_count();
    let leftover = store.leftover_count();
    let discard_store = store.clone();
    // Read at each render, for the picker and the heading alike: the poll
    // renders again when the cap changes ([`Listing::cap`]), a pick at once.
    let cap = store.cap_bytes();
    let mut shown = listing;
    let mut cap_notice = use_signal(|| None::<String>);
    let cap_store = store.clone();
    rsx! {
        section { class: "pharos-downloads", aria_labelledby: "video-downloads-title",
            h2 { id: "video-downloads-title", "Downloaded videos" }
            if settings {
                VideoCapSetting {
                cap,
                notice: cap_notice(),
                on_pick: move |option: String| match apply_cap(&*cap_store, &option) {
                    Ok(bytes) => {
                        platform::info(&format!("pharos: video cap set bytes={bytes}"));
                        shown.write().cap = bytes;
                        cap_notice.set(None);
                    }
                    Err(e) => {
                        platform::warn(&format!("pharos: video cap refused option={option:?} error={e}"));
                        cap_notice.set(Some(format!("The space allowance was not changed: {e}")));
                    }
                },
                }
            }
            VideoQueue { listing }
            VideoList {
                videos: rows_for(&listing.read(), user.as_deref(), || listing_now(&*store, user.as_deref()))
                    .into_iter()
                    .filter(KeptVideo::ready)
                    .collect::<Vec<_>>(),
                used,
                videos_used,
                used_known: usage.is_some(),
                cap,
                problem: discard.problem,
                unreadable,
                leftover,
                discarding: discard.running,
                on_discard: move |()| start_discard(discard_store.clone()),
                on_play: move |item: String| match item.parse::<ItemRef>() {
                    Ok(id) => nav.0.call(Route::Watch(id)),
                    Err(e) => platform::warn(&format!("pharos: video play failed item={item} error={e}")),
                },
                on_remove: move |item: String| {
                    if remove_if_owner(&*remove_store, remove_user.as_deref(), &item) {
                        if let Some(u) = remove_user.as_deref() {
                            download_driver::forget(u, &item);
                        }
                    }
                },
            }
        }
    }
}

/// The discard of unreadable folders: whether one runs, and what the last
/// one left to say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Discard {
    running: bool,
    problem: Option<String>,
}

/// Held for the whole app, not by the Downloads page: a page left while
/// the folders are deleting would otherwise come back with the button
/// enabled (a second discard beside the first) and lose the result.
static DISCARD: GlobalSignal<Discard> = Signal::global(Discard::default);

/// Discards the folders whose record cannot be read (`discard_unreadable`
/// deletes folders, so it can take a while). The deleting runs on its own
/// thread, off the UI executor, and its task is the app's, not the page's:
/// `DISCARD` says it runs until it ends (the button is disabled meanwhile)
/// and then holds its result. Runs once at a time, whatever page asks.
fn start_discard(store: std::sync::Arc<dyn super::kept_video::VideoStore>) {
    if DISCARD.peek().running {
        platform::info("pharos: video discard unreadable verdict=skipped reason=already_running");
        return;
    }
    {
        let mut d = DISCARD.write();
        d.running = true;
        d.problem = None;
    }
    spawn_forever(async move {
        let outcome = off_ui(move || store.discard_unreadable()).await;
        platform::info(&format!(
            "pharos: video discard unreadable result={outcome:?}"
        ));
        let problem = match outcome {
            Ok(Ok(_)) => None,
            Ok(Err(e)) => Some(format!("Some downloads could not be removed: {e}")),
            Err(OffUi::Stopped) => {
                Some("Some downloads could not be removed: the removal stopped".to_owned())
            }
            Err(OffUi::NotStarted(e)) => Some(format!(
                "Some downloads could not be removed: the removal could not start ({e}). Try again."
            )),
        };
        *DISCARD.write() = Discard {
            running: false,
            problem,
        };
    });
}

/// Why work sent off the UI gave no result.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OffUi {
    /// Its thread could not be made (the system refused one).
    NotStarted(String),
    /// Its thread ended without a result (it panicked).
    Stopped,
}

#[cfg(test)]
thread_local! {
    /// Makes the next `off_ui` on this thread fail to start its thread.
    static OFF_UI_SPAWN_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `work` on a thread of its own and resolves with its result, so a
/// blocking job (deleting folders) never holds the UI executor. Err when
/// the thread could not be made, or ended without a result.
async fn off_ui<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Result<T, OffUi> {
    use futures_util::task::AtomicWaker;
    use std::sync::{Arc, Mutex};

    struct Slot<T> {
        value: Mutex<Option<Option<T>>>,
        waker: AtomicWaker,
    }
    /// Delivers `None` if the thread unwinds before it delivers a value.
    struct Deliver<T>(Arc<Slot<T>>, Option<T>);
    impl<T> Drop for Deliver<T> {
        fn drop(&mut self) {
            let value = self.1.take();
            if let Ok(mut held) = self.0.value.lock() {
                *held = Some(value);
            }
            self.0.waker.wake();
        }
    }

    let slot = Arc::new(Slot {
        value: Mutex::new(None),
        waker: AtomicWaker::new(),
    });
    let sender = Arc::clone(&slot);
    let job = move || {
        let mut deliver = Deliver(sender, None);
        deliver.1 = Some(work());
    };
    #[cfg(test)]
    let job = (!OFF_UI_SPAWN_FAULT.with(|f| f.replace(false))).then_some(job);
    #[cfg(not(test))]
    let job = Some(job);
    let spawned = match job {
        Some(job) => std::thread::Builder::new()
            .name("pharos-off-ui".to_owned())
            .spawn(job)
            .map(drop),
        None => Err(std::io::Error::other("no thread for the test")),
    };
    if let Err(e) = spawned {
        platform::warn(&format!(
            "pharos: video off_ui verdict=not_started reason=thread_refused error={e}"
        ));
        return Err(OffUi::NotStarted(e.to_string()));
    }
    std::future::poll_fn(move |cx| {
        slot.waker.register(cx.waker());
        match slot.value.lock() {
            Ok(mut held) => match held.take() {
                Some(value) => std::task::Poll::Ready(value),
                None => std::task::Poll::Pending,
            },
            Err(_) => std::task::Poll::Ready(None),
        }
    })
    .await
    .ok_or(OffUi::Stopped)
}

/// Applies a cap option to the store: the bytes set, or why not.
pub fn apply_cap(store: &dyn VideoStore, option: &str) -> Result<u64, String> {
    let bytes = cap_from_option(option).ok_or_else(|| format!("{option:?} is not a choice"))?;
    store.set_cap_bytes(bytes)?;
    Ok(bytes)
}

/// The space allowance picker for downloaded video.
#[component]
pub fn VideoCapSetting(cap: u64, notice: Option<String>, on_pick: EventHandler<String>) -> Element {
    rsx! {
        div { class: "pharos-setting pharos-video-cap",
            div { class: "pharos-setting-text",
                span { id: "video-cap-label", class: "pharos-setting-name", "Space for downloads" }
                span { id: "video-cap-help", class: "pharos-setting-help",
                    "Downloads stop at this size. Lowering it keeps what is already saved."
                }
            }
            Choices {
                id: "video-cap",
                labelled_by: "video-cap-label",
                described_by: "video-cap-help",
                options: cap_options(),
                selected: cap_selected(cap),
                on_pick: move |o| on_pick.call(o),
            }
            if let Some(n) = notice {
                p { class: "pharos-setting-help", role: "alert", "{n}" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::cell::RefCell;

    /// A store published after the app mounted (the second registration
    /// returned while the first was still opening it) reaches a component
    /// that rendered without one: the root's watch re-renders it, with no
    /// remount and nothing blocking.
    #[test]
    fn a_store_published_after_mount_reaches_a_component_that_rendered_without_it() {
        static SLOT: Slot<u32> = Slot::new();
        static SEEN: GlobalSignal<u32> = Signal::global(|| 0);
        thread_local! {
            static SAW: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
        }
        #[component]
        fn Reader() -> Element {
            let _ = SEEN.read();
            SAW.with(|s| s.set(SLOT.get().map(|p| *p)));
            rsx! {}
        }
        fn root() -> Element {
            use_publish_watch(&SLOT, &SEEN, "test_store");
            rsx! { Reader {} }
        }
        let mut dom = VirtualDom::new(root);
        dom.rebuild_in_place();
        assert_eq!(
            SAW.with(std::cell::Cell::get),
            None,
            "a store before it was set"
        );
        std::thread::spawn(|| SLOT.set(std::sync::Arc::new(7)).unwrap())
            .join()
            .unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            for _ in 0..5 {
                if SAW.with(std::cell::Cell::get).is_some() {
                    break;
                }
                let woke =
                    tokio::time::timeout(std::time::Duration::from_secs(2), dom.wait_for_work())
                        .await;
                assert!(woke.is_ok(), "no work came: the reader was never told");
                dom.render_immediate_to_vec();
            }
        });
        assert_eq!(SAW.with(std::cell::Cell::get), Some(7));
    }
    use super::*;

    #[test]
    fn places_in_line_read_as_ordinals() {
        let got: Vec<String> = [1, 2, 3, 4, 11, 12, 13, 21, 22, 103].map(ordinal).into();
        assert_eq!(
            got,
            ["1st", "2nd", "3rd", "4th", "11th", "12th", "13th", "21st", "22nd", "103rd"]
        );
    }

    /// The title page names a queued download's place among the user's
    /// own, and says when it is paused or could not start.
    #[test]
    fn a_queued_download_says_its_place_on_the_title_page() {
        let entry = |user: &str, item: &str| download_queue::QueueEntry {
            user: user.into(),
            item: item.into(),
            version: String::new(),
            title: "Invented Film".into(),
            kind: "movie".into(),
            runtime_ms: 1,
            subtitles: Vec::new(),
            quality_bps: 1,
            paused: false,
            failed: None,
        };
        let mut queue = download_queue::Queue {
            entries: vec![entry("u", "1"), entry("other", "9"), entry("u", "2")],
        };
        assert_eq!(
            queued_status(&queue, "u", "2"),
            Some(QueuedKeep {
                line: "Queued \u{b7} 2nd".into(),
                failed: false
            })
        );
        assert_eq!(queued_status(&queue, "u", "9"), None);
        queue.set_paused("u", "2", true);
        assert_eq!(
            queued_status(&queue, "u", "2"),
            Some(QueuedKeep {
                line: "Paused".into(),
                failed: false
            })
        );
        queue.entries[2].failed = Some("no room".into());
        assert_eq!(
            queued_status(&queue, "u", "2"),
            Some(QueuedKeep {
                line: "Failed: no room".into(),
                failed: true
            })
        );
    }

    #[test]
    fn queue_rows_say_what_they_do_in_words() {
        assert_eq!(
            row_line(
                &RowState::Downloading {
                    percent: 43,
                    bytes: 1024
                },
                2048
            ),
            "Downloading 43% \u{b7} 1 KiB of 2 KiB"
        );
        assert_eq!(
            row_line(
                &RowState::Waiting {
                    halt: Halt::Unreachable,
                    bytes: 1024,
                    in_secs: 5
                },
                2048
            ),
            "Waiting for a connection \u{b7} tries again in 5 s \u{b7} 1 KiB kept"
        );
        assert_eq!(row_kind(&RowState::Queued), "queued");
    }

    /// The queue draws its countdown again only while a row waits.
    #[test]
    fn the_countdown_runs_only_while_a_row_waits() {
        let row = |state| download_queue::Row {
            item: "1".into(),
            title: "t".into(),
            quality_bps: 0,
            estimate: 0,
            position: 1,
            state,
            can_move_up: false,
            can_move_down: false,
        };
        let waiting = row(RowState::Waiting {
            halt: Halt::Unreachable,
            bytes: 0,
            in_secs: 4,
        });
        assert!(any_waiting(&[row(RowState::Queued), waiting]));
        assert!(!any_waiting(&[
            row(RowState::Queued),
            row(RowState::Paused {
                percent: 0,
                bytes: 0
            })
        ]));
        assert!(!any_waiting(&[]));
    }

    /// A halted record is not offered Retry on its title page: the queue
    /// carries it on by itself.
    #[test]
    fn a_halted_download_waits_rather_than_failing() {
        let mut v = offline_video::tests::video("8", "", KeptState::Failed { reason: "x".into() });
        v.halt = Some(Halt::SessionEnded);
        assert_eq!(state_line(&v), "Waiting to carry on");
        v.halt = Some(Halt::Paused);
        assert_eq!(state_line(&v), "Paused");
        v.halt = None;
        assert_eq!(state_line(&v), "Failed: x");
    }

    /// Discarding the unreadable folders deletes folders: it runs off the
    /// UI executor, so a render goes on meanwhile with the button disabled,
    /// and the result reaches `problem` once the work ends.
    #[test]
    fn a_discard_runs_off_the_ui_with_its_button_disabled_until_it_ends() {
        use super::super::kept_video::{KeepVideoRefusal, KeepVideoRequest, VideoStore};
        use std::sync::{Arc, Mutex};
        /// Says when the removal starts, is held until the test lets it
        /// finish, and records that it finished.
        struct Slow {
            held: Mutex<std::sync::mpsc::Receiver<()>>,
            started: Mutex<std::sync::mpsc::Sender<()>>,
            finished: Arc<std::sync::atomic::AtomicBool>,
        }
        impl VideoStore for Slow {
            fn videos(&self, _: &str) -> Vec<KeptVideo> {
                Vec::new()
            }
            fn start(&self, _: KeepVideoRequest) -> Result<(), KeepVideoRefusal> {
                Ok(())
            }
            fn remove(&self, _: &str, _: &str) {}
            fn save_position(&self, _: &str, _: &str, _: u64) {}
            fn cap_bytes(&self) -> u64 {
                0
            }
            fn used_bytes(&self, _: &str) -> u64 {
                0
            }
            fn free_bytes(&self) -> u64 {
                0
            }
            fn unreadable_count(&self) -> usize {
                1
            }
            fn discard_unreadable(&self) -> Result<u64, String> {
                let _ = self.started.lock().unwrap().send(());
                // Held until the test lets it finish.
                let _ = self.held.lock().unwrap().recv();
                self.finished
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                Err("one folder stayed".to_owned())
            }
        }
        thread_local! {
            static STORE: RefCell<Option<Arc<dyn VideoStore>>> = const { RefCell::new(None) };
        }
        fn root() -> Element {
            use_hook(|| {
                let store = STORE.with(|s| s.borrow().clone()).unwrap();
                start_discard(store);
            });
            let discard = DISCARD.read().clone();
            rsx! {
                VideoList {
                    videos: vec![],
                    used: 0,
                    cap: 0,
                    on_play: |_| {},
                    on_remove: |_| {},
                    problem: discard.problem,
                    unreadable: 1,
                    discarding: discard.running,
                }
            }
        }
        let (release, held) = std::sync::mpsc::channel();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        STORE.with(|s| {
            *s.borrow_mut() = Some(Arc::new(Slow {
                held: Mutex::new(held),
                started: Mutex::new(started_tx),
                finished: finished.clone(),
            }))
        });
        // A watchdog: an inline discard would block the UI executor until it
        // fires, and the checks below then see the removal already over.
        let watchdog = release.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(3));
            let _ = watchdog.send(());
        });
        let started = std::time::Instant::now();
        let mut dom = VirtualDom::new(root);
        dom.rebuild_in_place();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "the discard ran on the UI executor"
        );
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("disabled"), "{html}");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // Drive the UI until the removal has started: the spawned task runs
        // only once the executor is polled.
        let seen = rt.block_on(async {
            let start = std::time::Instant::now();
            while start.elapsed() < std::time::Duration::from_secs(5) {
                if started_rx.try_recv().is_ok() {
                    return true;
                }
                let _ =
                    tokio::time::timeout(std::time::Duration::from_millis(50), dom.wait_for_work())
                        .await;
                dom.render_immediate_to_vec();
            }
            false
        });
        assert!(seen, "the removal never started");
        // A UI update of its own while the removal is still held.
        dom.mark_dirty(ScopeId::APP);
        dom.render_immediate_to_vec();
        assert!(
            !finished.load(std::sync::atomic::Ordering::SeqCst),
            "the removal ended before the UI rendered beside it: it ran on the UI executor"
        );
        let html = dioxus_ssr::render(&dom);
        assert!(
            html.contains("disabled") && !html.contains("one folder stayed"),
            "{html}"
        );
        release.send(()).unwrap();
        let html = rt.block_on(async {
            let start = std::time::Instant::now();
            loop {
                let html = dioxus_ssr::render(&dom);
                if html.contains("one folder stayed")
                    || start.elapsed() > std::time::Duration::from_secs(3)
                {
                    break html;
                }
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    dom.wait_for_work(),
                )
                .await;
                dom.render_immediate_to_vec();
            }
        });
        assert!(
            html.contains("Some downloads could not be removed: one folder stayed"),
            "{html}"
        );
        assert!(
            !html.contains("disabled"),
            "the button stayed disabled: {html}"
        );
    }

    /// A discard outlives the page that started it: the page mounted again
    /// while the folders are deleting shows it running (no second discard
    /// starts), and its result reaches the page shown when it ends.
    #[test]
    fn a_discard_survives_its_page_unmounting() {
        use super::super::kept_video::{KeepVideoRefusal, KeepVideoRequest, VideoStore};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        struct Slow {
            held: Mutex<std::sync::mpsc::Receiver<()>>,
            calls: Arc<AtomicUsize>,
        }
        impl VideoStore for Slow {
            fn videos(&self, _: &str) -> Vec<KeptVideo> {
                Vec::new()
            }
            fn start(&self, _: KeepVideoRequest) -> Result<(), KeepVideoRefusal> {
                Ok(())
            }
            fn remove(&self, _: &str, _: &str) {}
            fn save_position(&self, _: &str, _: &str, _: u64) {}
            fn cap_bytes(&self) -> u64 {
                0
            }
            fn used_bytes(&self, _: &str) -> u64 {
                0
            }
            fn free_bytes(&self) -> u64 {
                0
            }
            fn unreadable_count(&self) -> usize {
                1
            }
            fn discard_unreadable(&self) -> Result<u64, String> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let _ = self.held.lock().unwrap().recv();
                Err("one folder stayed".to_owned())
            }
        }
        thread_local! {
            static STORE: RefCell<Option<Arc<dyn VideoStore>>> = const { RefCell::new(None) };
            static SHOWN: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
        }
        #[component]
        fn Page() -> Element {
            // Each mount presses the button.
            use_hook(|| start_discard(STORE.with(|s| s.borrow().clone()).unwrap()));
            let discard = DISCARD.read().clone();
            rsx! {
                VideoList {
                    videos: vec![],
                    used: 0,
                    cap: 0,
                    on_play: |_| {},
                    on_remove: |_| {},
                    problem: discard.problem,
                    unreadable: 1,
                    discarding: discard.running,
                }
            }
        }
        fn root() -> Element {
            rsx! {
                if SHOWN.with(std::cell::Cell::get) {
                    Page {}
                }
            }
        }
        let (release, held) = std::sync::mpsc::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        STORE.with(|s| {
            *s.borrow_mut() = Some(Arc::new(Slow {
                held: Mutex::new(held),
                calls: calls.clone(),
            }))
        });
        let watchdog = release.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let _ = watchdog.send(());
        });
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut dom = VirtualDom::new(root);
        dom.rebuild_in_place();
        let drive = |dom: &mut VirtualDom, until: &dyn Fn(&VirtualDom) -> bool| {
            rt.block_on(async {
                let start = std::time::Instant::now();
                while !until(dom) && start.elapsed() < std::time::Duration::from_secs(3) {
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_millis(50),
                        dom.wait_for_work(),
                    )
                    .await;
                    dom.render_immediate_to_vec();
                }
            });
        };
        drive(&mut dom, &|_| calls.load(Ordering::SeqCst) == 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the removal never started");
        // Leave the page, then come back while the folders are deleting.
        SHOWN.with(|s| s.set(false));
        dom.mark_dirty(ScopeId::APP);
        dom.render_immediate_to_vec();
        assert!(!dioxus_ssr::render(&dom).contains("Remove them"));
        SHOWN.with(|s| s.set(true));
        dom.mark_dirty(ScopeId::APP);
        dom.render_immediate_to_vec();
        let html = dioxus_ssr::render(&dom);
        assert!(
            html.contains("disabled"),
            "the page came back enabled: {html}"
        );
        release.send(()).unwrap();
        drive(&mut dom, &|dom| {
            dioxus_ssr::render(dom).contains("one folder stayed")
        });
        let html = dioxus_ssr::render(&dom);
        assert!(
            html.contains("Some downloads could not be removed: one folder stayed"),
            "the result was lost with the page: {html}"
        );
        assert!(!html.contains("disabled"), "a retry is not offered: {html}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "a second discard ran");
    }

    /// A poll refreshes the screen when only the unreadable count changed:
    /// a sweep can drop an unreadable folder while the list stays empty.
    #[test]
    fn a_poll_refreshes_an_unreadable_count_that_changed_alone() {
        let shown = Listing {
            owner: None,
            videos: vec![],
            unreadable: 1,
            leftover: 0,
            usage: None,
            cap: 0,
        };
        let now = Listing {
            owner: None,
            videos: vec![],
            unreadable: 0,
            leftover: 0,
            usage: None,
            cap: 0,
        };
        assert_eq!(
            refreshed(&shown, now.clone()),
            Some(now),
            "the count changed and the screen kept the old one"
        );
        assert_eq!(refreshed(&shown, shown.clone()), None);
    }

    /// A store whose usage is given, and whose separate figures must not
    /// be read: the screen takes both from one `usage` call.
    struct Shared(std::sync::atomic::AtomicU64);
    impl VideoStore for Shared {
        fn videos(&self, _: &str) -> Vec<KeptVideo> {
            Vec::new()
        }
        fn start(
            &self,
            _: super::super::kept_video::KeepVideoRequest,
        ) -> Result<(), super::super::kept_video::KeepVideoRefusal> {
            Ok(())
        }
        fn remove(&self, _: &str, _: &str) {}
        fn save_position(&self, _: &str, _: &str, _: u64) {}
        fn cap_bytes(&self) -> u64 {
            0
        }
        fn used_bytes(&self, _: &str) -> u64 {
            unreachable!("used_bytes read apart from the video subtotal")
        }
        fn video_bytes(&self, _: &str) -> u64 {
            unreachable!("video_bytes read apart from the shared usage")
        }
        fn usage(&self, _: &str) -> super::super::kept_video::Usage {
            super::super::kept_video::Usage {
                video: 100,
                used: 100 + self.0.load(std::sync::atomic::Ordering::SeqCst),
            }
        }
        fn free_bytes(&self) -> u64 {
            0
        }
    }

    /// CR 4644634617: a cap changed elsewhere (the cache coming up) reaches
    /// the poll, which renders the picker and the heading again.
    #[test]
    fn a_poll_refreshes_a_cap_that_changed_alone() {
        use std::sync::atomic::{AtomicU64, Ordering};
        struct Capped(AtomicU64);
        impl VideoStore for Capped {
            fn videos(&self, _: &str) -> Vec<KeptVideo> {
                Vec::new()
            }
            fn start(
                &self,
                _: super::super::kept_video::KeepVideoRequest,
            ) -> Result<(), super::super::kept_video::KeepVideoRefusal> {
                Ok(())
            }
            fn remove(&self, _: &str, _: &str) {}
            fn save_position(&self, _: &str, _: &str, _: u64) {}
            fn cap_bytes(&self) -> u64 {
                self.0.load(Ordering::SeqCst)
            }
            fn used_bytes(&self, _: &str) -> u64 {
                0
            }
            fn free_bytes(&self) -> u64 {
                0
            }
        }
        let store = Capped(AtomicU64::new(1024));
        let shown = listing(Some(&store), Some("A"));
        assert_eq!(shown.cap, 1024);
        store.0.store(4096, Ordering::SeqCst);
        let now = listing(Some(&store), Some("A"));
        assert_eq!(
            refreshed(&shown, now.clone()).map(|l| l.cap),
            Some(4096),
            "the cap changed and the screen kept the old one"
        );
    }

    /// CR 4640611377: music finishing a download changes the shared usage
    /// with no video row or folder count changing; the poll still renders
    /// it again. CR 4640611383: both figures come from one `usage` read.
    #[test]
    fn a_poll_refreshes_a_usage_that_changed_alone() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let store = Shared(AtomicU64::new(0));
        let shown = listing(Some(&store), Some("A"));
        assert_eq!(
            shown.usage,
            Some(super::super::kept_video::Usage {
                video: 100,
                used: 100
            })
        );
        assert_eq!(refreshed(&shown, listing(Some(&store), Some("A"))), None);
        store.0.store(20, Ordering::SeqCst);
        let now = listing(Some(&store), Some("A"));
        assert_eq!(
            refreshed(&shown, now.clone()),
            Some(now),
            "music's change left the old usage on screen"
        );
        assert_eq!(listing(Some(&store), None).usage, None);
    }

    /// CR 4642574429: a poll reads the authorized owner as it is now. Once
    /// a replacement session fences A, the polled listing is nobody's and
    /// differs from A's on screen, so the screen renders again without A's
    /// rows, though no prop or signal changed.
    #[test]
    fn a_poll_after_a_fence_drops_the_previous_owners_rows() {
        use std::sync::atomic::AtomicU64;
        let store = Shared(AtomicU64::new(0));
        offline_video::remember_user("A");
        let shown = polled(Some(&store));
        assert_eq!(shown.owner.as_deref(), Some("A"));
        offline_video::session_replaced(&offline_video::Session::opened(
            (),
            Some("tokB".to_owned()),
        ));
        let now = polled(Some(&store));
        offline_video::forget_user();
        assert_eq!(now.owner, None, "the poll read A's listing after the fence");
        assert!(now.videos.is_empty());
        assert!(refreshed(&shown, now).is_some(), "the poll kept A's rows");
    }

    /// A listing read for A is never shown once the screen is B's: B's
    /// rows are read fresh, so A's rows never sit next to B's Remove. A
    /// listing read for the same user is shown as polled.
    #[test]
    fn a_listing_read_for_another_user_is_not_shown() {
        let a_rows = vec![video("7", KeptState::Ready)];
        let shown = Listing {
            owner: Some("A".into()),
            videos: a_rows.clone(),
            unreadable: 0,
            leftover: 0,
            usage: None,
            cap: 0,
        };
        let rows = rows_for(&shown, Some("B"), || Listing {
            owner: Some("B".into()),
            ..Listing::default()
        });
        assert!(rows.is_empty(), "A's rows were shown to B: {rows:?}");
        // CR review 6018104500 (kept_video_view.rs:382-385): the re-read
        // says whether an owner was set on each side, never which account.
        let lines = platform::take_info_lines();
        assert!(
            lines.iter().any(|l| l.contains("verdict=reread")
                && l.contains("from_set=true")
                && l.contains("to_set=true")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .all(|l| !l.contains("\"A\"") && !l.contains("\"B\"")),
            "an account id was logged: {lines:?}"
        );
        let rows = rows_for(&shown, Some("A"), || panic!("read again for the same user"));
        assert_eq!(rows, a_rows);
        let rows = rows_for(&shown, None, Listing::default);
        assert!(rows.is_empty(), "A's rows were shown to nobody");
    }

    /// The listing carries the store's unreadable count with the videos,
    /// and the count even with nobody signed in.
    #[test]
    fn a_listing_reads_the_unreadable_count_with_the_videos() {
        use super::super::kept_video::{KeepVideoRefusal, KeepVideoRequest};
        struct Two;
        impl VideoStore for Two {
            fn videos(&self, _: &str) -> Vec<KeptVideo> {
                Vec::new()
            }
            fn start(&self, _: KeepVideoRequest) -> Result<(), KeepVideoRefusal> {
                Ok(())
            }
            fn remove(&self, _: &str, _: &str) {}
            fn save_position(&self, _: &str, _: &str, _: u64) {}
            fn cap_bytes(&self) -> u64 {
                0
            }
            fn used_bytes(&self, _: &str) -> u64 {
                0
            }
            fn free_bytes(&self) -> u64 {
                0
            }
            fn unreadable_count(&self) -> usize {
                2
            }
        }
        assert_eq!(listing(Some(&Two), Some("u")).unreadable, 2);
        assert_eq!(listing(Some(&Two), None).unreadable, 2);
        assert_eq!(listing(None, Some("u")), Listing::default());
    }

    /// A discard whose thread the system refuses says so and enables the
    /// button again, so the user can retry; it never stays "removing".
    #[test]
    fn a_discard_whose_thread_is_refused_reports_it_and_can_be_retried() {
        use super::super::kept_video::{KeepVideoRefusal, KeepVideoRequest};
        use std::sync::Arc;
        struct Never;
        impl VideoStore for Never {
            fn videos(&self, _: &str) -> Vec<KeptVideo> {
                Vec::new()
            }
            fn start(&self, _: KeepVideoRequest) -> Result<(), KeepVideoRefusal> {
                Ok(())
            }
            fn remove(&self, _: &str, _: &str) {}
            fn save_position(&self, _: &str, _: &str, _: u64) {}
            fn cap_bytes(&self) -> u64 {
                0
            }
            fn used_bytes(&self, _: &str) -> u64 {
                0
            }
            fn free_bytes(&self) -> u64 {
                0
            }
            fn unreadable_count(&self) -> usize {
                1
            }
        }
        fn root() -> Element {
            use_hook(|| {
                OFF_UI_SPAWN_FAULT.with(|f| f.set(true));
                start_discard(Arc::new(Never));
            });
            let discard = DISCARD.read().clone();
            rsx! {
                VideoList {
                    videos: vec![],
                    used: 0,
                    cap: 0,
                    on_play: |_| {},
                    on_remove: |_| {},
                    problem: discard.problem,
                    unreadable: 1,
                    discarding: discard.running,
                }
            }
        }
        let mut dom = VirtualDom::new(root);
        dom.rebuild_in_place();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let html = rt.block_on(async {
            let start = std::time::Instant::now();
            loop {
                let html = dioxus_ssr::render(&dom);
                if html.contains("could not start")
                    || start.elapsed() > std::time::Duration::from_secs(3)
                {
                    break html;
                }
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    dom.wait_for_work(),
                )
                .await;
                dom.render_immediate_to_vec();
            }
        });
        assert!(
            html.contains("the removal could not start (no thread for the test). Try again."),
            "{html}"
        );
        assert!(
            !html.contains("disabled"),
            "the button stayed disabled: {html}"
        );
    }

    fn video(item: &str, state: KeptState) -> KeptVideo {
        KeptVideo {
            item: item.into(),
            version: String::new(),
            user: "u".into(),
            title: "Invented Film".into(),
            kind: "movie".into(),
            runtime_ms: 5_400_000,
            master_url: "/m.m3u8".into(),
            audio: None,
            subtitles: vec![],
            files: vec![],
            quality_bps: 2_000_000,
            estimated_bytes: 1_572_864,
            state,
            position_ms: 0,
            saved_at: "2026-11-16T09:00:00Z".into(),
            estimate_from_adoption: false,
            stale_on_disk: 0,
            transferred: 0,
            halt: None,
        }
    }

    fn render(root: fn() -> Element) -> String {
        let mut dom = VirtualDom::new(root);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn control(video: Option<KeptVideo>) -> Element {
        rsx! {
            VideoKeepControl {
                video,
                quality: DEFAULT_QUALITY_BPS,
                busy: false,
                message: None,
                on_quality: |_| {},
                on_download: |()| {},
                on_remove: |()| {},
            }
        }
    }

    #[test]
    fn nothing_stored_offers_the_three_qualities_and_download() {
        let html = render(|| control(None));
        assert!(html.contains(QUALITY_SELECT));
        for bps in QUALITY_CHOICES_BPS {
            assert!(html.contains(quality_label(bps)), "{html}");
        }
        assert!(html.contains("Download"));
        assert!(!html.contains("Remove"));
    }

    /// A queued download that failed for good offers Retry (the queue's, in
    /// place of Download); a queued or paused one offers neither.
    #[test]
    fn a_queued_failure_offers_retry_and_a_waiting_entry_neither() {
        fn queued(failed: bool) -> Element {
            rsx! {
                VideoKeepControl {
                    video: None,
                    queued: QueuedKeep { line: "x".into(), failed },
                    quality: DEFAULT_QUALITY_BPS,
                    busy: false,
                    message: None,
                    on_quality: |_| {},
                    on_download: |()| {},
                    on_remove: |()| {},
                }
            }
        }
        let failed = render(|| queued(true));
        assert!(failed.contains("Retry"), "{failed}");
        assert!(!failed.contains(">Download<"), "{failed}");
        let waiting = render(|| queued(false));
        assert!(!waiting.contains("Retry"), "{waiting}");
        assert!(!waiting.contains(">Download<"), "{waiting}");
    }

    #[test]
    fn a_running_download_shows_its_percent_and_remove() {
        let html = render(|| {
            control(Some(video(
                "8",
                KeptState::Downloading {
                    done: 3,
                    total: 10,
                    bytes: 5,
                },
            )))
        });
        assert!(html.contains("Downloading 30%"), "{html}");
        assert!(html.contains("Remove"));
        assert!(!html.contains(DOWNLOAD_BUTTON));
    }

    #[test]
    fn a_ready_download_shows_its_size_and_remove() {
        let html = render(|| control(Some(video("8", KeptState::Ready))));
        assert!(html.contains("On this device \u{b7} 1.5 MiB"), "{html}");
        assert!(html.contains("Remove"));
    }

    #[test]
    fn a_failed_download_names_the_reason_and_retries() {
        let html = render(|| {
            control(Some(video(
                "8",
                KeptState::Failed {
                    reason: "segment 4 returned 502".into(),
                },
            )))
        });
        assert!(html.contains("segment 4 returned 502"), "{html}");
        assert!(html.contains("Retry"));
    }

    fn list() -> Element {
        rsx! {
            VideoList {
                videos: vec![
                    video("8", KeptState::Ready),
                    video("9", KeptState::Downloading { done: 1, total: 4, bytes: 3 }),
                ],
                used: 3_145_728,
                cap: 4 * 1024 * 1024 * 1024,
                on_play: |_| {},
                on_remove: |_| {},
            }
        }
    }

    /// Review 5987743326 (kept_video_view.rs:189): with 1 GiB of music and no
    /// videos, the head does not call the music video. The videos' own
    /// figure is labelled as video, and the shared total says it includes
    /// music.
    #[test]
    fn the_head_does_not_label_music_as_video() {
        fn head() -> Element {
            rsx! {
                VideoList {
                    videos: vec![],
                    used: 1024 * 1024 * 1024,
                    videos_used: Some(0),
                    cap: 4 * 1024 * 1024 * 1024,
                    on_play: |_| {},
                    on_remove: |_| {},
                }
            }
        }
        let mut dom = VirtualDom::new(head);
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(
            !html.contains("1 GiB of 4 GiB used by downloaded videos"),
            "{html}"
        );
        assert!(
            html.contains("0 B used by your downloaded videos"),
            "{html}"
        );
        assert!(
            html.contains("1 GiB of 4 GiB used on this device, with music and other downloads"),
            "{html}"
        );
        // With nothing else in the cap the one line stays as it was.
        let mut dom = VirtualDom::new(list);
        dom.rebuild_in_place();
        assert!(dioxus_ssr::render(&dom).contains("3 MiB of 4 GiB used by downloaded videos"));
    }

    /// The unreadable-downloads row shows only when some exist, in the slim
    /// pill family, and counts them.
    #[test]
    fn the_unreadable_row_shows_only_when_there_are_some() {
        fn with(n: usize) -> Element {
            rsx! {
                VideoList {
                    videos: vec![],
                    used: 0,
                    cap: 1024,
                    on_play: |_| {},
                    on_remove: |_| {},
                    unreadable: n,
                }
            }
        }
        let none = render(|| with(0));
        assert!(!none.contains("be read."), "{none}");
        assert!(!none.contains("Remove them"), "{none}");
        let one = render(|| with(1));
        assert!(one.contains(">1 download couldn"), "{one}");
        let two = render(|| with(2));
        assert!(two.contains(">2 downloads couldn"), "{two}");
        assert!(
            two.contains("pharos-button pharos-button-danger is-small")
                && two.contains("Remove them"),
            "{two}"
        );
    }

    /// A row's Remove acts only while the owner it was drawn for is still
    /// the authorized one when it is clicked.
    #[test]
    fn a_remove_clicked_after_the_owner_changed_removes_nothing() {
        use super::super::kept_video::{KeepVideoRefusal, KeepVideoRequest, VideoStore};
        #[derive(Default)]
        struct Log(std::sync::Mutex<Vec<(String, String)>>);
        impl VideoStore for Log {
            fn videos(&self, _: &str) -> Vec<KeptVideo> {
                Vec::new()
            }
            fn start(&self, _: KeepVideoRequest) -> Result<(), KeepVideoRefusal> {
                Ok(())
            }
            fn remove(&self, user: &str, item: &str) {
                self.0.lock().unwrap().push((user.into(), item.into()));
            }
            fn save_position(&self, _: &str, _: &str, _: u64) {}
            fn cap_bytes(&self) -> u64 {
                0
            }
            fn used_bytes(&self, _: &str) -> u64 {
                0
            }
            fn free_bytes(&self) -> u64 {
                0
            }
        }
        let store = Log::default();
        offline_video::remember_user("alice");
        assert!(remove_if_owner(&store, Some("alice"), "7"));
        offline_video::remember_user("bob");
        assert!(
            !remove_if_owner(&store, Some("alice"), "8"),
            "removed for an old owner"
        );
        offline_video::forget_user();
        assert!(
            !remove_if_owner(&store, Some("alice"), "9"),
            "removed with nobody signed in"
        );
        assert_eq!(
            *store.0.lock().unwrap(),
            vec![("alice".to_owned(), "7".to_owned())]
        );
    }

    /// Folders a removal left behind keep the row, and its Remove them, up
    /// after the unreadable ones are gone; the row goes once cleanup is done.
    #[test]
    fn folders_left_after_a_failed_removal_keep_remove_them() {
        fn with(unreadable: usize, leftover: usize) -> Element {
            rsx! {
                VideoList {
                    videos: vec![],
                    used: 0,
                    cap: 1024,
                    on_play: |_| {},
                    on_remove: |_| {},
                    unreadable,
                    leftover,
                }
            }
        }
        let left = render(|| with(0, 2));
        assert!(
            left.contains(">2 downloads couldn&#39;t be removed."),
            "{left}"
        );
        assert!(left.contains("Remove them"), "{left}");
        assert!(!left.contains("be read."), "{left}");
        let both = render(|| with(1, 1));
        assert!(both.contains(">1 download couldn&#39;t be read."), "{both}");
        assert!(
            both.contains(">1 download couldn&#39;t be removed."),
            "{both}"
        );
        assert_eq!(both.matches("Remove them").count(), 1, "{both}");
        let done = render(|| with(0, 0));
        assert!(!done.contains("Remove them"), "{done}");
    }

    /// Only a ready video has Play; both have Remove; the total heads it.
    #[test]
    fn the_downloads_list_plays_only_what_is_ready() {
        let html = render(list);
        assert_eq!(html.matches("Play Invented Film").count(), 1, "{html}");
        assert_eq!(html.matches(">Remove<").count(), 2, "{html}");
        assert!(html.contains("3 MiB of 4 GiB used"), "{html}");
        assert!(html.contains("Medium (2 Mbps)"));
        assert!(html.contains("Downloading 25%"));
    }

    /// CR 4646280250: with no authorized owner the usage was not read: no
    /// figure is shown, never 0 B. The control is the test above.
    #[test]
    fn an_unread_usage_shows_no_figure() {
        let html = render(|| {
            rsx! { VideoList { videos: vec![], used: 0, cap: 1024, used_known: false, on_play: |_| {}, on_remove: |_| {} } }
        });
        assert!(!html.contains("0 B"), "{html}");
        assert!(
            html.contains("shows once your account is confirmed"),
            "{html}"
        );
    }

    #[test]
    fn an_empty_list_says_so() {
        let html = render(|| {
            rsx! { VideoList { videos: vec![], used: 0, cap: 1024, on_play: |_| {}, on_remove: |_| {} } }
        });
        assert!(html.contains("pharos-video-empty"));
    }

    #[test]
    fn percent_never_exceeds_a_hundred_or_divides_by_zero() {
        assert_eq!(percent(0, 0), 0);
        assert_eq!(percent(5, 4), 100);
        assert_eq!(percent(1, 3), 33);
    }

    fn cap_setting() -> Element {
        rsx! {
            VideoCapSetting { cap: 16 * crate::native::offline_ui::GIB, notice: None, on_pick: |_| {} }
        }
    }

    #[test]
    fn the_cap_picker_offers_every_choice_and_presses_the_current_one() {
        let html = render(cap_setting);
        for (value, _) in cap_options() {
            assert!(
                html.contains(&format!("id=\"video-cap-{value}\"")),
                "{html}"
            );
        }
        let pressed: Vec<&str> = html
            .split("<button")
            .filter(|b| b.contains("aria-pressed=\"true\""))
            .collect();
        assert_eq!(pressed.len(), 1, "{html}");
        assert!(pressed[0].contains("id=\"video-cap-16\""), "{html}");
        assert!(html.contains("Space for downloads"), "{html}");
    }

    #[test]
    fn an_option_that_is_not_offered_never_reaches_the_store() {
        struct Spy(std::sync::Mutex<Vec<u64>>);
        impl VideoStore for Spy {
            fn videos(&self, _: &str) -> Vec<KeptVideo> {
                vec![]
            }
            fn start(
                &self,
                _: super::super::kept_video::KeepVideoRequest,
            ) -> Result<(), super::super::kept_video::KeepVideoRefusal> {
                Ok(())
            }
            fn remove(&self, _: &str, _: &str) {}
            fn save_position(&self, _: &str, _: &str, _: u64) {}
            fn cap_bytes(&self) -> u64 {
                0
            }
            fn set_cap_bytes(&self, bytes: u64) -> Result<(), String> {
                self.0.lock().unwrap().push(bytes);
                Ok(())
            }
            fn used_bytes(&self, _: &str) -> u64 {
                0
            }
            fn free_bytes(&self) -> u64 {
                0
            }
        }
        let spy = Spy(std::sync::Mutex::new(vec![]));
        assert_eq!(
            apply_cap(&spy, "32"),
            Ok(32 * crate::native::offline_ui::GIB)
        );
        assert!(apply_cap(&spy, "3").is_err());
        assert_eq!(
            *spy.0.lock().unwrap(),
            [32 * crate::native::offline_ui::GIB]
        );
    }
}
