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
    log::info!(
        "pharos: start tokio={}",
        tokio::runtime::Handle::try_current().is_ok()
    );
    dioxus::launch(root);

    fn root() -> Element {
        pharos_ui::native::use_server_proxy();
        // The shared stylesheet is the server's, through the proxy, so it
        // matches the server's UI. Linked here, after the proxy's handlers
        // are in: a link in the page's head loads before them and comes
        // back empty.
        rsx! {
            document::Stylesheet { href: "/ui/assets/pharos.css" }
            pharos_ui::App {}
        }
    }
}

#[cfg(not(target_os = "android"))]
fn main() {
    eprintln!("pharos-tv only runs on Android");
}
