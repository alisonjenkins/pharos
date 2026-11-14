//! The Pharos phone app (spec 044). Built with `--features mobile` for
//! Android; `nix run .#android-phone-apk` drives the whole build.
//!
//! Runs the shared `App` in the system WebView, as the TV app does, in a
//! process that also hosts the car's media service (`pharos_android`).

#[cfg(target_os = "android")]
fn main() {
    use dioxus::prelude::*;

    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("pharos-phone"),
    );
    // The default hook's backtrace loops through JNI frames without end on
    // Android (see pharos-ui's tv.rs); log the message and its thread.
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        log::error!(
            "pharos: panic on thread {}: {info}",
            thread.name().unwrap_or("<unnamed>")
        );
    }));
    // The generated classes call into this library by name; without a use
    // from here the linker drops every `Java_` export.
    pharos_android::link();
    log::info!("pharos: start app=phone");
    let config = dioxus::mobile::Config::new()
        .with_background_color((0x0e, 0x11, 0x16, 0xff))
        .with_custom_head(format!(
            "<style>html{{background:#0e1116}}</style><style>{}</style>",
            include_str!("../../pharos-ui/assets/pharos.css")
        ));
    dioxus::LaunchBuilder::new().with_cfg(config).launch(root);

    fn root() -> Element {
        pharos_ui::native::use_server_proxy();
        rsx! {
            pharos_ui::App {}
        }
    }
}

#[cfg(not(target_os = "android"))]
fn main() {
    eprintln!("pharos-phone only runs on Android");
}
