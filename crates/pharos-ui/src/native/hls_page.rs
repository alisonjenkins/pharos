//! The Rust HLS engine as the Android app's page runs it (spec 031).
//!
//! The app's Rust runs natively, outside the WebView, where there is no
//! `MediaSource`. This crate's engine ([`hls_web`](super::hls_web)), built to
//! wasm as the `pharos-tv-hls` module, runs in the page instead and tells the
//! app what happens as [`EngineEvent`]s: the app keeps the log, the telemetry
//! and the player's state.

use std::rc::Rc;

use super::hls_web::RustHls;
use super::platform::{set_page_sink, PageOutput};
use super::playback::{DeviceClass, Fatal, FragmentEvent, PlaybackEngine};
use super::tv_page::EngineEvent;

/// A play the page's engine runs; dropping it ends the play.
pub struct PagePlay {
    _engine: RustHls,
}

/// One play to start, as the app's start script gives it
/// (`tv_page::engine_start_js`).
pub struct Start<'a> {
    pub video_id: &'a str,
    pub url: &'a str,
    pub start_ms: u64,
    /// A [`DeviceClass`] label.
    pub device: &'a str,
    pub token: Option<String>,
}

/// Start `start.url` on its `<video>`. Every event, and from now on every log
/// line and client event of this page, goes to `send` as an [`EngineEvent`]'s
/// JSON.
pub fn start(start: Start<'_>, send: impl Fn(String) + 'static) -> Result<PagePlay, String> {
    let device = DeviceClass::from_label(start.device)
        .ok_or_else(|| format!("no device class named {:?}", start.device))?;
    let send: Rc<dyn Fn(String)> = Rc::new(send);
    let output = Rc::clone(&send);
    set_page_sink(Box::new(move |out| {
        let event = match out {
            PageOutput::Line { warn, text } => EngineEvent::Line {
                warn,
                text: text.to_owned(),
            },
            PageOutput::Event(sent) => EngineEvent::of_client_event(sent),
        };
        output(event.to_json());
    }));

    let (fatal, fragment) = (Rc::clone(&send), send);
    let mut engine = RustHls::for_device(device).with_token(start.token);
    engine.attach(
        start.video_id,
        start.url,
        start.start_ms,
        Rc::new(move |failure: Fatal| {
            fatal(
                EngineEvent::Fatal {
                    kind: failure.kind.label().to_owned(),
                    reason: failure.reason,
                }
                .to_json(),
            );
        }),
        Rc::new(move |seen: FragmentEvent| {
            let event = match seen {
                FragmentEvent::Loaded(load) => EngineEvent::Frag {
                    bytes: load.bytes,
                    load_ms: load.load_ms,
                    media_ms: load.media_ms,
                },
                FragmentEvent::Failed => EngineEvent::FragFailed { timed_out: false },
                FragmentEvent::TimedOut => EngineEvent::FragFailed { timed_out: true },
            };
            fragment(event.to_json());
        }),
    )?;
    Ok(PagePlay { _engine: engine })
}
