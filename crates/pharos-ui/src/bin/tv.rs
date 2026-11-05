//! Android TV entrypoint (spec 025). Built with `--features mobile` for an
//! Android target; `nix run .#android-apk` drives the whole build.
//!
//! Slice 1 in progress: the shared `App` does not run here yet (its DOM reads
//! are not ported), so this launches a placeholder screen.

#[cfg(target_os = "android")]
fn main() {
    use dioxus::prelude::*;

    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("pharos-tv"),
    );
    dioxus::launch(|| {
        rsx! {
            main {
                style: "display:flex;align-items:center;justify-content:center;height:100vh;margin:0;background:#101014;color:#eee;font:32px sans-serif;",
                "Pharos for Android TV is in progress."
            }
        }
    });
}

#[cfg(not(target_os = "android"))]
fn main() {
    eprintln!("pharos-tv only runs on Android");
}
