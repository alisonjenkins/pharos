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
const PROXIED: [&str; 11] = [
    "Items",
    "Videos",
    "videos",
    "Audio",
    "audio",
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

/// The container list a TV build declares: one no file has, so the server
/// never offers direct play (an empty list would mean "any"). A file played
/// straight from the server cannot seek here: the Android WebView applies a
/// request's `Range` to the proxy's answer itself, skipping bytes the proxy
/// never had. HLS segments are fetched whole, so they play.
pub const NO_DIRECT_PLAY: &str = "none";

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
        /// Where the focus was as the key went down, for the calls a key
        /// listener makes synchronously on the web.
        #[serde(default, flatten)]
        at: KeyContext,
    },
    /// The WebView's history moved (the remote's Back) to `path`.
    Pop { path: String },
    /// The viewport's CSS size.
    Size { w: f64, h: f64 },
    /// A media element's state, after any of its events.
    Media(Media),
    /// An hls.js event on the player for `id`.
    Hls {
        id: String,
        #[serde(flatten)]
        event: HlsEvent,
    },
}

/// The page around a key press, read by the page's script as it happened.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct KeyContext {
    /// A handler nearer the focus already took the key.
    pub prevented: bool,
    /// The page has a player (`#pharos-player, .pharos-listen`).
    pub player: bool,
    /// The focus is inside it.
    pub in_player: bool,
    /// The focus is on a button or link.
    pub on_button: bool,
}

/// Whether a transport key goes on to the player (B618): there is one, the
/// focus is outside it, and it is not Space on a focused button or link,
/// which Space presses as everywhere.
pub fn forwards_to_player(key: &str, at: KeyContext) -> bool {
    at.player && !at.in_player && !(key == " " && at.on_button)
}

/// A media element as its last event left it: what the player's controls
/// read many times a second, so it is pushed, not asked for.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Media {
    pub id: String,
    /// `currentTime`, s.
    pub pos: f64,
    /// `duration`, s; `NaN` and infinity arrive as `null`.
    #[serde(default)]
    pub dur: Option<f64>,
    pub paused: bool,
    pub ended: bool,
    pub seeking: bool,
    /// `readyState`.
    pub ready: u16,
    #[serde(default = "one")]
    pub rate: f64,
    pub vol: f64,
    pub muted: bool,
    #[serde(default)]
    pub w: u32,
    #[serde(default)]
    pub h: u32,
    /// Whether it has a source.
    #[serde(default)]
    pub src: bool,
    /// `getVideoPlaybackQuality`'s dropped and total frames.
    #[serde(default)]
    pub dropped: u64,
    #[serde(default)]
    pub frames: u64,
}

fn one() -> f64 {
    1.0
}

/// `HAVE_FUTURE_DATA`: enough to play at the position now.
const HAVE_FUTURE_DATA: u16 = 3;

impl Media {
    /// The position `elapsed_ms` after this state arrived: a playing element
    /// moves on between its `timeupdate`s (about 4 a second), so the
    /// progress bar does not step. Never past the duration.
    pub fn position_ms(&self, elapsed_ms: f64) -> u64 {
        let moving = !self.paused && !self.ended && !self.seeking && self.ready >= HAVE_FUTURE_DATA;
        let mut s = self.pos;
        if moving {
            s += elapsed_ms.max(0.0) / 1000.0 * self.rate;
        }
        if let Some(d) = self.dur.filter(|d| d.is_finite() && *d > 0.0) {
            s = s.min(d);
        }
        secs_to_ms(s)
    }

    pub fn duration_ms(&self) -> u64 {
        self.dur.filter(|d| d.is_finite()).map_or(0, secs_to_ms)
    }

    /// Whether it can play at its position now, not mid-seek.
    pub fn has_data(&self) -> bool {
        !self.seeking && self.ready >= HAVE_FUTURE_DATA
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn secs_to_ms(s: f64) -> u64 {
    if s.is_finite() && s > 0.0 {
        (s * 1000.0).round() as u64
    } else {
        0
    }
}

/// What hls.js reported.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "ev", rename_all = "lowercase")]
pub enum HlsEvent {
    /// `Hls.Events.ERROR`: its `type` and `details`.
    Error {
        kind: String,
        details: String,
        fatal: bool,
    },
    /// `Hls.Events.FRAG_LOADED` for a main fragment.
    Frag {
        bytes: u64,
        load_ms: u64,
        media_ms: u64,
    },
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
        for segment in ["Items", "Videos", "videos", "Audio", "pharos", "ui"] {
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
                text: false,
                at: KeyContext::default(),
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
    fn media_states_parse_and_a_playing_one_moves_on() {
        let v = serde_json::json!({"t": "media", "id": "v", "pos": 10.0, "dur": 20.0,
            "paused": false, "ended": false, "seeking": false, "ready": 4, "vol": 1.0,
            "muted": false, "src": true});
        let Some(Bridge::Media(m)) = parse_bridge(&v) else {
            panic!("not media");
        };
        assert_eq!(m.position_ms(500.0), 10_500);
        assert_eq!(m.position_ms(60_000.0), 20_000, "never past the end");
        assert_eq!(m.duration_ms(), 20_000);
        assert!(m.has_data());
        let paused = Media {
            paused: true,
            ..m.clone()
        };
        assert_eq!(paused.position_ms(500.0), 10_000);
        let waiting = Media {
            ready: 2,
            ..m.clone()
        };
        assert_eq!(waiting.position_ms(500.0), 10_000);
        assert!(!waiting.has_data());
        let live = Media { dur: None, ..m };
        assert_eq!(live.duration_ms(), 0);
    }

    #[test]
    fn transport_keys_go_to_a_player_the_focus_is_outside() {
        let key = serde_json::json!({"t": "key", "key": "ArrowRight", "player": true});
        let Some(Bridge::Key { at, .. }) = parse_bridge(&key) else {
            panic!("not a key");
        };
        assert!(forwards_to_player("ArrowRight", at));
        let inside = KeyContext {
            in_player: true,
            ..at
        };
        assert!(!forwards_to_player("ArrowRight", inside));
        let none = KeyContext::default();
        assert!(!forwards_to_player("ArrowRight", none));
        let on_button = KeyContext {
            on_button: true,
            ..at
        };
        assert!(!forwards_to_player(" ", on_button));
        assert!(forwards_to_player("ArrowRight", on_button));
    }

    #[test]
    fn hls_events_parse() {
        let e = serde_json::json!({"t": "hls", "id": "v", "ev": "error",
            "kind": "networkError", "details": "fragLoadError", "fatal": true});
        assert_eq!(
            parse_bridge(&e),
            Some(Bridge::Hls {
                id: "v".into(),
                event: HlsEvent::Error {
                    kind: "networkError".into(),
                    details: "fragLoadError".into(),
                    fatal: true
                }
            })
        );
        let f = serde_json::json!({"t": "hls", "id": "v", "ev": "frag",
            "bytes": 10, "load_ms": 2, "media_ms": 4000});
        assert!(matches!(
            parse_bridge(&f),
            Some(Bridge::Hls {
                event: HlsEvent::Frag { bytes: 10, .. },
                ..
            })
        ));
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
