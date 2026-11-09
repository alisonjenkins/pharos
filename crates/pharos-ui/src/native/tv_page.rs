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

/// The browse grid whose children are the tiles, as `web` finds them.
const GRID: &str = ".pharos-browse .pharos-grid";

/// Script: scroll the browse tile at `at` to the top of the page and focus
/// its link, so the remote's next press starts from the section jumped to
/// (B1028). Answers whether the tile was in the page.
pub fn scroll_to_tile_js(at: u32) -> String {
    format!(
        r#"
const grid = document.querySelector({grid});
const tile = grid ? grid.children[{at}] : null;
if (!tile) return false;
tile.scrollIntoView(true);
const link = tile.querySelector("a[href], button, [tabindex]") || tile;
if (link.focus) link.focus({{ preventScroll: true }});
return true;
"#,
        grid = serde_json::Value::String(GRID.to_owned()),
    )
}

/// Script: scroll the page back to its top.
pub const SCROLL_TO_TOP_JS: &str = "window.scrollTo(0, 0);";

/// Script: watch the elements with `ids` for coming into view, sending
/// `true` each time one does, until [`unwatch_sentinel_js`] for the same
/// `watch` sends `false` and ends it (B1028). Sends `false` at once when
/// none of them is in the page, or when the watch was dropped before this
/// ran.
pub fn watch_sentinel_js(watch: u64, ids: &[&str]) -> String {
    let ids = serde_json::Value::from(ids.to_vec());
    format!(
        r#"
const all = window.__pharosSentinels || (window.__pharosSentinels = {{}});
if (all[{watch}] === "gone") {{ delete all[{watch}]; dioxus.send(false); return; }}
const els = {ids}.map((id) => document.getElementById(id)).filter(Boolean);
if (!els.length) {{ dioxus.send(false); return; }}
let end;
const done = new Promise((r) => {{ end = r; }});
const seen = new IntersectionObserver((es) => {{
  if (es.some((e) => e.isIntersecting)) dioxus.send(true);
}});
els.forEach((el) => seen.observe(el));
all[{watch}] = () => {{ seen.disconnect(); delete all[{watch}]; dioxus.send(false); end(); }};
await done;
"#
    )
}

/// Script: end the sentinel watch `watch`, or mark it gone when its script
/// has not run yet.
pub fn unwatch_sentinel_js(watch: u64) -> String {
    format!(
        "const all = window.__pharosSentinels || (window.__pharosSentinels = {{}}); \
         const stop = all[{watch}]; \
         if (typeof stop === \"function\") stop(); else all[{watch}] = \"gone\";"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_jump_scrolls_the_grid_tile_and_focuses_it() {
        let js = scroll_to_tile_js(42);
        assert!(
            js.contains(r#"document.querySelector(".pharos-browse .pharos-grid")"#),
            "{js}"
        );
        assert!(js.contains("grid.children[42]"), "{js}");
        assert!(js.contains("scrollIntoView(true)"), "{js}");
        assert!(js.contains("focus({ preventScroll: true })"), "{js}");
        assert!(
            js.contains("return false") && js.contains("return true"),
            "{js}"
        );
    }

    #[test]
    fn a_sentinel_watch_observes_every_id_and_ends_on_unwatch() {
        let js = watch_sentinel_js(7, &["pharos-more-sentinel", "pharos-more-end"]);
        assert!(
            js.contains(r#"["pharos-more-sentinel","pharos-more-end"].map"#),
            "{js}"
        );
        assert!(js.contains("new IntersectionObserver"), "{js}");
        assert!(js.contains("dioxus.send(true)"), "{js}");
        assert!(js.contains("all[7] = () =>"), "{js}");
        assert!(js.contains(r#"all[7] === "gone""#), "{js}");
        let stop = unwatch_sentinel_js(7);
        assert!(stop.contains("const stop = all[7]"), "{stop}");
        assert!(stop.contains(r#"all[7] = "gone""#), "{stop}");
    }

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
