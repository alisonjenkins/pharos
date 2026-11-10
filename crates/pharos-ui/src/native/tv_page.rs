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

/// The field the TV adds to each client event it submits, so the server's
/// log tells its events from a browser's (FR-608).
pub const EVENT_CLIENT: (&str, &str) = ("client", "android-tv");

/// `event` as the TV submits it: tagged with [`EVENT_CLIENT`], once.
pub fn tagged(mut event: pharos_client::api::ClientEvent) -> pharos_client::api::ClientEvent {
    if !event.fields.iter().any(|(k, _)| *k == EVENT_CLIENT.0) {
        event
            .fields
            .push((EVENT_CLIENT.0, EVENT_CLIENT.1.to_owned()));
    }
    event
}

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

/// An artwork `src` (`/Items/{id}/Images/…`) as the server's own URL, which
/// the WebView loads and caches itself; any other `src` as it is.
pub fn art_src(base: &str, src: &str) -> String {
    if src.starts_with('/') && src.contains("/Images/") {
        server_url(base, src)
    } else {
        src.to_owned()
    }
}

/// `base` + the page's `path_and_query` for the server.
pub fn server_url(base: &str, path_and_query: &str) -> String {
    format!("{base}{path_and_query}")
}

/// A direct stream's `url` as the server's own address, carrying the
/// Media-scoped `token` (ADR-0058) as `api_key`: an `<audio>` element cannot
/// send a header, and loaded straight from the server (not through the page
/// proxy) the server answers its `Range` requests itself, so it seeks.
pub fn direct_media_url(base: &str, url: &str, token: &str) -> String {
    let url = if url.starts_with('/') {
        server_url(base, url)
    } else {
        url.to_owned()
    };
    let join = if url.contains('?') { '&' } else { '?' };
    let mut encoded = String::with_capacity(token.len());
    for b in token.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(b));
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    format!("{url}{join}api_key={encoded}")
}

/// The script that starts page visualiser run `id` drawing `style`, once
/// the wasm module (`window.__pharosVis`) is loaded. A run its owner already
/// stopped (`"gone"`) does not start. The page reports every outcome itself
/// as a `visualiser` bridge message: `started`, or the `render::Fallback`
/// label with the error. The app runs it without waiting on its result,
/// which the WebView can drop after the script ran (B1036, V996).
pub fn visualiser_start_js(id: u64, style: &str) -> String {
    let style = serde_json::Value::String(style.to_owned());
    format!(
        r#"const all = window.__pharosVisRuns = window.__pharosVisRuns || {{}};
const send = (reason, error) => window.__pharosSend &&
  window.__pharosSend({{ t: "visualiser", id: {id}, reason: String(reason), error: String(error || "") }});
(async () => {{
  try {{
    if (!window.__pharosVis && window.__pharosVisLoad) await window.__pharosVisLoad;
    if (!window.__pharosVis) throw new Error("the visualiser module is not loaded");
    if (all[{id}] === "gone") {{ delete all[{id}]; send("stopped_first"); return; }}
    all[{id}] = window.__pharosVis.start({style}, (reason) => send(reason));
    send("started");
  }} catch (e) {{
    send("no_webgl2", e && e.message ? e.message : e);
  }}
}})();"#
    )
}

/// The script that loads the visualiser module into the page once, from its
/// wasm-bindgen `glue` (`--target no-modules`) and its wasm as base64;
/// concurrent callers share the one load, and a failed load can be retried.
pub fn visualiser_load_js(glue: &str, wasm_b64: &str) -> String {
    let glue = serde_json::Value::String(glue.to_owned());
    let wasm = serde_json::Value::String(wasm_b64.to_owned());
    // The glue's top-level `let wasm_bindgen` is only reachable by name from
    // a classic script, so the script itself hands it to `window`. The line
    // break is JavaScript's `\n` escape: a raw one inside the string literal
    // is a syntax error that failed the whole load on the TV (B1032).
    format!(
        r#"if (!window.__pharosVisLoad) {{
  window.__pharosVisLoad = (async () => {{
    const s = document.createElement("script");
    s.textContent = {glue} + "\nwindow.__pharosVisInit = wasm_bindgen;";
    document.head.append(s);
    if (!window.__pharosVisInit) throw new Error("the visualiser glue defined no wasm_bindgen");
    const b = atob({wasm});
    const bytes = new Uint8Array(b.length);
    for (let i = 0; i < b.length; i++) bytes[i] = b.charCodeAt(i);
    await window.__pharosVisInit({{ module_or_path: bytes }});
    window.__pharosVis = window.__pharosVisInit;
    return true;
  }})();
  window.__pharosVisLoad.catch(() => {{ window.__pharosVisLoad = null; }});
}}
return await window.__pharosVisLoad;"#
    )
}

/// The script that scrolls the panel `id` so its child `at` is centred:
/// the web build's `center_child`, run in the TV app's page (B1033).
pub fn center_child_js(id: &str, at: u32) -> String {
    let id = serde_json::Value::String(id.to_owned());
    format!(
        r#"const panel = document.getElementById({id});
const child = panel && panel.children[{at}];
if (child) panel.scrollTop = Math.max(0, child.offsetTop + child.offsetHeight / 2 - panel.clientHeight / 2);"#
    )
}

/// The script that stops page visualiser run `id`, or marks it `"gone"` so
/// a start still on its way does nothing.
pub fn visualiser_stop_js(id: u64) -> String {
    format!(
        r#"const all = window.__pharosVisRuns = window.__pharosVisRuns || {{}};
const run = all[{id}];
if (run && run.stop) {{ run.stop(); run.free(); delete all[{id}]; }} else {{ all[{id}] = "gone"; }}"#
    )
}

/// How long before a media token expires the app asks for the next one, so
/// a song started just before expiry still loads.
pub const MEDIA_TOKEN_MARGIN_MS: u64 = 15 * 60 * 1000;

/// The shortest wait between media-token requests, so a server answering
/// with an already-expired token is not asked in a tight loop.
pub const MEDIA_TOKEN_MIN_WAIT_MS: u64 = 30 * 1000;

/// How long to wait before asking for the next media token, for one that
/// expires at `expires_at_ms`, at `now_ms`.
pub fn media_token_wait_ms(expires_at_ms: u64, now_ms: u64) -> u64 {
    expires_at_ms
        .saturating_sub(now_ms)
        .saturating_sub(MEDIA_TOKEN_MARGIN_MS)
        .max(MEDIA_TOKEN_MIN_WAIT_MS)
}

/// How often the page's bridge passes a video's `timeupdate` on to Rust:
/// each one re-renders the player, which costs the TV's CPU while it
/// decodes video.
pub const VIDEO_TICK_MS: u32 = 1000;

/// How often it passes a song's on (B1030, V989): the karaoke wipe moves and
/// the sung line changes on each, so once a second left a line up to a
/// second late and the wipe stepping. The web's `timeupdate` comes about as
/// often.
pub const AUDIO_TICK_MS: u32 = 250;

/// The players whose arrows the bridge turns into moves between their
/// controls: the video player, and the music screen while it fills the
/// screen (B1034, V994). Not the music screen in the mini bar or floating
/// window, whose controls are the stage bar's.
pub const REMOTE_PLAYERS: &str = "#pharos-player, .pharos-stage.is-full .pharos-listen";

/// Where Up or Down from the player itself lands: the video's control row,
/// or the music screen's play button.
pub const PLAYER_HOME: &str = ".pharos-control-row button, .pharos-listen-play";

/// Controls in a remote player that the arrows pass over: the lyrics
/// panel's lines, which scroll with the song under a remote walking down
/// them, so Down never reached the transport (B1034).
pub const REMOTE_SKIPS: &str = ".pharos-lyrics-line";

/// The remote's Info key: from the page, to the docked player bar's play
/// button (else its first control); from the bar, back to the control it
/// came from. A long page is then no walk to the bar (B1222, V1182).
pub const BAR_KEY_JS: &str = r#"
addEventListener("keydown", (e) => {
  if (!e.isTrusted || e.repeat || e.key !== "Info") return;
  const bar = document.querySelector(".pharos-stage.is-mini");
  if (!bar) return;
  e.preventDefault();
  e.stopImmediatePropagation();
  const at = document.activeElement;
  if (at && bar.contains(at)) {
    const back = window.__pharosBeforeBar;
    window.__pharosBeforeBar = null;
    const ok = !!back && back.isConnected;
    if (ok) {
      back.focus({ preventScroll: true });
      back.scrollIntoView({ block: "nearest" });
    }
    console.info("pharos: keys: mini bar left via=info back=" + ok);
    return;
  }
  const shown = (el) => { const r = el.getBoundingClientRect(); return r.width > 0 && r.height > 0; };
  const to = [...bar.querySelectorAll(".pharos-stage-play")].find(shown) ||
    [...bar.querySelectorAll('__PLAYER_HOME__')].find(shown) ||
    [...bar.querySelectorAll("button:not([disabled])")].find(shown);
  if (!to) return;
  window.__pharosBeforeBar = at && at !== document.body ? at : null;
  to.focus({ preventScroll: true });
  console.info("pharos: keys: mini bar reached via=info");
}, true);
"#;

/// The bridge script `template` with its tick intervals, player selectors
/// and Info key filled in.
/// What OK on the picture presses instead of pausing, while one shows:
/// Up Next's Play now, or the skip intro/credits button (FR-604).
pub const OK_PRESSES: &str = ".pharos-up-next-play, .pharos-up-next-pill-play, .pharos-skip";

pub fn bridge_script(template: &str) -> String {
    template
        .replace("__BAR_KEYS__", BAR_KEY_JS)
        .replace("__VIDEO_TICK_MS__", &VIDEO_TICK_MS.to_string())
        .replace("__AUDIO_TICK_MS__", &AUDIO_TICK_MS.to_string())
        .replace("__REMOTE_PLAYERS__", REMOTE_PLAYERS)
        .replace("__PLAYER_HOME__", PLAYER_HOME)
        .replace("__REMOTE_SKIPS__", REMOTE_SKIPS)
        .replace("__OK_PRESSES__", OK_PRESSES)
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
    /// The remote's Back at the first page, where the WebView has no page
    /// before to go back to.
    Root,
    /// The viewport's CSS size.
    Size { w: f64, h: f64 },
    /// OK on the picture pressed the shown Skip or Up Next button (FR-604);
    /// `target` is its class.
    #[serde(rename = "okpress")]
    OkPress {
        #[serde(default)]
        target: String,
    },
    /// The remote's Stop key, which the page's script handled: it paused
    /// the player, and left it when it was a video (FR-609).
    Stop {
        #[serde(default)]
        video: bool,
    },
    /// The page was hidden (the app left the screen: Home, another app, the
    /// TV's input switch) or shown again. `paused` counts the videos the
    /// page's script paused as it hid (FR-605).
    Visibility {
        hidden: bool,
        #[serde(default)]
        paused: u32,
    },
    /// A media element's state, after any of its events.
    Media(Media),
    /// An hls.js event on the player for `id`.
    Hls {
        id: String,
        #[serde(flatten)]
        event: HlsEvent,
    },
    /// The page's visualiser run `id`: `started`, `stopped_first`, or the
    /// `render::Fallback` label why it could not draw, with the error.
    Visualiser {
        id: u64,
        reason: String,
        #[serde(default)]
        error: String,
    },
    /// A directly played media element's `error`: its `MediaError` code and
    /// message.
    #[serde(rename = "mediaerror")]
    MediaError {
        id: String,
        #[serde(default)]
        code: u16,
        #[serde(default)]
        message: String,
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

/// The neighbour a remote's next or previous key asks for (FR-609).
pub fn track_skip(key: &str) -> Option<super::watch_view::Skip> {
    match key {
        "MediaTrackNext" => Some(super::watch_view::Skip::Next),
        "MediaTrackPrevious" => Some(super::watch_view::Skip::Previous),
        _ => None,
    }
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
    /// A throttled `timeupdate`, which Rust hears only this way.
    #[serde(default)]
    pub tick: bool,
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
    fn a_tv_event_names_its_client_once() {
        use pharos_client::api::{ClientEvent, ClientEventKind, ClientLevel};
        let event = ClientEvent {
            kind: ClientEventKind::PlaybackError,
            level: ClientLevel::Error,
            route: "/watch/1".to_owned(),
            fields: vec![("engine", "hlsjs".to_owned())],
        };
        let once = tagged(event);
        assert_eq!(
            once.fields,
            vec![
                ("engine", "hlsjs".to_owned()),
                ("client", "android-tv".to_owned())
            ]
        );
        let twice = tagged(once.clone());
        assert_eq!(twice.fields, once.fields, "tagged twice");
    }

    #[test]
    fn ok_presses_the_buttons_the_player_draws() {
        let view = include_str!("watch_view.rs");
        for class in OK_PRESSES.split(", ") {
            let name = class.trim_start_matches('.');
            assert!(
                view.contains(&format!("class: \"{name}\"")),
                "the player draws no {name}"
            );
        }
    }

    #[test]
    fn the_remotes_next_and_previous_keys_skip() {
        use super::super::watch_view::Skip;
        assert_eq!(track_skip("MediaTrackNext"), Some(Skip::Next));
        assert_eq!(track_skip("MediaTrackPrevious"), Some(Skip::Previous));
        assert_eq!(track_skip("ArrowRight"), None);
        assert_eq!(track_skip("MediaFastForward"), None);
    }

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
    fn a_direct_stream_loads_from_the_server_with_the_media_token() {
        let base = "https://media.example";
        assert_eq!(
            direct_media_url(base, "/audio/ab/universal?PlaySessionId=p", "t0k"),
            "https://media.example/audio/ab/universal?PlaySessionId=p&api_key=t0k"
        );
        assert_eq!(
            direct_media_url(base, "/audio/ab/stream", "a+b/c"),
            "https://media.example/audio/ab/stream?api_key=a%2Bb%2Fc"
        );
        assert_eq!(
            direct_media_url(base, "https://other.example/x?y=1", "t"),
            "https://other.example/x?y=1&api_key=t"
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
    fn the_next_media_token_is_asked_for_before_this_one_expires() {
        let six_h = 6 * 60 * 60 * 1000;
        assert_eq!(
            media_token_wait_ms(1_000 + six_h, 1_000),
            six_h - MEDIA_TOKEN_MARGIN_MS
        );
        // Inside the margin, or already expired: soon, but never at once.
        assert_eq!(media_token_wait_ms(2_000, 1_000), MEDIA_TOKEN_MIN_WAIT_MS);
        assert_eq!(media_token_wait_ms(0, 1_000), MEDIA_TOKEN_MIN_WAIT_MS);
    }

    #[test]
    fn a_song_ticks_four_times_as_often_as_a_video() {
        let script = bridge_script(
            "const every = v instanceof HTMLVideoElement ? __VIDEO_TICK_MS__ : __AUDIO_TICK_MS__;",
        );
        assert_eq!(
            script,
            "const every = v instanceof HTMLVideoElement ? 1000 : 250;"
        );
        const { assert!(AUDIO_TICK_MS * 4 <= VIDEO_TICK_MS) };
    }

    /// B1034/V994: the remote moves between the full music screen's
    /// controls as it does the video player's, and lands on its play button;
    /// the music screen in the mini bar is left to the stage bar.
    #[test]
    fn the_remote_moves_round_the_full_music_screen() {
        use crate::native::stage::View;
        let script =
            bridge_script("document.querySelector('__REMOTE_PLAYERS__') ?? '__PLAYER_HOME__'");
        assert!(!script.contains("__"), "{script}");
        let players: Vec<&str> = REMOTE_PLAYERS.split(", ").collect();
        let full = format!(".pharos-stage.is-{} .pharos-listen", View::Full.label());
        assert!(players.contains(&full.as_str()), "{REMOTE_PLAYERS}");
        assert!(players.contains(&"#pharos-player"), "{REMOTE_PLAYERS}");
        for compact in [View::Mini, View::Floating] {
            assert!(!REMOTE_PLAYERS.contains(&format!("is-{}", compact.label())));
        }
        assert!(
            !players.contains(&".pharos-listen"),
            "the compact music screen is the stage bar's"
        );
        assert!(PLAYER_HOME.split(", ").any(|s| s == ".pharos-listen-play"));
        assert!(!bridge_script("__REMOTE_SKIPS__").contains("__"));
        assert_eq!(bridge_script("__OK_PRESSES__"), OK_PRESSES);
        // B1222/V1182: the Info key's script is in the bridge, filled in.
        let keys = bridge_script("__BAR_KEYS__");
        assert!(keys.contains(r#"e.key !== "Info""#), "{keys}");
        assert!(keys.contains(".pharos-stage.is-mini"), "{keys}");
        assert!(keys.contains(".pharos-listen-play"), "{keys}");
        // The bar's play button first: the document's first match was its
        // Previous button (found on the TV, v22).
        assert!(
            keys.find(".pharos-stage-play") < keys.find(".pharos-listen-play"),
            "{keys}"
        );
        assert!(!keys.contains("__PLAYER_HOME__"), "{keys}");
    }

    #[test]
    fn a_page_visualiser_starts_stops_and_skips_a_stopped_run() {
        let start = visualiser_start_js(3, "plasma");
        assert!(
            start.contains(r#"window.__pharosVis.start("plasma","#),
            "{start}"
        );
        assert!(start.contains(r#"if (all[3] === "gone")"#), "{start}");
        assert!(start.contains(r#"t: "visualiser", id: 3"#), "{start}");
        // The page reports a start and a failure itself (B1036, V996).
        assert!(start.contains(r#"send("started")"#), "{start}");
        assert!(start.contains(r#"send("no_webgl2", "#), "{start}");
        assert!(start.contains("await window.__pharosVisLoad"), "{start}");
        let stop = visualiser_stop_js(3);
        assert!(
            stop.contains("run.stop(); run.free(); delete all[3];"),
            "{stop}"
        );
        assert!(stop.contains(r#"all[3] = "gone""#), "{stop}");
        let v = serde_json::json!({"t": "visualiser", "id": 3, "reason": "no_webgl2"});
        assert_eq!(
            parse_bridge(&v),
            Some(Bridge::Visualiser {
                id: 3,
                reason: "no_webgl2".into(),
                error: String::new(),
            })
        );
        let v = serde_json::json!({"t": "visualiser", "id": 3, "reason": "started", "error": ""});
        assert!(
            matches!(parse_bridge(&v), Some(Bridge::Visualiser { reason, .. }) if reason == "started")
        );
    }

    #[test]
    fn the_visualiser_load_script_keeps_its_line_break_escaped() {
        let js = visualiser_load_js("let wasm_bindgen = 1;", "AAEC");
        assert!(
            js.contains(r#"s.textContent = "let wasm_bindgen = 1;" + "\nwindow.__pharosVisInit = wasm_bindgen;";"#),
            "{js}"
        );
        // No string literal in the script spans a line.
        for line in js.lines() {
            assert_eq!(
                line.matches('"').count() % 2,
                0,
                "unbalanced quotes: {line}"
            );
        }
        assert!(js.contains(r#"atob("AAEC")"#), "{js}");
    }

    #[test]
    fn a_lyrics_line_is_centred_in_its_panel() {
        let js = center_child_js("pharos-lyrics", 20);
        assert!(
            js.contains(r#"document.getElementById("pharos-lyrics")"#),
            "{js}"
        );
        assert!(js.contains("panel.children[20]"), "{js}");
        assert!(js.contains("panel.clientHeight / 2"), "{js}");
    }

    #[test]
    fn a_media_error_reads_from_the_bridge() {
        let v = serde_json::json!({"t": "mediaerror", "id": "pharos-video", "code": 4,
            "message": "MEDIA_ELEMENT_ERROR: Format error"});
        assert_eq!(
            parse_bridge(&v),
            Some(Bridge::MediaError {
                id: "pharos-video".into(),
                code: 4,
                message: "MEDIA_ELEMENT_ERROR: Format error".into(),
            })
        );
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
        assert_eq!(
            art_src("https://s", "/Users/2/Images/Primary"),
            "https://s/Users/2/Images/Primary"
        );
        assert_eq!(
            art_src("https://s", "/videos/1/master.m3u8"),
            "/videos/1/master.m3u8"
        );
        assert_eq!(art_src("https://s", "https://x/a.png"), "https://x/a.png");
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
        assert_eq!(
            parse_bridge(&serde_json::json!({"t": "root"})),
            Some(Bridge::Root)
        );
        let pop = serde_json::json!({"t": "pop", "path": "/ui/"});
        assert_eq!(
            parse_bridge(&pop),
            Some(Bridge::Pop {
                path: "/ui/".into()
            })
        );
        assert_eq!(
            parse_bridge(&serde_json::json!({"t": "visibility", "hidden": true, "paused": 1})),
            Some(Bridge::Visibility {
                hidden: true,
                paused: 1
            })
        );
        assert_eq!(
            parse_bridge(&serde_json::json!({"t": "stop", "video": true})),
            Some(Bridge::Stop { video: true })
        );
        assert_eq!(
            parse_bridge(&serde_json::json!({"t": "okpress", "target": "pharos-skip"})),
            Some(Bridge::OkPress {
                target: "pharos-skip".to_owned()
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
        assert!(!m.tick, "only a throttled timeupdate is a tick");
        let mut ticked = v.clone();
        ticked["tick"] = serde_json::json!(true);
        assert!(matches!(
            parse_bridge(&ticked),
            Some(Bridge::Media(Media { tick: true, .. }))
        ));
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
