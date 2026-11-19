//! Views for downloaded videos (spec 048): the Download control on a title
//! page, the Downloads page and the offline start's list. The pure
//! components take data in and send events out, so they render under
//! `dioxus-ssr`; the stateful wrappers below them talk to the registered
//! [`VideoStore`].

use std::cell::RefCell;
use std::rc::Rc;

use dioxus::prelude::*;
use pharos_client::api::{Detail, ItemRef, PharosApi, TrackPicks};

use super::app::{call, AccountMe, Ctx};
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
) -> Element {
    let state = video
        .as_ref()
        .map(|v| (state_line(v), matches!(v.state, KeptState::Failed { .. })));
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
    #[props(default)] on_discard: EventHandler<()>,
    /// A discard is running: its button is disabled until it ends.
    #[props(default)]
    discarding: bool,
) -> Element {
    rsx! {
        p { class: "pharos-muted", id: "pharos-video-total",
            "{format_bytes(used)} of {format_bytes(cap)} used by downloaded videos."
        }
        if let Some(text) = problem {
            p { class: "pharos-notice", role: "alert", "{text}" }
        }
        if unreadable > 0 {
            div { class: "pharos-download-head", id: "pharos-video-unreadable",
                span { class: "pharos-muted",
                    if unreadable == 1 {
                        "1 download couldn't be read."
                    } else {
                        "{unreadable} downloads couldn't be read."
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

thread_local! {
    /// The play sessions of started downloads. A session ends when its
    /// grant is dropped, and the stream URL the store fetches carries it, so
    /// the grants stay until the page closes.
    static HELD_GRANTS: RefCell<Vec<pharos_client::api::NativeGrant>> = const { RefCell::new(Vec::new()) };
}

/// The signed-in user: the account when it has loaded, else the one
/// remembered on this device.
fn use_user() -> Option<String> {
    let account = try_use_context::<AccountMe>();
    let me = account
        .and_then(|a| a.0.read().clone().flatten())
        .map(|m| m.id);
    me.or_else(offline_video::last_user)
}

/// What a screen of downloads shows from the store: the user's videos and
/// how many folders could not be read. Polled as one value, so either one
/// changing reaches the screen (a sweep can drop an unreadable folder while
/// the list stays the same).
#[derive(Debug, Clone, Default, PartialEq)]
struct Listing {
    videos: Vec<KeptVideo>,
    unreadable: usize,
}

/// The listing to show for `user` now: nothing without a store or a user,
/// except the unreadable count, which is the store's.
fn listing(store: Option<&dyn VideoStore>, user: Option<&str>) -> Listing {
    let Some(store) = store else {
        return Listing::default();
    };
    Listing {
        videos: user.map(|u| store.videos(u)).unwrap_or_default(),
        unreadable: store.unreadable_count(),
    }
}

/// The listing a poll sets, when it differs from the one shown.
fn refreshed(shown: &Listing, now: Listing) -> Option<Listing> {
    (*shown != now).then_some(now)
}

/// The user's listing, re-read every [`POLL_MS`] while the component lives.
fn use_listing(user: Option<String>) -> Signal<Listing> {
    // Each render hands the poller the user as it is now.
    let current = use_hook(|| Rc::new(RefCell::new(user.clone())));
    *current.borrow_mut() = user;
    let read = {
        let current = Rc::clone(&current);
        move || listing(video_store().as_deref(), current.borrow().as_deref())
    };
    let mut shown = use_signal(read.clone());
    use_future(move || {
        let read = read.clone();
        async move {
            poll_every(
                platform::SLEEP_IS_REAL,
                || platform::sleep_ms(POLL_MS),
                || {
                    let was = shown.peek().unreadable;
                    let next = refreshed(&shown.peek(), read());
                    if let Some(now) = next {
                        if now.unreadable != was {
                            platform::info(&format!(
                                "pharos: video listing unreadable verdict=changed from={was} to={}",
                                now.unreadable
                            ));
                        }
                        shown.set(now);
                    }
                    true
                },
            )
            .await;
        }
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
    let listing = use_listing(user.clone());
    let id = detail.item.id;
    let item = id.to_string();
    let mut quality = use_signal(|| DEFAULT_QUALITY_BPS);
    let mut busy = use_signal(|| false);
    let mut message = use_signal(|| None::<String>);
    let video = listing.read().videos.iter().find(|v| v.item == item).cloned();
    let remove_user = user.clone();
    let remove_item = item.clone();
    let remove_store = store.clone();
    rsx! {
        VideoKeepControl {
            video,
            quality: quality(),
            busy: busy(),
            message: message(),
            on_quality: move |bps| quality.set(bps),
            on_download: move |()| {
                let (detail, version, store) = (detail.clone(), version.clone(), store.clone());
                let bps = *quality.peek();
                busy.set(true);
                message.set(None);
                spawn(async move {
                    match start_download(ctx, id, &detail, &version, bps, &*store).await {
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
                if let Some(user) = &remove_user {
                    platform::info(&format!("pharos: video removed item={remove_item}"));
                    remove_store.remove(user, &remove_item);
                }
                message.set(None);
            },
        }
    }
}

/// Negotiates an HLS stream at `quality_bps`, hands it to the store and
/// keeps the grant alive. The error is the line to show.
async fn start_download(
    ctx: Ctx,
    id: ItemRef,
    detail: &Detail,
    version: &str,
    quality_bps: u64,
    store: &dyn super::kept_video::VideoStore,
) -> Result<(), String> {
    let Some(api) = ctx.api.peek().clone() else {
        return Err("Connect to the server to download this video.".into());
    };
    let me = api
        .me(&call("title", "download"))
        .await
        .map_err(|e| format!("Could not tell who is signed in: {e}"))?;
    // The chosen file's tracks are its own.
    let own;
    let detail = if version.is_empty() {
        detail
    } else {
        own = api
            .detail_version(&call("title", "download"), id, version)
            .await
            .map_err(|e| format!("Could not read this version: {e}"))?;
        &own
    };
    let caps = offline_video::download_caps(platform::capabilities(), quality_bps);
    let (decision, grant) = api
        .play(
            &call("title", "download"),
            id,
            version,
            &caps,
            TrackPicks::default(),
        )
        .await
        .map_err(|e| format!("The server would not start a stream: {e}"))?;
    platform::info(&format!(
        "pharos: video keep negotiated item={id} method={} quality_bps={quality_bps} audio={:?} url={}",
        decision.method, decision.audio, decision.stream_url
    ));
    let request = offline_video::keep_request(detail, version, &me.id, &decision, quality_bps)?;
    HELD_GRANTS.with(|g| g.borrow_mut().push(grant));
    store.start(request).map_err(|e| e.to_string())?;
    platform::info(&format!(
        "pharos: video keep started item={id} quality_bps={quality_bps}"
    ));
    Ok(())
}

/// The Downloads page's videos. Nothing without a registered store.
#[component]
pub fn VideoDownloads(
    /// Show the space allowance picker (the Downloads page does; the
    /// offline start, which only plays, does not).
    #[props(default = true)]
    settings: bool,
) -> Element {
    let Some(store) = shown_video_store() else {
        return rsx! {};
    };
    let user = use_user();
    let listing = use_listing(user.clone());
    let nav = use_context::<Nav>();
    let used = user.as_deref().map_or(0, |u| store.used_bytes(u));
    let remove_user = user.clone();
    let remove_store = store.clone();
    let problem = use_signal(|| None::<String>);
    let discarding = use_signal(|| false);
    // Read at each render. The listing polls it with the videos, so a change
    // a sweep makes renders again even when the list is the same; a discard
    // sets `problem`, which renders again too.
    let unreadable = store.unreadable_count();
    let discard_store = store.clone();
    let mut cap = use_signal(|| store.cap_bytes());
    let mut cap_notice = use_signal(|| None::<String>);
    let cap_store = store.clone();
    rsx! {
        section { class: "pharos-downloads", aria_labelledby: "video-downloads-title",
            h2 { id: "video-downloads-title", "Downloaded videos" }
            if settings {
                VideoCapSetting {
                cap: cap(),
                notice: cap_notice(),
                on_pick: move |option: String| match apply_cap(&*cap_store, &option) {
                    Ok(bytes) => {
                        platform::info(&format!("pharos: video cap set bytes={bytes}"));
                        cap.set(bytes);
                        cap_notice.set(None);
                    }
                    Err(e) => {
                        platform::warn(&format!("pharos: video cap refused option={option:?} error={e}"));
                        cap_notice.set(Some(format!("The space allowance was not changed: {e}")));
                    }
                },
                }
            }
            VideoList {
                videos: listing.read().videos.clone(),
                used,
                cap: store.cap_bytes(),
                problem: problem(),
                unreadable,
                discarding: discarding(),
                on_discard: move |()| start_discard(discard_store.clone(), discarding, problem),
                on_play: move |item: String| match item.parse::<ItemRef>() {
                    Ok(id) => nav.0.call(Route::Watch(id)),
                    Err(e) => platform::warn(&format!("pharos: video play failed item={item} error={e}")),
                },
                on_remove: move |item: String| {
                    if let Some(user) = &remove_user {
                        platform::info(&format!("pharos: video removed item={item}"));
                        remove_store.remove(user, &item);
                    }
                },
            }
        }
    }
}

/// Discards the folders whose record cannot be read (`discard_unreadable`
/// deletes folders, so it can take a while). The deleting runs on its own
/// thread, off the UI executor; `discarding` is set until it ends (the
/// button is disabled meanwhile) and `problem` is set back on the UI.
/// Runs once at a time.
fn start_discard(
    store: std::sync::Arc<dyn super::kept_video::VideoStore>,
    mut discarding: Signal<bool>,
    mut problem: Signal<Option<String>>,
) {
    if *discarding.peek() {
        return;
    }
    discarding.set(true);
    spawn(async move {
        let outcome = off_ui(move || store.discard_unreadable()).await;
        platform::info(&format!(
            "pharos: video discard unreadable result={outcome:?}"
        ));
        problem.set(match outcome {
            Some(Ok(_)) => None,
            Some(Err(e)) => Some(format!("Some downloads could not be removed: {e}")),
            None => Some("Some downloads could not be removed: the removal stopped".to_owned()),
        });
        discarding.set(false);
    });
}

/// Runs `work` on a thread of its own and resolves with its result, so a
/// blocking job (deleting folders) never holds the UI executor. `None` when
/// the thread ended without a result (it panicked).
fn off_ui<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> impl std::future::Future<Output = Option<T>> {
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
    std::thread::spawn(move || {
        let mut deliver = Deliver(sender, None);
        deliver.1 = Some(work());
    });
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

/// The offline start's list of videos: the ready ones, which play with no
/// server. Nothing when there are none.
#[component]
pub fn OfflineVideos() -> Element {
    // Subscribed: a store published later renders the list then.
    let _store = shown_video_store();
    let videos = offline_video::ready_for_last_user();
    if videos.is_empty() {
        return rsx! {};
    }
    rsx! {
        section { class: "pharos-offline", id: "pharos-offline-videos",
            h2 { "Offline: videos on this device" }
            VideoDownloads { settings: false }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

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

    /// Discarding the unreadable folders deletes folders: it runs off the
    /// UI executor, so a render goes on meanwhile with the button disabled,
    /// and the result reaches `problem` once the work ends.
    #[test]
    fn a_discard_runs_off_the_ui_with_its_button_disabled_until_it_ends() {
        use super::super::kept_video::{KeepVideoRefusal, KeepVideoRequest, VideoStore};
        use std::sync::{Arc, Mutex};
        struct Slow(Mutex<std::sync::mpsc::Receiver<()>>);
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
                // Held until the test lets it finish.
                let _ = self.0.lock().unwrap().recv();
                Err("one folder stayed".to_owned())
            }
        }
        thread_local! {
            static STORE: RefCell<Option<Arc<dyn VideoStore>>> = const { RefCell::new(None) };
        }
        fn root() -> Element {
            let discarding = use_signal(|| false);
            let problem = use_signal(|| None::<String>);
            use_hook(|| {
                let store = STORE.with(|s| s.borrow().clone()).unwrap();
                start_discard(store, discarding, problem);
            });
            rsx! {
                VideoList {
                    videos: vec![],
                    used: 0,
                    cap: 0,
                    on_play: |_| {},
                    on_remove: |_| {},
                    problem: problem(),
                    unreadable: 1,
                    discarding: discarding(),
                }
            }
        }
        let (release, held) = std::sync::mpsc::channel();
        STORE.with(|s| *s.borrow_mut() = Some(Arc::new(Slow(Mutex::new(held)))));
        // A watchdog: an inline discard would block the render forever.
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
        release.send(()).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
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

    /// A poll refreshes the screen when only the unreadable count changed:
    /// a sweep can drop an unreadable folder while the list stays empty.
    #[test]
    fn a_poll_refreshes_an_unreadable_count_that_changed_alone() {
        let shown = Listing {
            videos: vec![],
            unreadable: 1,
        };
        let now = Listing {
            videos: vec![],
            unreadable: 0,
        };
        assert_eq!(
            refreshed(&shown, now.clone()),
            Some(now),
            "the count changed and the screen kept the old one"
        );
        assert_eq!(refreshed(&shown, shown.clone()), None);
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
