//! Android TV entrypoint (spec 025). Built with `--features mobile` for an
//! Android target; `nix run .#android-apk` drives the whole build.
//!
//! Runs the shared `App` in the system WebView. Its DOM reads go through
//! `document::eval` (`native/android_page.rs`), and the page's requests to
//! the server are answered by the app with the device token in a header.

#[cfg(target_os = "android")]
fn main() {
    use dioxus::prelude::*;

    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("pharos-tv"),
    );
    // The default hook prints a backtrace, and on Android the unwinder loops
    // through JNI frames (`art_quick_generic_jni_trampoline`) without end:
    // the panicking thread printed tens of millions of frames, never got to
    // unwind, and the panic message scrolled out of logcat. Log the message
    // and where it came from instead.
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        log::error!(
            "pharos: panic on thread {}: {info}",
            thread.name().unwrap_or("<unnamed>")
        );
    }));
    log::info!(
        "pharos: start tokio={}",
        tokio::runtime::Handle::try_current().is_ok()
    );
    // Styled from the first frame: Pharos Night behind the WebView, which is
    // white until the page paints, and the stylesheet this build's markup
    // was written against, in the head. Fetched from the server it arrived
    // after the first paint (unstyled content), and could be a newer
    // server's.
    let config = dioxus::mobile::Config::new()
        .with_background_color((0x0e, 0x11, 0x16, 0xff))
        .with_custom_head(format!(
            "<style>html{{background:#0e1116}}</style><style>{}</style>",
            include_str!("../../assets/pharos.css")
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
    eprintln!("pharos-tv only runs on Android");
}
