//! Spec 032 slice 0 spike: the shared components drawn by Dioxus Native
//! (Blitz) on Android TV, no WebView. Renders the home rails, a 120-poster
//! grid and logs every key with its timing under the `pharos-bench` tag.
//!
//! Fixture data only; the live server comes with slice 1.

use dioxus::prelude::*;

mod screens;
use screens::{Screen, Spike};

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: android_activity::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("pharos-bench"),
    );
    blitz_shell::set_android_app(app);
    log::info!("pharos-tv-native start");
    main();
}

fn main() {
    dioxus::launch(root);
}

fn root() -> Element {
    rsx! { Spike { start: Screen::Home } }
}

