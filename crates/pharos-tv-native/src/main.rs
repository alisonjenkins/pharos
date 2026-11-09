//! Spec 032 slice 0 spike: the shared components drawn by Dioxus Native
//! (Blitz) on Android TV, no WebView. Renders the home rails, a 120-poster
//! grid and logs every key with its timing under the `pharos-bench` tag.
//!
//! On the TV it loads home and a library from the live server (signing in
//! by QuickConnect the first time); on the host it shows fixture data.

use dioxus::prelude::*;

#[cfg(target_os = "android")]
mod audio;
#[cfg(target_os = "android")]
mod codecs;
#[cfg(target_os = "android")]
mod hls_net;
#[cfg(target_os = "android")]
mod hole;
#[cfg(target_os = "android")]
mod launch;
#[cfg(target_os = "android")]
mod live;
#[cfg(target_os = "android")]
mod net;
// Pure, so it builds and is tested on the host too.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
mod feeder;
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
mod ts;
#[cfg(target_os = "android")]
mod video;
mod screens;
use screens::{Data, Screen, Spike};

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: android_activity::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("pharos-bench"),
    );
    stdout_to_log();
    if hole::enabled() {
        hole::start(&app);
    }
    // Decoder probe: logs the device's decoders and the capabilities they
    // give, off the UI thread.
    {
        let app = app.clone();
        std::thread::spawn(move || {
            codecs::probe(&app);
        });
    }
    blitz_shell::set_android_app(app);
    // Render-scale probe: `setprop debug.pharos.render_scale 0.667` on the TV.
    if let Some(scale) = android_properties::getprop("debug.pharos.render_scale")
        .value()
        .and_then(|v| v.parse::<f32>().ok())
    {
        log::info!("render scale {scale}");
        blitz_shell::set_render_scale(scale);
    }
    // GPU-wait probe: `setprop debug.pharos.gpu_wait 1` restores the upstream
    // wait after each frame, so one APK compares both.
    #[cfg(feature = "vello-hybrid")]
    {
        let wait = android_properties::getprop("debug.pharos.gpu_wait")
            .value()
            .as_deref()
            == Some("1");
        log::info!("gpu wait {wait}");
        anyrender_vello_hybrid::set_wait_for_gpu(wait);
    }
    log::info!("pharos-tv-native start");
    live::start();
    match url::Url::parse(&live::server()) {
        Ok(server) => launch::launch(root, server),
        Err(e) => log::error!("server address: {e}"),
    }
}

/// Blitz's `log-times` feature writes frame and phase times to stdout, which
/// Android discards. Pipe stdout into the log under the same tag.
#[cfg(target_os = "android")]
fn stdout_to_log() {
    use std::io::BufRead;
    use std::os::fd::FromRawFd;
    let mut fds = [0; 2];
    // SAFETY: `pipe` fills two new descriptors; `dup2` points stdout at the
    // write end, and the read end is owned by the File below.
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 || libc::dup2(fds[1], 1) < 0 {
            return;
        }
        let read = std::fs::File::from_raw_fd(fds[0]);
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(read).lines().map_while(Result::ok) {
                log::info!("{line}");
            }
        });
    }
}

fn main() {
    dioxus_native::launch(root);
}

#[cfg(not(target_os = "android"))]
fn root() -> Element {
    rsx! { Spike { start: Screen::Home, data: Data::fixture() } }
}

/// The hole probe's page: nothing over the video rect, a translucent bar and
/// an opaque label where a player's controls would go.
#[cfg(target_os = "android")]
#[component]
fn HoleProbe() -> Element {
    // CSS px: the TV's density 320 gives a 960x540 viewport on 1920x1080.
    rsx! {
        div { style: "width: 960px; height: 540px;",
            div { style: "height: 360px;" }
            div { style: "margin-left: 160px; width: 620px; height: 20px; padding: 10px; background-color: rgba(0, 0, 0, 0.5); color: white; font-size: 16px;",
                "Controls drawn by Blitz over the video layer"
            }
            div { style: "height: 30px;" }
            div { style: "margin-left: 160px; width: 300px; height: 40px; padding: 10px; background-color: #1e6fd9; color: white; font-size: 16px;",
                "Opaque UI outside the video rect"
            }
        }
    }
}

/// Fixture data until the live server answers; then its home and library.
#[cfg(target_os = "android")]
fn root() -> Element {
    // Constant for the process, so the hooks below run in the same order.
    if hole::probe_page() {
        return rsx! { HoleProbe {} };
    }
    let mut data = use_signal(Data::fixture);
    let mut notice = use_signal(|| Some("Connecting…".to_owned()));
    let mut detail = use_signal(|| None::<pharos_client::api::Detail>);
    use_hook(|| {
        if let Some(mut events) = live::take_events() {
            spawn(async move {
                use futures_util::StreamExt;
                while let Some(event) = events.next().await {
                    match event {
                        live::Event::Code(code) => {
                            notice.set(Some(format!("Approve this TV with code {code}")));
                        }
                        live::Event::Ready { home, grid } => {
                            data.set(Data::new(&home, grid.items));
                            notice.set(None);
                        }
                        live::Event::Detail(d) => detail.set(Some(*d)),
                        live::Event::Failed(e) => notice.set(Some(format!("Offline: {e}"))),
                    }
                }
            });
        }
    });
    rsx! {
        Spike {
            start: Screen::Home,
            data: data(),
            notice: notice(),
            detail: detail(),
            on_open: move |id| live::send(live::Command::Detail(id)),
            on_back: move |()| detail.set(None),
        }
    }
}
