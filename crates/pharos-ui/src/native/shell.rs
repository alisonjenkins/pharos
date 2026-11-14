//! Which shell runs the page: a browser, the Android TV app or the Android
//! phone app. Both apps run this crate in a WebView on Android, so the build
//! alone cannot tell a remote-driven TV from a touch phone; the phone app
//! says so before launch, and an unregistered Android build is the TV.

use std::sync::OnceLock;

/// The shell the page runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Browser,
    AndroidTv,
    AndroidPhone,
}

impl Shell {
    pub fn label(self) -> &'static str {
        match self {
            Shell::Browser => "browser",
            Shell::AndroidTv => "android_tv",
            Shell::AndroidPhone => "android_phone",
        }
    }

    /// Whether a D-pad remote is the only input: only the TV app.
    pub fn remote_driven(self) -> bool {
        self == Shell::AndroidTv
    }
}

static PHONE: OnceLock<()> = OnceLock::new();

/// Called by the phone app before launch. Returns false if already called.
pub fn register_phone() -> bool {
    PHONE.set(()).is_ok()
}

/// The shell for an Android app (`android_app`) or a browser, given whether
/// the phone app registered.
pub fn resolve(android_app: bool, phone_registered: bool) -> Shell {
    match (android_app, phone_registered) {
        (false, _) => Shell::Browser,
        (true, false) => Shell::AndroidTv,
        (true, true) => Shell::AndroidPhone,
    }
}

/// The shell this process runs in.
pub fn current() -> Shell {
    resolve(cfg!(target_os = "android"), PHONE.get().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_is_the_tv_until_the_phone_app_registers() {
        assert_eq!(resolve(true, false), Shell::AndroidTv);
        assert_eq!(resolve(true, true), Shell::AndroidPhone);
    }

    #[test]
    fn a_browser_stays_a_browser_even_if_a_phone_registered() {
        assert_eq!(resolve(false, false), Shell::Browser);
        assert_eq!(resolve(false, true), Shell::Browser);
    }

    #[test]
    fn only_the_tv_app_is_remote_driven() {
        assert!(Shell::AndroidTv.remote_driven());
        assert!(!Shell::AndroidPhone.remote_driven());
        assert!(!Shell::Browser.remote_driven());
    }

    #[test]
    fn labels_are_distinct() {
        let l = [Shell::Browser, Shell::AndroidTv, Shell::AndroidPhone].map(Shell::label);
        assert_eq!(l, ["browser", "android_tv", "android_phone"]);
    }

    #[test]
    fn registering_twice_is_refused() {
        register_phone();
        assert!(!register_phone());
    }
}
