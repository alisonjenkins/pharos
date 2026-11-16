//! Views for downloaded videos (spec 048): the Download control on a title
//! page, the Downloads page and the offline start's list. The pure
//! components take data in and send events out, so they render under
//! `dioxus-ssr`; the stateful wrappers below them talk to the registered
//! [`VideoStore`].

use std::cell::RefCell;

use dioxus::prelude::*;
use pharos_client::api::{Detail, ItemRef, PharosApi, TrackPicks};

use super::app::{call, AccountMe, Ctx};
use super::kept_video::{
    quality_label, KeptState, KeptVideo, QUALITY_CHOICES_BPS, DEFAULT_QUALITY_BPS,
};
use super::offline_ui::format_bytes;
use super::offline_video;
use super::offline_view::poll_every;
use super::platform;
use super::ports::video_store;
use super::route::Route;
use super::views::Nav;

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
        KeptState::Downloading { done, total, .. } => format!("Downloading {}%", percent(*done, *total)),
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
    let state = video.as_ref().map(|v| (state_line(v), matches!(v.state, KeptState::Failed { .. })));
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
) -> Element {
    rsx! {
        p { class: "pharos-muted", id: "pharos-video-total",
            "{format_bytes(used)} of {format_bytes(cap)} used by downloaded videos."
        }
        if let Some(text) = problem {
            p { class: "pharos-notice", role: "alert", "{text}" }
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
    let me = account.and_then(|a| a.0.read().clone().flatten()).map(|m| m.id);
    me.or_else(offline_video::last_user)
}

/// The user's videos, re-read every [`POLL_MS`] while the component lives.
fn use_videos(user: Option<String>) -> Signal<Vec<KeptVideo>> {
    let read = {
        let user = user.clone();
        move || match (video_store(), &user) {
            (Some(store), Some(user)) => store.videos(user),
            _ => Vec::new(),
        }
    };
    let mut videos = use_signal(read.clone());
    use_future(move || {
        let read = read.clone();
        async move {
            poll_every(
                platform::SLEEP_IS_REAL,
                || platform::sleep_ms(POLL_MS),
                || {
                    let now = read();
                    if *videos.peek() != now {
                        videos.set(now);
                    }
                    true
                },
            )
            .await;
        }
    });
    videos
}

/// The Download control of a film's or episode's title page. Nothing
/// without a registered store.
#[component]
pub fn VideoKeep(detail: Detail, version: String) -> Element {
    let Some(store) = video_store() else {
        return rsx! {};
    };
    let ctx = use_context::<Ctx>();
    let user = use_user();
    let videos = use_videos(user.clone());
    let id = detail.item.id;
    let item = id.to_string();
    let mut quality = use_signal(|| DEFAULT_QUALITY_BPS);
    let mut busy = use_signal(|| false);
    let mut message = use_signal(|| None::<String>);
    let video = videos.read().iter().find(|v| v.item == item).cloned();
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
        .play(&call("title", "download"), id, version, &caps, TrackPicks::default())
        .await
        .map_err(|e| format!("The server would not start a stream: {e}"))?;
    platform::info(&format!(
        "pharos: video keep negotiated item={id} method={} quality_bps={quality_bps} audio={:?} url={}",
        decision.method, decision.audio, decision.stream_url
    ));
    let request = offline_video::keep_request(detail, version, &me.id, &decision, quality_bps)?;
    HELD_GRANTS.with(|g| g.borrow_mut().push(grant));
    store.start(request).map_err(|e| e.to_string())?;
    platform::info(&format!("pharos: video keep started item={id} quality_bps={quality_bps}"));
    Ok(())
}

/// The Downloads page's videos. Nothing without a registered store.
#[component]
pub fn VideoDownloads() -> Element {
    let Some(store) = video_store() else {
        return rsx! {};
    };
    let user = use_user();
    let videos = use_videos(user.clone());
    let nav = use_context::<Nav>();
    let used = user.as_deref().map_or(0, |u| store.used_bytes(u));
    let remove_user = user.clone();
    let remove_store = store.clone();
    rsx! {
        section { class: "pharos-downloads", aria_labelledby: "video-downloads-title",
            h2 { id: "video-downloads-title", "Downloaded videos" }
            VideoList {
                videos: videos(),
                used,
                cap: store.cap_bytes(),
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

/// The offline start's list of videos: the ready ones, which play with no
/// server. Nothing when there are none.
#[component]
pub fn OfflineVideos() -> Element {
    let videos = offline_video::ready_for_last_user();
    if videos.is_empty() {
        return rsx! {};
    }
    rsx! {
        section { class: "pharos-offline", id: "pharos-offline-videos",
            h2 { "Offline: videos on this device" }
            VideoDownloads {}
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

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
            control(Some(video("8", KeptState::Downloading { done: 3, total: 10, bytes: 5 })))
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
            control(Some(video("8", KeptState::Failed { reason: "segment 4 returned 502".into() })))
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
}
