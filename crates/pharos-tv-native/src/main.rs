//! Spec 032 slice 0 spike: the shared components drawn by Dioxus Native
//! (Blitz) on Android TV, no WebView. Renders the home rails, a 120-poster
//! grid and logs every key with its timing under the `pharos-bench` tag.
//!
//! Fixture data only; the live server comes with slice 1.

use dioxus::prelude::*;

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
    main();
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

fn root() -> Element {
    rsx! { Spike { start: Screen::Home, data: Data::fixture() } }
}
