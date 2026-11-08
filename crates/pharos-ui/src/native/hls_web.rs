//! Media Source Extensions adapter for [`pharos_hls`] (spec 031 slice 2c, ADR-0071).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::{Rc, Weak};

use gloo_net::http::Request;
use pharos_client::api::{ClientEvent, ClientEventKind, ClientLevel};
use pharos_hls::planner::{Action, Buffered, Config, Planner, Request as SegRequest, Track};
use pharos_hls::playlist::{parse_master, parse_media, MediaPlaylist, Segment};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast as _, JsValue};

use super::platform::{
    current_path, get_text, info, js_message, load_script, now_ms, report_event, seconds_to_ms,
    sleep_ms, video_element, warn, HlsJs, HLSJS_SRC,
};
use super::playback::{
    check_master, pick_audio_uri, plan_hls, ranges_to_ms, FragmentLoad, HlsFallbackCause, OnFatal,
    OnFragment, PlaybackEngine,
};

const MAX_CONSECUTIVE_FAILURES: u32 = 3;
/// A failed segment is retried after this times its attempt count, so a server
/// briefly out of encoders is not three failures in a few milliseconds.
const RETRY_BASE_MS: u32 = 1_000;

type EventListener = Closure<dyn FnMut(web_sys::Event)>;

enum Op {
    Append {
        bytes: Vec<u8>,
        request: SegRequest,
        load_ms: u64,
        media_ms: u64,
    },
    Remove {
        start_s: f64,
        end_s: f64,
    },
}

#[derive(Clone, Copy)]
enum RunningOp {
    Append {
        request: SegRequest,
        bytes: u64,
        load_ms: u64,
        media_ms: u64,
    },
    Remove,
}

struct TrackRunner {
    init_uri: String,
    segments: Vec<Segment>,
    source_buffer: web_sys::SourceBuffer,
    controller: Option<web_sys::AbortController>,
    generation: u64,
    queue: VecDeque<Op>,
    running: Option<RunningOp>,
    failures: Option<(SegRequest, u32)>,
    /// `updateend`s still to come from `SourceBuffer.abort()`, which belong to no queued op.
    aborted_updateends: u32,
    listeners: Vec<(&'static str, EventListener)>,
}

impl TrackRunner {
    fn new(init_uri: String, segments: Vec<Segment>, source_buffer: web_sys::SourceBuffer) -> Self {
        Self {
            init_uri,
            segments,
            source_buffer,
            controller: None,
            generation: 0,
            queue: VecDeque::new(),
            running: None,
            failures: None,
            aborted_updateends: 0,
            listeners: Vec::new(),
        }
    }
}

struct Session {
    video_id: String,
    url: String,
    start_ms: u64,
    video: web_sys::HtmlMediaElement,
    media_source: web_sys::MediaSource,
    object_url: Option<String>,
    on_source_open: Option<EventListener>,
    video_listeners: Vec<(&'static str, EventListener)>,
    planner: Option<Planner>,
    video_track: Option<TrackRunner>,
    audio_track: Option<TrackRunner>,
    eos_pending: bool,
    dead: bool,
    fallback: Option<HlsJs>,
    on_fatal: OnFatal,
    on_fragment: OnFragment,
}

impl Session {
    fn track_mut(&mut self, track: Track) -> Option<&mut TrackRunner> {
        match track {
            Track::Video => self.video_track.as_mut(),
            Track::Audio => self.audio_track.as_mut(),
        }
    }
}

#[derive(Default)]
pub(super) struct RustHls {
    session: Option<Rc<RefCell<Session>>>,
}

impl PlaybackEngine for RustHls {
    fn attach(
        &mut self,
        video_id: &str,
        url: &str,
        start_ms: u64,
        on_fatal: OnFatal,
        on_fragment: OnFragment,
    ) -> Result<(), String> {
        let video = video_element(video_id).ok_or_else(|| format!("no <video id={video_id}>"))?;
        let media_source =
            web_sys::MediaSource::new().map_err(|e| format!("MediaSource::new: {e:?}"))?;
        let object_url = web_sys::Url::create_object_url_with_source(&media_source)
            .map_err(|e| format!("createObjectURL: {e:?}"))?;

        let session = Rc::new(RefCell::new(Session {
            video_id: video_id.to_owned(),
            url: url.to_owned(),
            start_ms,
            video: video.clone(),
            media_source: media_source.clone(),
            object_url: Some(object_url.clone()),
            on_source_open: None,
            video_listeners: Vec::new(),
            planner: None,
            video_track: None,
            audio_track: None,
            eos_pending: false,
            dead: false,
            fallback: None,
            on_fatal,
            on_fragment,
        }));

        let weak = Rc::downgrade(&session);
        let on_open = EventListener::new(move |_| {
            let next = Weak::clone(&weak);
            wasm_bindgen_futures::spawn_local(async move {
                load(next).await;
            });
        });
        let once = web_sys::AddEventListenerOptions::new();
        once.set_once(true);
        media_source
            .add_event_listener_with_callback_and_add_event_listener_options(
                "sourceopen",
                on_open.as_ref().unchecked_ref(),
                &once,
            )
            .map_err(|e| {
                let _ = web_sys::Url::revoke_object_url(&object_url);
                format!("sourceopen listener: {e:?}")
            })?;
        session.borrow_mut().on_source_open = Some(on_open);

        video.set_src(&object_url);
        self.session = Some(session);
        Ok(())
    }
}

impl Drop for RustHls {
    fn drop(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        let (
            video,
            media_source,
            on_source_open,
            video_listeners,
            video_cleanup,
            audio_cleanup,
            fallback,
            object_url,
        ) = {
            let mut s = session.borrow_mut();
            s.dead = true;
            s.eos_pending = false;
            let video_cleanup = s.video_track.take().map(take_track_cleanup);
            let audio_cleanup = s.audio_track.take().map(take_track_cleanup);
            (
                s.video.clone(),
                s.media_source.clone(),
                s.on_source_open.take(),
                std::mem::take(&mut s.video_listeners),
                video_cleanup,
                audio_cleanup,
                s.fallback.take(),
                s.object_url.take(),
            )
        };

        if let Some(cleanup) = video_cleanup {
            finish_track_cleanup(cleanup);
        }
        if let Some(cleanup) = audio_cleanup {
            finish_track_cleanup(cleanup);
        }
        if let Some(on_open) = on_source_open {
            let _ = media_source.remove_event_listener_with_callback(
                "sourceopen",
                on_open.as_ref().unchecked_ref(),
            );
        }
        for (name, listener) in video_listeners {
            let _ =
                video.remove_event_listener_with_callback(name, listener.as_ref().unchecked_ref());
        }
        drop(fallback);
        let _ = video.pause();
        let _ = video.remove_attribute("src");
        video.load();
        if let Some(url) = object_url {
            let _ = web_sys::Url::revoke_object_url(&url);
        }
    }
}

type TrackCleanup = (
    web_sys::SourceBuffer,
    Option<web_sys::AbortController>,
    Vec<(&'static str, EventListener)>,
);

fn take_track_cleanup(mut track: TrackRunner) -> TrackCleanup {
    track.generation = track.generation.saturating_add(1);
    track.queue.clear();
    track.running = None;
    (
        track.source_buffer,
        track.controller.take(),
        std::mem::take(&mut track.listeners),
    )
}

fn finish_track_cleanup((sb, controller, listeners): TrackCleanup) {
    if let Some(ctrl) = controller {
        ctrl.abort();
    }
    for (name, listener) in listeners {
        let _ = sb.remove_event_listener_with_callback(name, listener.as_ref().unchecked_ref());
    }
}

async fn load(weak: Weak<RefCell<Session>>) {
    let Some((url, start_ms)) = weak.upgrade().and_then(|s| {
        let mut s = s.borrow_mut();
        s.on_source_open = None;
        (!s.dead).then(|| (s.url.clone(), s.start_ms))
    }) else {
        return;
    };

    let master_text = match get_text(&url).await {
        Ok(text) => text,
        Err((status, reason)) => {
            fail_manifest(&weak, "manifestLoadError", status, &reason, &url);
            return;
        }
    };
    let master = match parse_master(&master_text, &url) {
        Ok(m) => m,
        Err(e) => {
            fail_manifest(&weak, "manifestParsingError", None, &e.to_string(), &url);
            return;
        }
    };

    let (video_pl, audio_pl) = if let Some(variant) = master.variants.first() {
        if let Err((cause, value)) = check_master(&master) {
            fallback(&weak, cause, &value).await;
            return;
        }
        let video_text = match get_text(&variant.uri).await {
            Ok(t) => t,
            Err((status, reason)) => {
                fail_manifest(&weak, "levelLoadError", status, &reason, &variant.uri);
                return;
            }
        };
        let video_pl = match parse_media(&video_text, &variant.uri) {
            Ok(p) => p,
            Err(e) => {
                fail_manifest(
                    &weak,
                    "levelParsingError",
                    None,
                    &e.to_string(),
                    &variant.uri,
                );
                return;
            }
        };
        let audio_pl = if let Some(audio_uri) = pick_audio_uri(&master, variant) {
            let audio_text = match get_text(&audio_uri).await {
                Ok(t) => t,
                Err((status, reason)) => {
                    fail_manifest(&weak, "audioTrackLoadError", status, &reason, &audio_uri);
                    return;
                }
            };
            match parse_media(&audio_text, &audio_uri) {
                Ok(p) => Some(p),
                Err(e) => {
                    fail_manifest(
                        &weak,
                        "audioTrackParsingError",
                        None,
                        &e.to_string(),
                        &audio_uri,
                    );
                    return;
                }
            }
        } else {
            None
        };
        (video_pl, audio_pl)
    } else {
        (empty_media_playlist(), None)
    };

    let plan = match plan_hls(
        &master,
        &video_pl,
        audio_pl.as_ref(),
        web_sys::MediaSource::is_type_supported,
    ) {
        Ok(p) => p,
        Err((cause, value)) => {
            fallback(&weak, cause, &value).await;
            return;
        }
    };

    let Some(session) = weak.upgrade() else {
        return;
    };
    let Some((video, media_source)) = ({
        let s = session.borrow();
        (!s.dead).then(|| (s.video.clone(), s.media_source.clone()))
    }) else {
        return;
    };

    // `plan_hls` already refused a playlist without EXT-X-MAP; this keeps the type honest.
    let audio_init = audio_pl.as_ref().map(|a| a.init_uri.clone());
    let (Some(video_init_uri), None | Some(Some(_))) = (video_pl.init_uri.clone(), &audio_init)
    else {
        fallback(&weak, HlsFallbackCause::NotFmp4, "none").await;
        return;
    };

    let duration_ms = pharos_hls::timeline::duration_ms(&video_pl);
    if duration_ms > 0 && media_source.ready_state() == web_sys::MediaSourceReadyState::Open {
        #[allow(clippy::cast_precision_loss)]
        media_source.set_duration(duration_ms as f64 / 1000.0);
    }

    let video_sb = match media_source.add_source_buffer(&plan.video_mime) {
        Ok(sb) => sb,
        Err(e) => {
            fail_media(&weak, "bufferAddCodecError", &js_message(&e));
            return;
        }
    };
    let audio_sb = match plan.audio_mime.as_deref() {
        Some(mime) => match media_source.add_source_buffer(mime) {
            Ok(sb) => Some(sb),
            Err(e) => {
                fail_media(&weak, "bufferAddCodecError", &js_message(&e));
                return;
            }
        },
        None => None,
    };

    let audio_runner = match (audio_sb, audio_pl.as_ref(), audio_init.flatten()) {
        (Some(sb), Some(apl), Some(init_uri)) => {
            let mut runner = TrackRunner::new(init_uri, apl.segments.clone(), sb);
            wire_source_buffer(&weak, Track::Audio, &mut runner);
            Some(runner)
        }
        _ => None,
    };

    let mut video_runner = TrackRunner::new(video_init_uri, video_pl.segments.clone(), video_sb);
    wire_source_buffer(&weak, Track::Video, &mut video_runner);

    let seg_count = video_pl.segments.len();
    let planner = Planner::new(
        video_pl.segments,
        true,
        audio_pl.map(|a| (a.segments, true)),
        Config::default(),
        start_ms,
    );

    if start_ms > 0 {
        #[allow(clippy::cast_precision_loss)]
        video.set_current_time(start_ms as f64 / 1000.0);
    }

    let video_listeners = wire_video(&weak, &video);
    {
        let mut s = session.borrow_mut();
        if s.dead {
            drop(s);
            for (name, listener) in video_listeners {
                let _ = video
                    .remove_event_listener_with_callback(name, listener.as_ref().unchecked_ref());
            }
            finish_track_cleanup(take_track_cleanup(video_runner));
            if let Some(runner) = audio_runner {
                finish_track_cleanup(take_track_cleanup(runner));
            }
            return;
        }
        s.planner = Some(planner);
        s.video_track = Some(video_runner);
        s.audio_track = audio_runner;
        s.video_listeners = video_listeners;
    }

    let audio_label = plan.audio_mime.as_deref().unwrap_or("none");
    info(&format!(
        "pharos: playback engine=rust start video={} audio={audio_label} segments={seg_count} url={url}",
        plan.video_mime,
    ));

    pump(&session);
}

fn empty_media_playlist() -> MediaPlaylist {
    MediaPlaylist {
        version: None,
        target_duration_s: 0,
        media_sequence: 0,
        vod: true,
        ended: true,
        init_uri: None,
        start_offset_ms: None,
        segments: Vec::new(),
        unsupported: Vec::new(),
    }
}

async fn fallback(weak: &Weak<RefCell<Session>>, cause: HlsFallbackCause, value: &str) {
    let Some((video, object_url, video_id, url, start_ms, on_fatal, on_fragment)) =
        weak.upgrade().and_then(|s| {
            let mut s = s.borrow_mut();
            (!s.dead).then(|| {
                (
                    s.video.clone(),
                    s.object_url.take(),
                    s.video_id.clone(),
                    s.url.clone(),
                    s.start_ms,
                    Rc::clone(&s.on_fatal),
                    Rc::clone(&s.on_fragment),
                )
            })
        })
    else {
        return;
    };

    let _ = video.remove_attribute("src");
    video.load();
    if let Some(obj_url) = object_url {
        let _ = web_sys::Url::revoke_object_url(&obj_url);
    }

    warn(&format!(
        "pharos: playback engine=rust fallback=hlsjs cause={} value={value} url={url}",
        cause.label()
    ));
    report_event(ClientEvent {
        kind: ClientEventKind::PlaybackError,
        level: ClientLevel::Warn,
        route: current_path(),
        fields: vec![
            ("engine", "rust".to_owned()),
            ("action", "fallback".to_owned()),
            ("cause", cause.label().to_owned()),
            ("value", value.to_owned()),
        ],
    });

    if let Err(e) = load_script(HLSJS_SRC, "Hls").await {
        on_fatal(e);
        return;
    }

    let Some(session) = weak.upgrade() else {
        return;
    };
    if session.borrow().dead {
        return;
    }

    let mut hls = HlsJs::default();
    match hls.attach(&video_id, &url, start_ms, Rc::clone(&on_fatal), on_fragment) {
        Ok(()) => {
            session.borrow_mut().fallback = Some(hls);
        }
        Err(e) => on_fatal(e),
    }
}

fn wire_video(
    weak: &Weak<RefCell<Session>>,
    video: &web_sys::HtmlMediaElement,
) -> Vec<(&'static str, EventListener)> {
    let mut out = Vec::with_capacity(4);
    for name in ["timeupdate", "progress", "waiting"] {
        let w = Weak::clone(weak);
        let l = EventListener::new(move |_| {
            if let Some(s) = w.upgrade() {
                pump(&s);
            }
        });
        let _ = video.add_event_listener_with_callback(name, l.as_ref().unchecked_ref());
        out.push((name, l));
    }

    let w = Weak::clone(weak);
    let on_seeking = EventListener::new(move |_| {
        if let Some(s) = w.upgrade() {
            on_seek(&s);
        }
    });
    let _ = video.add_event_listener_with_callback("seeking", on_seeking.as_ref().unchecked_ref());
    out.push(("seeking", on_seeking));
    out
}

fn wire_source_buffer(weak: &Weak<RefCell<Session>>, track: Track, runner: &mut TrackRunner) {
    let w_end = Weak::clone(weak);
    let on_end = EventListener::new(move |_| {
        if let Some(s) = w_end.upgrade() {
            on_updateend(&s, track);
        }
    });
    let _ = runner
        .source_buffer
        .add_event_listener_with_callback("updateend", on_end.as_ref().unchecked_ref());
    runner.listeners.push(("updateend", on_end));

    let w_err = Weak::clone(weak);
    let on_err = EventListener::new(move |_| {
        fail_media(
            &w_err,
            "bufferAppendError",
            &format!("{} SourceBuffer error", track.label()),
        );
    });
    let _ = runner
        .source_buffer
        .add_event_listener_with_callback("error", on_err.as_ref().unchecked_ref());
    runner.listeners.push(("error", on_err));
}

fn read_time_ranges(sb: &web_sys::SourceBuffer) -> Vec<(u64, u64)> {
    let Ok(ranges) = sb.buffered() else {
        return Vec::new();
    };
    let len = ranges.length();
    let mut pairs = Vec::with_capacity(len as usize);
    for i in 0..len {
        if let (Ok(start), Ok(end)) = (ranges.start(i), ranges.end(i)) {
            pairs.push((start, end));
        }
    }
    ranges_to_ms(&pairs)
}

fn snapshot_state(session: &Rc<RefCell<Session>>) -> Option<(u64, Buffered)> {
    let (video, v_sb, a_sb) = {
        let s = session.borrow();
        if s.dead {
            return None;
        }
        (
            s.video.clone(),
            s.video_track.as_ref().map(|t| t.source_buffer.clone()),
            s.audio_track.as_ref().map(|t| t.source_buffer.clone()),
        )
    };
    let play_ms = seconds_to_ms(video.current_time());
    let buffered = Buffered {
        video: v_sb.as_ref().map_or_else(Vec::new, read_time_ranges),
        audio: a_sb.as_ref().map_or_else(Vec::new, read_time_ranges),
    };
    Some((play_ms, buffered))
}

fn on_seek(session: &Rc<RefCell<Session>>) {
    let Some((current_ms, buffered)) = snapshot_state(session) else {
        return;
    };
    let actions = {
        let mut s = session.borrow_mut();
        if s.dead {
            return;
        }
        s.eos_pending = false;
        s.planner
            .as_mut()
            .map_or_else(Vec::new, |p| p.seek(current_ms, &buffered))
    };
    execute_actions(session, actions);
    pump(session);
}

fn pump(session: &Rc<RefCell<Session>>) {
    let Some((play_ms, buffered)) = snapshot_state(session) else {
        return;
    };
    let actions = {
        let mut s = session.borrow_mut();
        if s.dead {
            return;
        }
        s.planner
            .as_mut()
            .map_or_else(Vec::new, |p| p.tick(play_ms, &buffered))
    };
    execute_actions(session, actions);
}

fn execute_actions(session: &Rc<RefCell<Session>>, actions: Vec<Action>) {
    for action in actions {
        match action {
            Action::FetchInit { track } => start_fetch(session, track, SegRequest::Init),
            Action::Fetch { track, index } => start_fetch(session, track, SegRequest::Media(index)),
            Action::Abort { track } => abort_track(session, track),
            Action::Evict {
                track,
                start_ms,
                end_ms,
            } => {
                #[allow(clippy::cast_precision_loss)]
                let op = Op::Remove {
                    start_s: start_ms as f64 / 1000.0,
                    end_s: end_ms as f64 / 1000.0,
                };
                {
                    let mut s = session.borrow_mut();
                    if let Some(t) = s.track_mut(track) {
                        t.queue.push_back(op);
                    }
                }
                run_queue(session, track);
            }
            Action::EndOfStream => {
                session.borrow_mut().eos_pending = true;
                maybe_end_of_stream(session);
            }
        }
    }
}

fn abort_track(session: &Rc<RefCell<Session>>, track: Track) {
    let (controller, sb, aborting_append) = {
        let mut s = session.borrow_mut();
        if s.dead {
            return;
        }
        let open = s.media_source.ready_state() == web_sys::MediaSourceReadyState::Open;
        let Some(t) = s.track_mut(track) else {
            return;
        };
        t.generation = t.generation.saturating_add(1);
        t.queue.retain(|op| !matches!(op, Op::Append { .. }));
        // SourceBuffer.abort() throws while a remove is running, so only an append is cut short.
        let aborting_append = open
            && matches!(t.running, Some(RunningOp::Append { .. }))
            && t.source_buffer.updating();
        if aborting_append {
            t.running = None;
            t.aborted_updateends = t.aborted_updateends.saturating_add(1);
        }
        (
            t.controller.take(),
            t.source_buffer.clone(),
            aborting_append,
        )
    };
    if let Some(ctrl) = controller {
        ctrl.abort();
    }
    if aborting_append {
        let _ = sb.abort();
    }
}

fn start_fetch(session: &Rc<RefCell<Session>>, track: Track, request: SegRequest) {
    let controller = match web_sys::AbortController::new() {
        Ok(c) => c,
        Err(e) => {
            fail_media(
                &Rc::downgrade(session),
                "internalException",
                &js_message(&e),
            );
            return;
        }
    };
    let signal = controller.signal();

    let (uri, media_ms, generation, old_controller) = {
        let mut s = session.borrow_mut();
        if s.dead {
            return;
        }
        let Some(t) = s.track_mut(track) else {
            return;
        };
        let (uri, media_ms) = match request {
            SegRequest::Init => (t.init_uri.clone(), 0),
            SegRequest::Media(idx) => {
                let Some(seg) = t.segments.get(idx) else {
                    return;
                };
                (seg.uri.clone(), seg.duration_ms)
            }
        };
        t.generation = t.generation.saturating_add(1);
        let gen = t.generation;
        let old = t.controller.replace(controller);
        (uri, media_ms, gen, old)
    };

    if let Some(old) = old_controller {
        old.abort();
    }

    let weak = Rc::downgrade(session);
    wasm_bindgen_futures::spawn_local(async move {
        let sent_at = now_ms();
        let outcome = fetch_segment(&uri, &signal, sent_at).await;
        if signal.aborted() {
            return;
        }
        let Some(session) = weak.upgrade() else {
            return;
        };
        match outcome {
            Ok((bytes, load_ms)) => {
                let queued = {
                    let mut s = session.borrow_mut();
                    if s.dead {
                        return;
                    }
                    let Some(t) = s.track_mut(track) else {
                        return;
                    };
                    if t.generation != generation {
                        return;
                    }
                    t.controller = None;
                    t.failures = None;
                    t.queue.push_back(Op::Append {
                        bytes,
                        request,
                        load_ms,
                        media_ms,
                    });
                    true
                };
                if queued {
                    run_queue(&session, track);
                }
            }
            Err((status, reason)) => {
                on_fetch_failed(&session, track, request, generation, &uri, status, &reason);
            }
        }
    });
}

async fn fetch_segment(
    uri: &str,
    signal: &web_sys::AbortSignal,
    sent_at: f64,
) -> Result<(Vec<u8>, u64), (Option<u16>, String)> {
    let resp = Request::get(uri)
        .abort_signal(Some(signal))
        .send()
        .await
        .map_err(|e| (None, e.to_string()))?;
    let headers_at = now_ms();
    if !resp.ok() {
        return Err((Some(resp.status()), resp.status_text()));
    }
    let bytes = resp.binary().await.map_err(|e| (None, e.to_string()))?;
    let done_at = now_ms();
    let start = if headers_at > 0.0 {
        headers_at
    } else {
        sent_at
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let load_ms = (done_at - start).max(0.0).round() as u64;
    Ok((bytes, load_ms))
}

fn on_fetch_failed(
    session: &Rc<RefCell<Session>>,
    track: Track,
    request: SegRequest,
    generation: u64,
    uri: &str,
    status: Option<u16>,
    reason: &str,
) {
    let idx_label = match request {
        SegRequest::Init => "init".to_owned(),
        SegRequest::Media(i) => i.to_string(),
    };
    let status_label = status.map_or_else(|| "none".to_owned(), |s| s.to_string());

    let (count, on_fatal) = {
        let mut s = session.borrow_mut();
        if s.dead {
            return;
        }
        let on_fatal = Rc::clone(&s.on_fatal);
        let Some(t) = s.track_mut(track) else {
            return;
        };
        if t.generation != generation {
            return;
        }
        t.controller = None;
        let next_count = match t.failures {
            Some((prev, n)) if prev == request => n.saturating_add(1),
            _ => 1,
        };
        t.failures = Some((request, next_count));
        if next_count >= MAX_CONSECUTIVE_FAILURES {
            s.dead = true;
        }
        (next_count, on_fatal)
    };

    warn(&format!(
        "pharos: playback error engine=rust type=network track={} index={idx_label} status={status_label} reason={reason} url={uri}",
        track.label(),
    ));
    let fatal = count >= MAX_CONSECUTIVE_FAILURES;
    report_event(ClientEvent {
        kind: ClientEventKind::PlaybackError,
        level: if fatal {
            ClientLevel::Error
        } else {
            ClientLevel::Warn
        },
        route: current_path(),
        fields: vec![
            ("engine", "rust".to_owned()),
            ("type", "network".to_owned()),
            ("track", track.label().to_owned()),
            ("index", idx_label.clone()),
            ("status", status_label),
            ("reason", reason.to_owned()),
            ("action", if fatal { "fail" } else { "retry" }.to_owned()),
            ("attempt", count.to_string()),
        ],
    });

    if fatal {
        on_fatal(format!(
            "the stream failed ({} segment {idx_label}: {reason})",
            track.label()
        ));
    } else {
        let weak = Rc::downgrade(session);
        wasm_bindgen_futures::spawn_local(async move {
            sleep_ms(RETRY_BASE_MS.saturating_mul(count)).await;
            let Some(session) = weak.upgrade() else {
                return;
            };
            // The planner keeps the request in flight through the backoff, so ticks meanwhile do not refetch it.
            {
                let mut s = session.borrow_mut();
                if s.dead
                    || s.track_mut(track)
                        .map_or(true, |t| t.generation != generation)
                {
                    return;
                }
                if let Some(planner) = s.planner.as_mut() {
                    planner.failed(track);
                }
            }
            pump(&session);
        });
    }
}

fn run_queue(session: &Rc<RefCell<Session>>, track: Track) {
    let next = {
        let mut s = session.borrow_mut();
        // An append or remove on an ended MediaSource reopens it, so a seek back after endOfStream still plays.
        if s.dead || s.media_source.ready_state() == web_sys::MediaSourceReadyState::Closed {
            return;
        }
        let Some(t) = s.track_mut(track) else {
            return;
        };
        if t.running.is_some() || t.source_buffer.updating() {
            return;
        }
        t.queue.pop_front().map(|op| {
            let running = match &op {
                Op::Append {
                    bytes,
                    request,
                    load_ms,
                    media_ms,
                } => RunningOp::Append {
                    request: *request,
                    bytes: bytes.len() as u64,
                    load_ms: *load_ms,
                    media_ms: *media_ms,
                },
                Op::Remove { .. } => RunningOp::Remove,
            };
            t.running = Some(running);
            (t.source_buffer.clone(), op)
        })
    };

    let Some((sb, op)) = next else {
        maybe_end_of_stream(session);
        return;
    };

    match op {
        Op::Append { mut bytes, .. } => {
            if let Err(err) = sb.append_buffer_with_u8_array(&mut bytes) {
                on_append_error(session, track, &err);
            }
        }
        Op::Remove { start_s, end_s } => {
            if let Err(err) = sb.remove(start_s, end_s) {
                if let Some(t) = session.borrow_mut().track_mut(track) {
                    t.running = None;
                }
                fail_media(
                    &Rc::downgrade(session),
                    "bufferRemoveError",
                    &js_message(&err),
                );
            }
        }
    }
}

fn on_append_error(session: &Rc<RefCell<Session>>, track: Track, err: &JsValue) {
    let is_quota = js_sys::Reflect::get(err, &JsValue::from_str("name"))
        .ok()
        .and_then(|v| v.as_string())
        .as_deref()
        == Some("QuotaExceededError");

    if is_quota {
        let new_target_ms = {
            let mut s = session.borrow_mut();
            if let Some(t) = s.track_mut(track) {
                t.running = None;
            }
            s.planner.as_mut().map_or(0, |p| p.quota_exceeded(track))
        };
        warn(&format!(
            "pharos: playback engine=rust quota_exceeded track={} forward_target_ms={new_target_ms}",
            track.label()
        ));
        run_queue(session, track);
        pump(session);
        return;
    }

    if let Some(t) = session.borrow_mut().track_mut(track) {
        t.running = None;
    }
    fail_media(
        &Rc::downgrade(session),
        "bufferAppendError",
        &js_message(err),
    );
}

fn on_updateend(session: &Rc<RefCell<Session>>, track: Track) {
    let (finished, on_fragment) = {
        let mut s = session.borrow_mut();
        if s.dead {
            return;
        }
        let on_fragment = Rc::clone(&s.on_fragment);
        let finished = s.track_mut(track).and_then(|t| {
            if t.aborted_updateends > 0 {
                t.aborted_updateends -= 1;
                None
            } else {
                t.running.take()
            }
        });
        if matches!(finished, Some(RunningOp::Append { .. })) {
            if let Some(planner) = s.planner.as_mut() {
                planner.appended(track);
            }
        }
        (finished, on_fragment)
    };

    if let Some(RunningOp::Append {
        request: SegRequest::Media(_),
        bytes,
        load_ms,
        media_ms,
    }) = finished
    {
        if track == Track::Video && bytes > 0 && media_ms > 0 {
            on_fragment(FragmentLoad {
                bytes,
                load_ms,
                media_ms,
            });
        }
    }

    run_queue(session, track);
    pump(session);
    maybe_end_of_stream(session);
}

fn maybe_end_of_stream(session: &Rc<RefCell<Session>>) {
    let media_source = {
        let mut s = session.borrow_mut();
        if s.dead
            || !s.eos_pending
            || s.media_source.ready_state() != web_sys::MediaSourceReadyState::Open
        {
            return;
        }
        let track_idle = |t: Option<&TrackRunner>| {
            t.map_or(true, |r| {
                r.running.is_none() && r.queue.is_empty() && !r.source_buffer.updating()
            })
        };
        if !track_idle(s.video_track.as_ref()) || !track_idle(s.audio_track.as_ref()) {
            return;
        }
        s.eos_pending = false;
        s.media_source.clone()
    };
    let _ = media_source.end_of_stream();
}

fn fail_manifest(
    weak: &Weak<RefCell<Session>>,
    details: &str,
    status: Option<u16>,
    reason: &str,
    uri: &str,
) {
    let Some(on_fatal) = weak.upgrade().and_then(|s| {
        let mut s = s.borrow_mut();
        if s.dead {
            return None;
        }
        s.dead = true;
        Some(Rc::clone(&s.on_fatal))
    }) else {
        return;
    };
    let status_label = status.map_or_else(|| "none".to_owned(), |s| s.to_string());
    warn(&format!(
        "pharos: playback error engine=rust type=networkError details={details} status={status_label} reason={reason} fatal=true action=fail attempt=0 url={uri}"
    ));
    report_event(ClientEvent {
        kind: ClientEventKind::PlaybackError,
        level: ClientLevel::Error,
        route: current_path(),
        fields: vec![
            ("engine", "rust".to_owned()),
            ("type", "networkError".to_owned()),
            ("details", details.to_owned()),
            ("status", status_label),
            ("reason", reason.to_owned()),
            ("action", "fail".to_owned()),
            ("attempt", "0".to_owned()),
        ],
    });
    on_fatal(format!("the stream failed (networkError: {reason})"));
}

fn fail_media(weak: &Weak<RefCell<Session>>, details: &str, message: &str) {
    let Some((url, on_fatal)) = weak.upgrade().and_then(|s| {
        let mut s = s.borrow_mut();
        if s.dead {
            return None;
        }
        s.dead = true;
        Some((s.url.clone(), Rc::clone(&s.on_fatal)))
    }) else {
        return;
    };
    warn(&format!(
        "pharos: playback error engine=rust type=mediaError details={details} message={message} fatal=true action=fail attempt=0 url={url}"
    ));
    report_event(ClientEvent {
        kind: ClientEventKind::PlaybackError,
        level: ClientLevel::Error,
        route: current_path(),
        fields: vec![
            ("engine", "rust".to_owned()),
            ("type", "mediaError".to_owned()),
            ("details", details.to_owned()),
            ("message", message.to_owned()),
            ("action", "fail".to_owned()),
            ("attempt", "0".to_owned()),
        ],
    });
    on_fatal(format!("the stream failed (mediaError: {message})"));
}
