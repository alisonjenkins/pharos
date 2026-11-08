//! The Android TV app's page rules (spec 025), kept apart from the I/O in
//! `android_page` so host tests cover them: which server the app talks to,
//! the in-app history Back walks, which page requests go to the server, how
//! a login answer reads as the sign-in form's status, and the messages the
//! page's script sends into Rust.

use serde::Deserialize;

/// The server a build points at when nothing is stored: `PHAROS_TV_SERVER`
/// at build time, else Alison's prototype server (spec 025 slice 0).
pub const DEFAULT_SERVER: &str = match option_env!("PHAROS_TV_SERVER") {
    Some(server) => server,
    None => "https://prototype.aliflix.alison-jenkins.com",
};

/// The user agent the shared UI reads: `device_class` takes "Android TV" for
/// a TV, so the ten-foot layout applies.
pub const USER_AGENT: &str = "Pharos Android TV";

/// The first page the app opens on.
pub const START_PATH: &str = "/ui/";

/// The server address to use: the stored one when it is a usable `https://`
/// address, else the build's default. A trailing slash is dropped.
pub fn server_base(stored: Option<&str>) -> String {
    stored
        .map(str::trim)
        .filter(|s| s.starts_with("https://") && s.len() > "https://".len())
        .unwrap_or(DEFAULT_SERVER)
        .trim_end_matches('/')
        .to_owned()
}

/// The app's own history: the WebView has no address bar, so the pages the
/// app pushed are kept here and the remote's Back walks them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    stack: Vec<String>,
}

impl Default for Paths {
    fn default() -> Self {
        Self::new()
    }
}

impl Paths {
    pub fn new() -> Self {
        Paths {
            stack: vec![START_PATH.to_owned()],
        }
    }

    pub fn current(&self) -> &str {
        self.stack.last().map_or(START_PATH, String::as_str)
    }

    pub fn push(&mut self, path: &str) {
        if self.current() != path {
            self.stack.push(path.to_owned());
        }
    }

    pub fn replace(&mut self, path: &str) {
        match self.stack.last_mut() {
            Some(top) => path.clone_into(top),
            None => self.stack.push(path.to_owned()),
        }
    }

    /// Whether the app pushed a page Back can return to.
    pub fn can_go_back(&self) -> bool {
        self.stack.len() > 1
    }

    /// Step back; the new current page, or `None` at the first page.
    pub fn back(&mut self) -> Option<&str> {
        if !self.can_go_back() {
            return None;
        }
        self.stack.pop();
        Some(self.current())
    }

    /// The page the WebView's own history shows after a `popstate`: walk back
    /// to it when the app holds it, else take it as the new page.
    pub fn popped_to(&mut self, path: &str) {
        if let Some(at) = self.stack.iter().rposition(|p| p == path) {
            self.stack.truncate(at + 1);
        } else {
            self.replace(path);
        }
    }
}

/// The first path segments the page asks the server for (images, streams,
/// subtitles, the UI's fonts and scripts). The app answers them by fetching
/// from the server with its token in a header, so no token is ever in a URL.
const PROXIED: [&str; 9] = [
    "Items",
    "Videos",
    "Audio",
    "Users",
    "UserImage",
    "Persons",
    "Artists",
    "pharos",
    "ui",
];

/// The container list a TV build declares: one no file has, so the server
/// never offers direct play (an empty list would mean "any"). Carried from
/// `feat/tv-android-playback` (#1002) for the native spike.
pub const NO_DIRECT_PLAY: &str = "none";

/// Whether the page's request for `segment` (its path's first segment) is
/// answered from the server.
pub fn proxied(segment: &str) -> bool {
    PROXIED.contains(&segment)
}

/// Every segment the proxy registers a handler for.
pub fn proxied_segments() -> &'static [&'static str] {
    &PROXIED
}

/// `base` + the page's `path_and_query` for the server.
pub fn server_url(base: &str, path_and_query: &str) -> String {
    format!("{base}{path_and_query}")
}

/// A login refusal's `Error.kind` as the HTTP status the sign-in form reads
/// (`machine::sign_in_error`): a wrong password is 401, the throttle 429.
pub fn login_status(kind: &str) -> u16 {
    match kind {
        "notPermitted" | "revoked" | "notFound" | "invalid" => 401,
        "limit" => 429,
        "unavailable" => 503,
        _ => 500,
    }
}

/// A message the page's script sends into Rust.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Bridge {
    /// A key pressed outside a text field, not a held key's repeat.
    Key {
        key: String,
        #[serde(default)]
        ctrl: bool,
        #[serde(default)]
        meta: bool,
        /// Whether the key was pressed in a text field; only the app-wide
        /// key listener hears those.
        #[serde(default)]
        text: bool,
    },
    /// The WebView's history moved (the remote's Back) to `path`.
    Pop { path: String },
    /// The viewport's CSS size.
    Size { w: f64, h: f64 },
}

/// Read one bridge message; `None` for anything unknown.
pub fn parse_bridge(value: &serde_json::Value) -> Option<Bridge> {
    Bridge::deserialize(value).ok()
}

/// Whether the page's script stops the WebView's own handling of `key`: the
/// arrows, which the app's spatial focus moves itself, and Space, the
/// player's play key, which would also click the focused button. Enter is
/// left to the WebView, which clicks the focused control.
pub fn script_takes(key: &str) -> bool {
    matches!(
        key,
        "ArrowUp" | "ArrowDown" | "ArrowLeft" | "ArrowRight" | " "
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_https_server_wins_and_anything_else_falls_back() {
        assert_eq!(
            server_base(Some("https://media.example/ ")),
            "https://media.example"
        );
        assert_eq!(server_base(Some("http://media.example")), DEFAULT_SERVER);
        assert_eq!(server_base(Some("https://")), DEFAULT_SERVER);
        assert_eq!(server_base(None), DEFAULT_SERVER.trim_end_matches('/'));
    }

    #[test]
    fn back_walks_the_pushed_pages_and_stops_at_the_first() {
        let mut paths = Paths::new();
        paths.push("/ui/browse/1");
        paths.push("/ui/title/2");
        paths.push("/ui/title/2");
        assert_eq!(paths.back(), Some("/ui/browse/1"));
        assert_eq!(paths.back(), Some(START_PATH));
        assert_eq!(paths.back(), None);
        assert!(!paths.can_go_back());
    }

    #[test]
    fn a_replaced_page_is_skipped_by_back() {
        let mut paths = Paths::new();
        paths.push("/ui/browse/1");
        paths.replace("/ui/browse/1?page=2");
        assert_eq!(paths.current(), "/ui/browse/1?page=2");
        assert_eq!(paths.back(), Some(START_PATH));
    }

    #[test]
    fn a_popstate_to_a_held_page_truncates_and_to_a_new_one_replaces() {
        let mut paths = Paths::new();
        paths.push("/ui/a");
        paths.push("/ui/b");
        paths.popped_to("/ui/a");
        assert_eq!(paths.current(), "/ui/a");
        assert!(paths.can_go_back());
        paths.popped_to("/ui/elsewhere");
        assert_eq!(paths.current(), "/ui/elsewhere");
        assert_eq!(paths.back(), Some(START_PATH));
    }

    #[test]
    fn server_paths_are_proxied_and_the_apps_own_are_not() {
        for segment in ["Items", "Videos", "Audio", "pharos", "ui"] {
            assert!(proxied(segment), "{segment}");
        }
        for segment in ["", "assets", "__events", "index.html", "items"] {
            assert!(!proxied(segment), "{segment}");
        }
        assert_eq!(
            server_url("https://s", "/Items/1/Images/Primary?tag=a"),
            "https://s/Items/1/Images/Primary?tag=a"
        );
    }

    #[test]
    fn login_refusals_read_as_the_forms_statuses() {
        assert_eq!(login_status("notPermitted"), 401);
        assert_eq!(login_status("limit"), 429);
        assert_eq!(login_status("unavailable"), 503);
        assert_eq!(login_status("other"), 500);
    }

    #[test]
    fn bridge_messages_parse_and_unknown_ones_do_not() {
        let key = serde_json::json!({"t": "key", "key": "ArrowDown"});
        assert_eq!(
            parse_bridge(&key),
            Some(Bridge::Key {
                key: "ArrowDown".into(),
                ctrl: false,
                meta: false,
                text: false
            })
        );
        let pop = serde_json::json!({"t": "pop", "path": "/ui/"});
        assert_eq!(
            parse_bridge(&pop),
            Some(Bridge::Pop {
                path: "/ui/".into()
            })
        );
        assert_eq!(parse_bridge(&serde_json::json!({"t": "nope"})), None);
        assert_eq!(parse_bridge(&serde_json::json!(3)), None);
    }

    #[test]
    fn the_script_takes_the_arrows_and_space() {
        assert!(script_takes("ArrowLeft"));
        assert!(script_takes(" "));
        assert!(!script_takes("Enter"));
        assert!(!script_takes("a"));
    }

    #[test]
    fn the_user_agent_reads_as_a_tv() {
        assert!(USER_AGENT.contains("Android TV"));
    }
}
